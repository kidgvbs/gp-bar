# BTM (Background Task Management) instabile su macOS 27.0.1 beta (26A434) — nota di investigate

**Data:** 22 ottobre 2026 · **Ambiente:** macOS 27.0.1 beta build **26A434**, Apple Silicon, solo Command Line Tools (nessun Xcode), firma **ad-hoc** (nessun certificato sviluppatore)
**Contesto:** GP Bar — app menu-bar (Tauri 2) che registra un LaunchDaemon privilegiato via `SMAppService.daemon(plistName:)`. Repo: `~/Rust/gp-bar`, branch `solution-1-smappservice` (worktree `~/Rust/gp-bar-solution1`), commit `0838ab3`.

## Setup funzionante (baseline)

Il meccanismo **funziona** in condizioni pulite, provato end-to-end il 21/10:

1. Bundle in path stabile (`~/Applications/`), firma ad-hoc, **plist del LaunchDaemon dentro il bundle e coperta dalla firma del bundle** (verificato: compare in `_CodeSignature/CodeResources` → `files2`).
2. `SMAppService.registerAndReturnError()` con app in policy **Regular** → **prompt di sistema con Touch ID** → approvazione → `status() == 1 (Enabled)`.
3. launchd avvia il daemon **on-demand** alla prima lookup del Mach service, daemon come **root** (uid 0), XPC risponde. Tutto regolare.

## Le tre anomalie osservate (in ordine cronologico, 21–22/10)

### A. `register` → `EPERM` silenzioso da app Accessory

`SMAppService.registerAndReturnError()` fallisce con **"Operation not permitted"** e **nessun prompt** quando chiamante è un'app **Accessory** (`NSApplication.setActivationPolicy(.accessory)`: niente Dock, mai frontmost). Il prompt di autorizzazione non può essere presentato: non esiste una finestra chiave a cui ancorarlo.

**Fix nostro:** step-up a policy `Regular` per la durata della chiamata, poi ritorno ad Accessory. Funziona (prompt + Touch ID).

**Domanda per la ricerca:** documentato da Apple? È una limitazione nota di SMAppService con app LSUIElement/Accessory, o un difetto della beta?

### B. `register` → `-67054` "Codesigning failure loading plist"

La plist del LaunchDaemon scritta **a runtime** in `Contents/Library/LaunchDaemons/` dopo che il bundle era già firmato viene rifiutata: BTM verifica che la plist sia **coperta dalla firma del bundle** (Sealed Resources) e un file aggiunto dopo la firma non lo è.

```
registering the helper daemon failed: The operation couldn't be completed.
Codesigning failure loading plist: dev.gpbar.helper.plist code: -67054
```

**Fix nostro:** l'app ri-firma il bundle ad-hoc (`codesign --force --sign - --identifier <id>`) subito dopo la scrittura della plist. Funziona. (Con distribuzione firmata Developer ID la plist va spedita a build time e il problema non esiste.)

**Domanda:** il comportamento di BTM qua è corretto/intenzionale; l'unica via ufficiale è la plist nel bundle al momento della firma?

### C. BTM "dimentica" la registrazione → NotFound spontaneo

**Questa è l'anomalia più strana e la meno spiegata.**

Sequenza osservata:
1. Registration riuscita e validata (Enabled, tunnel su, tutto funzionante), ~ore 10:30 del 22/10.
2. Qualche ora dopo, il footer dell'app riporta `helper: not installed`; da codice `SMAppService.status()` torna **`NotFound` (3)**, e la verifica indipendente da riga di comando (`swift -e 'import ServiceManagement; ...'` — quindi **processo diverso, sessione diversa**) conferma: **3**.
3. **Nessun riavvio, nessun logout, nessun unregister, nessun `codesign` sul bundle nel frattempo** (verificato). Il record BTM è sparito da solo.

Nota il codice `3`: in questa SDK `NotFound` indica che BTM non riesce a risolvere **l'app** associata al record — il che suggerisce che il legame record→bundle si è rotto, non che l'utente l'abbia revocato (revoca sarebbe `NotRegistered`).

### D. Il caso peggiore: dopo bootout manuale, il job non riparte più

Sequenza (ore ~12:45 del 22/10):

1. Daemon attivo e funzionante (registrato via BTM, label `dev.gpbar.helper`, `launchctl print system/dev.gpbar.helper` → stato running).
2. Per un test di turnover (sostituire il binario del daemon residente con uno nuovo) eseguo **da root**: `launchctl bootout system/dev.gpbar.helper`.
3. L'app poi chiama di nuovo `SMAppService.registerAndReturnError()` → **OK, senza prompt** (BTM ricorda l'approvazione), e `status()` torna **Enabled**.
4. Ma il daemon **non riparte mai più**: alla lookup del Mach service, niente. Evidenza:
   - `launchctl print system/dev.gpbar.helper` → **`active count = 0`**, `type = Submitted`, `managed_by = com.apple.xpc.ServiceManagement`
   - Il client XPC riceve **"connection invalid/unreachable"**
   - Il log del daemon (StandardOutPath della plist) resta **0 byte**
   - Nessun errore da `launchctl`, nessun log di sistema evidente
5. Un **reboot** della macchina non è stato ancora provato (è l'ovvio prossimo passo, ma il tunnel era di lavoro).

Ogni successiva `unregister`/`register` via `SMAppService` (da codice, stesso risultato con prompt o senza) **non ha rimesso in moto il job**. Il sistema è finito in uno stato ibrido: BTM dice "Enabled", launchd dice "Submitted ma count=0", il daemon non c'è.

## Fatti stratificati sopra (possono rilevare o essere rumore)

- La beta **26A434** ha già un difetto noto e confermato empiricamente (incubo di ottobre, nostro): i processi **root nati fuori da launchd** (authtrampoline/AEWP e — risultato dei test di ieri — anche i **figli di un LaunchDaemon**, non solo figli diretti di launchd) sono **sordi ai segnali**: `kill -TERM/INT/HUP` ritorna 0, l'handler non gira mai. Solo i figli **diretti** di launchd sono signalable. Ci ha spinto a ridisegnare il daemon (worker via job launchd one-shot). Vedi `RECAP.md` §10-11 nel repo.
- Tutto il test è su **firma ad-hoc**: il Designated Requirement è debolissimo, BTM non ha team identity a cui ancorare il record. Plausibile che parte di questa fragilità peggiori senza una firma vera (abbiamo solo CLT, nessun Apple Developer certificato).
- Il registro BTM è visibile come `/private/var/db/launchd.db/com.apple.launchd/…` + il database `smd`/`btm` di sistema: non abbiamo ancora provato a guardare **dove** finisce il record (file di dump, `btm dump`?).
- I tempi di alcuni flip (A→C) sembrano potersi legare a **ri-scansioni di LaunchServices/BTM**: la mezz'ora di registro "perso" include i nostri spostamenti/cancellazioni del bundle S1 (`rm -rf` + `cp -R`). Non è impossibile che BTM abbia "reindicizzato" e il record sia stato staccato dal bundle (che nel frattempo era ricreato con firma/stessa identità).

## Cosa chiedere / cercare

1. **Conferme del bug C/D su 27.0.1 (26A434)** o su altre beta di macOS 27: report su forum (Apple Developer Forums, Reddit r/MacOS beta, MacRumors), issue su progetti noti che usano SMAppService (es. [`SMAppService`](https://developer.apple.com/documentation/servicemanagement/smappservice) — tool open-source che ne fanno uso: fare grep su GitHub di `daemonServiceWithPlistName`/`daemon(plistName:)` e vedere se qualcuno ha issue tipo "daemon doesn't start after bootout" / "SMAppService forgets registration".
2. **Comportamento atteso di BTM**: esiste una via supportata per forzare il turnover del binario di un LaunchDaemon registrato via SMAppService quando il bundle è firmato ad-hoc e vive in `~/Applications`? La documentazione Apple parla esplicitamente di "the daemon binary must live in a stable location"; con bundle user-writable e firma ad-hoc, il re-registration è stabile?
3. **Debug di sistema**: c'è un modo ufficiale per ispezionare lo stato BTM da CLI? (`sfltool`/`btm`? Su questa beta `btm` esiste? — ho visto riferimenti a `/System/Library/PrivateFrameworks/BackgroundTaskManagement.framework` ma non ho ancora provato a caricare/dumpare). Se esiste un dump, possiamo confrontare il record prima/dopo il flip NotFound.
4. **Riferimenti noti**: issue di altri progetti Tauri/Electron con `SMAppService` che documentino comportamento simile (Finder Sync / Login Items API, la parte "Allow in background", è la stessa infrastruttura BTM: vale la pena cercare anche "Login Items not persisted macOS 27 beta").
5. **Un test pulito da fare al collega** (5 minuti, su una macchina con la stessa beta): registrare un LaunchDaemon minimale via `SMAppService` da un'app firmata ad-hoc in `~/Applications`, aspettare qualche ora (o forzare `launchctl bootout` + re-register), vedere se il job riparte. Se su macchina pulita non riproduce, il problema è legato ai nostri ri-registrazioni multiple / spostamenti di bundle della giornata.

## Stato attuale (per contesto)

- **Produzione = Soluzione 3** (sudoers NOPASSWD + wrapper root-owned): semplice, nessuna di queste anomalie, in uso e verde. Il BTM/SMAppService (Soluzione 1) è parcheggiato sul branch `solution-1-smappservice`, in attesa di capire se C/D sono riproducibili o dovuti alla beta.
- Questa nota è anche una richiesta di confronto: se hai altri strumenti che usano `SMAppService`/Login Items su questa beta e vedi flip o rifiuti inspiegabili, fatti sentire.
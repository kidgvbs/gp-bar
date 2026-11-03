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

---

## Aggiornamento 2 novembre 2026 (macOS 27.2, 26B123) — il NotFound della sonda era uno zombie BTM di ottobre

Fase 0 della ripresa della soluzione 1: sonda SMProbe v2 (identità
`dev.gpbar.smprobe`, bundle minimale ad-hoc, installata in
`~/Applications`; codice in `smprobe/` sul branch
`solution-1-smappservice`). Il Passo 3 (registrazione) è fallito con
esattamente il paio di sintomi dell'issue exelban/stats #3716 — che è
su macOS 27.0 **release** (26A428), non beta:

- `SMAppService.status()` → **`NotFound` (3)** persistente, anche a
  LaunchServices assestato (`lsregister -dump` vedeva correttamente
  app, identifier e plist come library item);
- `registerAndReturnError()` → **EPERM "Operation not permitted"**,
  nessun prompt — da Terminale e persino dal sampler lanciato via
  `open`.

### Causa trovata: la registrazione di ottobre non fu mai cancellata

`sfltool dumpbtm` mostrava **due record per la stessa identità
`dev.gpbar.smprobe`**: lo zombie di ottobre — `[disabled, allowed,
notified]`, l'approvazione della v1 del probe — e la scansione fresca
del 2/11 — `[pending authorization, not notified]`, con l'item demone
`16.dev.gpbar.smprobe.helper` `[enabled, disallowed, not notified]`.
Il cleanup del 22/10 cancellò i **bundle** ("cartelle ripulite", RECAP
§12.4) ma non eseguì mai `unregister`: il record sopravvisse a due
upgrade di OS. Il fantasma rende ambigua l'identità → SMAppService non
risolve più il servizio → `NotFound`; il passo di autorizzazione viene
rifiutato → EPERM. È il desync record↔bundle dell'issue stats #3716
(lì innescato da auto-update: "parent bundle version = 846 ← app is
now 851"; qui da elimina-senza-unregister + reinstall con la stessa
identità).

Questo **rafforza il sospetto già scritto in "Fatti stratificati"**:
l'anomalia C (NotFound spontaneo del 22/10) è plausibilmente la stessa
famiglia di desync, innescata dai nostri `rm -rf` + `cp -R` del
bundle durante il turnover del demone — non un flip a caso del
database.

### Due scoperte semantiche (2/11)

1. **`register` EPERM ≠ registrazione rifiutata.** L'EPERM copre solo
   la presentazione del prompt: la registrazione **atterra comunque**
   in `RequiresApproval`. Evidenza: subito dopo ogni register EPERMato
   (Terminale ~17:56Z, sampler 18:04:26Z) lo status era 2. Un binario
   nudo non sembra poter presentare il prompt nemmeno via `open`
   (il prompt del 21/10 arrivava dall'app Tauri con step-up Regular,
   fix `999df9a`): per la sonda l'approvazione va completata a mano
   via toggle in Impostazioni; per l'app reale (Tauri) vale la via
   prompt.
2. **`RequiresApproval` si completa a mano in Impostazioni.** Fatta la
   registrazione pulita (EPERM al prompt ma registrazione atterrata),
   lo stato era `RequiresApproval` (2): l'utente ha approvato **a
   mano** il toggle "Consenti in background" (Impostazioni di Sistema
   → Elementi di login, voce SMProbe) → `status` → `Enabled` e launchd
   ha avviato il demone on-demand come root al primo ping
   (`ok uid=0`).
   [Corretto il 3/11: l'approvazione NON è per-record come prima
   ipotizzato — il toggle "Consenti in background" **sopravvive
   all'unregister** e le registrazioni successive lo ereditano: una
   unregister→register del 3/11 è andata dritta a `Enabled` senza
   ri-approvazione e con `register: ok` (nessun EPERM: nessun prompt
   necessario). Il 2/11 servì il toggle manuale perché la voce era
   nata spenta con la scansione fresca, non perché l'approvazione non
   possa persistere.]

### Riparazione validata (funziona anche dallo stato rotto, senza prompt)

```sh
smprobe unregister      # SMAppService.unregister: ok anche con status=3, da Terminale
smprobe status          # → 0 (NotRegistered): il fantasma è andato
open ~/Applications/SMProbe.app   # il sampler ri-registra (EPERM al prompt: atteso); poi
                                 # approva a mano: Impostazioni → Elementi di login → SMProbe ON
```

### Conseguenze per la soluzione 1 reale

1. **Requisito di design (Fase 3, registrazione/UX):** l'app deve
   rilevare `NotFound`/EPERM dopo un cambio di bundle e proporre (o
   eseguire) unregister→register come **recovery**, con messaggio
   chiaro — e guidare l'utente all'approvazione (prompt Touch ID se
   l'app lo presenta, altrimenti il toggle in Impostazioni). La classe di guasto esiste su macOS 27.0 release con app
   firmate e distribuite — non è roba da beta.
2. Il test C/D della sonda su 27.2 è ripartito alle 18:04Z del 2/11
   con una registrazione sana: endurance → reboot → bootout.

---

## Aggiornamento 3 novembre 2026 — Fase 0 chiusa: D è riproducibile su 27.2, ma riparabile con una chiamata

### Passo 4 (endurance, anomalia C): NON riprodotta

47 campioni su 47 verdi (2/11 18:09Z → 3/11 08:53Z); il `grep -v`
restituisce solo le righe dell'era-zombie. Nota onesta: il Mac ha
dormito quasi tutta la notte (~4 ore di campionamento effettivo), ma
ogni campione ha comunque costretto launchd a uno spawn fresco del
demone (pid sempre diversi) e nessun flip è mai comparso. Zero
`NotFound` post-registrazione.

### Passo 5 (reboot): PASS

Dopo il riavvio, senza aprire l'app: `status` → `1 (Enabled)`, `ping`
→ `ok uid=0` con pid nato dopo il boot. BTM ha re-bootstrappato il
job all'avvio: registrazione e approvazione sopravvivono al boot su
27.2. Era la domanda lasciata aperta il 22/10.

### Passo 6 (bootout, anomalia D): RIPRODOTTA — ma con il recovery che a ottobre non c'era

Baseline pre-bootout: `launchctl print` mostra il job (`Submitted`,
`managed_by = com.apple.xpc.ServiceManagement`) e il legame esplicito
`BTM uuid = 951890E3…` verso il record del demone nel dumpbtm. Poi:

1. `sudo launchctl bootout system/dev.gpbar.smprobe.helper` → tutti e
   sei i ping (3 min) → unreachable; `launchctl print` → **"Could not
   find service"** (il job è sparito da launchd, non solo count=0);
   `status` resta `1 (Enabled)`; il record BTM del demone resta
   `[enabled, allowed]` (Generation 105, "Last Use" pre-bootout —
   BTM seguiva l'attività del demone a ogni spawn). Stato ibrido
   BTM-sì/launchd-no, nessun re-bootstrap spontaneo. **D confermata
   anche su 27.2.**
2. **Recovery scoperto: basta una `register()`.** Da Terminale:
   `register` → **`register: ok — status=1 (Enabled)`** (approvazione
   già attiva → nessun prompt → nessun EPERM) e al ping successivo il
   demone è vivo con pid nuovo: smd ha re-sottomesso il job. Il
   sampler non lo aveva fatto perché registra solo con `status==0` —
   la D si presenta come `Enabled`, quindi il retry non scatta.
   **Lezione di design: per l'app reale il retry va innescato da
   "unreachable", non da `status==0`.**
3. Secondo esperimento (unregister → register dal sampler): dritta a
   `Enabled` **senza ri-approvazione** — il toggle "Consenti in
   background" è rimasto attivo attraverso l'unregister (correzione
   sopra) — con `register=ok` nel log; cold restart confermati (ping
   ogni >60 s → pid sempre diversi; print: `state = running`).

**Differenza chiave vs 26A434**: lì il re-register dopo il bootout
non rimetteva MAI in moto il job ("nessun unregister/register
successivo ha sanato", §D di questa nota). Su 27.2 la stessa chiamata
lo risana al primo colpo. D passa da muro a guasto riparabile con
una chiamata.

### Esito Fase 0 e requisiti per la soluzione 1

- **C: non riprodotta** su 27.2 (endurance + reboot).
- **D: riprodotta ma riparabile** → requisito **R1**: l'app fa
  self-healing (helper unreachable + `status==Enabled` → richiama
  `register()` → job torna; nessuna interazione utente).
- **Zombie (2/11)** → requisito **R2**: dopo cambio bundle, se
  `NotFound`/EPERM → unregister→register con eventuale guida
  all'approvazione.
- **Prima installazione** → requisito **R3**: register dalla UI
  dell'app (prompt Touch ID, politica Regular, fix `999df9a`) con
  fallback documentato al toggle in Impostazioni.
- **GO per la Fase 1** (trasporto XPC ricostruito da main), con la
  soluzione 3 in produzione come fallback perenne.
- Curiosità forense: il record zombie dell'app (UUID 6508D9DE,
  ottobre) persiste nel dumpbtm anche dopo i register/unregister di
  questi giorni — innocuo finora, documentato.

## Aggiornamento 3 novembre 2026 (sera) — Passo 7: cleanup post-Fase 0, e una terza conferma di R2

Sonda ritirata. `smprobe unregister` → **`status=0 (NotRegistered)`**;
job booted out, demone terminato, bundle `~/Applications/SMProbe.app`
rimosso. Un processo residuo è sopravvissuto al primo giro di pulizia: la
CLI `smprobe` (partita alle 10:53, orfana su launchd) — `pkill -f
smprobe-helper` non la matchava perché il nome è un altro; uccisa a mano
(PID 13122). Lezione da tenere: `pkill -f` per nome esatto, o per
percorso del bundle.

Il dump post-cleanup (`~/4-after-cleanup.txt`) conferma le attese e
aggiunge un dato:

- lo **zombie 6508D9DE** persiste dopo il cleanup, come previsto
  (Generation 1, bundle inesistente, `[disabled, allowed, notified]`) —
  inerte e innocuo;
- i record **v2** dei test (app #2 + helper #16, `Last Use` = il bootout
  di oggi) permangono come **`[disabled, allowed, not notified]`** — la
  registrazione è sparita ma **il bit "allowed" dell'approvazione
  sopravvive all'unregister**.

La seconda osservazione è la **terza conferma indipendente** del
comportamento che fonda R2 (le prime due: theevilbit, e la register del
3/11 atterrata dritta a `Enabled` senza ri-approvazione) — e stavolta
osservata nel flusso di *cleanup*, non di re-register: il database BTM
**non torna pulito** dopo unregister, i record restano lì disabilitati
con il consenso conservato, pronti perché una `register()` futura li
riattivi senza ri-approvazione. Il "reinstall mechanism"
unregister→register è quindi ancora più economico del previsto: il
consenso dell'utente è un asset persistente, non qualcosa da ri-chiedere.
# Prior art — app macOS con helper root: SMAppService LaunchDaemon + XPC

> Ricerca del 3/11/2026, fatta alla partenza della **Fase 1** della
> soluzione 1 su richiesta dell'utente: *"documentati in giro su come si
> comportano le altre app su MacOS che hanno lo stesso funzionamento che
> vogliamo implementare noi, così da poterne prendere spunto"*.
>
> Il pattern studiato: **app utente che registra via `SMAppService` un
> LaunchDaemon embedded nel proprio bundle, approvato dall'utente in
> Impostazioni, e gli parla via XPC (mach service) per le operazioni
> privilegiate**. È la via Apple da macOS 13 (erede di SMJobBless).
>
> Le scoperte empiriche nostre (Fase 0) sono in `docs/btm-27.0.1-beta.md`
> (aggiornamenti 2–3/11) e RECAP §20; i requisiti di design R1–R3 e la
> mappa B1–B11 della review sono in `gp-bar-review-smappservice.md`.
> Fonti primarie in coda.

## 1. exelban/stats — il caso più vicino al nostro (open source, Swift)

Stats (monitor menu-bar) espone un helper SMC/fan privilegiato registrato
via `SMAppService.daemon(plistName:)` — migrato dalle vecchie API per via
delle rotture su macOS 26.x (#3193). L'issue [#3716]
(28/10/2026, **macOS 27.0 release**, 26A428, non beta) descrive esattamente
la nostra classe zombie:

- auto-update in-place dell'app (build 846 → 851): il record BTM resta
  ancorato al **bundle-846** (`parent bundle version = 846 <- app is now
  851` nel `launchctl print`);
- `SMAppService.status` → `.notFound` e `register()` → **EPERM**,
  *mentre launchd ha il demone registrato e running*;
- il popup dell'app continua a offrire "Install fan helper"; l'errore UI
  è consapevole dell'approvazione: *"Could not enable fan helper… make
  sure Stats is allowed in the background"*;
- il log utente mostra il flusso interno di recovery:
  `failed to register SMC helper daemon: Operation not permitted` e
  `failed to register SMC helper daemon after reset: Operation not
  permitted` — quindi il codice già prevede un **tentativo di reset**
  (unregister) tra i due register;
- l'autore non riesce a riprodurre e risponde spedendo una build con un
  **"mechanism that should allow to reinstall the SMC properly"** —
  il reinstall come feature di prodotto, non come istruzione manuale.

**Cosa ne prendiamo.** La conferma più importante è di classe: il record
zombie non è una nostra anomalia né una cosa da beta — colpisce l'app
open-source più diffusa della categoria su una release. Il settore
risponde con (a) installazione guidata dall'app, (b) messaggio d'errore
che punta all'approvazione in Impostazioni, (c) **uninstall→register
come "reinstall mechanism"** — esattamente il nostro R2/R3. Nota di
onestà: il sorgente non è stato ispezionato (GitHub code search richiede
login); la lettura si basa su issue, log utente e risposta dell'autore.

## 2. SecureXPC (trilemma-dev) — il pattern di riferimento del trasporto

Libreria Swift per XPC con supporto dedicato ai daemon `SMAppService`
(`XPCServer.forMachService()`, `XPCClient.forMachService(named:
withServerRequirement:)`). Punti di design che ci riguardano:

- **Route/protocollo in un file condiviso** tra client e server — lo
  stesso istinto del nostro crate proto (review B11: lib condivisa, non
  `#[path]`);
- **Reply tipizzati** (Codable), non dizionari dinamici — come la review
  chiede per `Reply` (B11) al posto di `serde_json::Value`;
- **Sicurezza**: "i mach service sono per default accessibili a
  qualunque processo non sandboxato" e la storia di exploit XPC è
  lunga; SecureXPC valida il peer **via audit token** ("Don't Trust the
  PID") e deriva automaticamente l'identità attesa per i servizi
  SMAppService; il client può *anche* pinnare il server via code-signing
  requirement (validazione mutua);
- Implica la nostra gradinata: **euid subito** (parity con la regola
  sudoers), requirement legato al Team ID appena esisterà un'identità
  (hook B2 futuro). Swift-only, ma il pattern è neutro rispetto al
  linguaggio.

## 3. Docker Desktop — on-demand e recovery a livello industriale

La cronologia documentata di `com.docker.vmnetd` è una mini-storia del
pattern:

- **< 4.15**: helper privilegiato *permanente*, installato al primo run
  con prompt admin;
- **≥ 4.15**: *"Docker Desktop for Mac does not require the user to run
  a permanent privileged process. Whenever elevated privileges are
  needed, Docker Desktop prompts the user with information on the task
  it needs to perform"* — on-demand + **prompt al momento del bisogno,
  con la spiegazione del task**;
- **≥ 4.97**: privileged ports gestiti direttamente, l'helper non serve
  più nella configurazione di default;
- il CLI di installazione (`install --user <utente>`) fa, tra le altre
  cose, *"Uninstalls the previous `com.docker.vmnetd` if present"* —
  il reinstall meccanismo, e la limitazione dichiarata è **un solo
  utente per macchina** (il peer è l'utente che ha installato: la stessa
  semplifica del nostro check euid == owner del bundle);
- comunicano via **Unix domain socket** (`/var/run/com.docker.vmnetd.sock`),
  non via mach service XPC; noi restiamo su XPC, che è la via indicata
  da Apple e percorsa da SecureXPC/stats;
- l'app rifiuta di girare come root (parallelo delle nostre lezioni
  SUDO_UID/`stat -f %u /dev/console`).

## 4. Apple — linee ufficiali, e una novità macOS 26+ che non conoscevamo

Dalla guida di deployment ufficiale (*Manage login items and background
tasks on Mac*) e dalle doc SMAppService:

- gli helper vivono **dentro il bundle** (`Contents/Library/LaunchDaemons`,
  path relativi nel record BTM) e SMAppService *"works with all types of
  apps, regardless of how they were installed"*;
- `SMAppService.openSystemSettingsLoginItems()` apre **direttamente il
  pannello giusto** di Impostazioni — il primitive per la guida
  all'approvazione del nostro R3 (meglio che istruzioni a mano);
- `sfltool dumpbtm`/`resetbtm` sono documentati come strumenti **per
  admin/UMAN, "between tests" + riavvio** — conferma che il reset del DB
  non è uno strumento di prodotto;
- un profilo MDM (ServiceManagement) può pre-approvare/silenziare le
  notifiche — fuori dal nostro caso personale, ma descrive il modello
  di autorizzazione;
- **novità macOS 26+**: *"if background tasks are started by an app and
  remain active after a user quits the app, the user is prompted to
  allow or not allow the background tasks to remain active"*. Per noi:
  il demone on-demand a idle-exit 60s minimizza la finestra in cui
  l'app può uscire col demone vivo; in Fase 3 va prevista la
  comunicazione di questo possibile prompt di sistema.

## 5. theevilbit (Csaba Fitzl) — la semantica BTM confermata in autonomia

Dal suo walkthrough SMAppService (settembre 2023, con Team ID reale):

- i quattro stati coincidono con i nostri (NotRegistered/Enabled/
  RequiresApproval/NotFound);
- la voce si sposta ma resta relativa al bundle; **"if the daemon was
  approved once, unregistering and registering it again doesn't require
  further authentication"** — conferma indipendente della scoperta del
  nostro 3/11 (il toggle sopravvive all'unregister);
- disabilitare da Impostazioni porta il record a `[enabled, disallowed]`
  e il job **sparisce da launchd**;
- bug segnalato su 13.6 (FB13206906): il disable da Impostazioni *non*
  disabilitava il job launchd — la sincronizzazione BTM↔launchd è
  storicamente fragile: è il motivo esistente del nostro R1 (il
  self-healing non può fidarsi di un solo lato);
- la persistenza ora vive nei bundle: tool di sicurezza e utenti devono
  guardare lì.

## 6. Bryson Tyrrell — il pattern end-to-end, con l'UX completa

Prototipo documentato (aprile 2025) dell'intero flusso, utile come
"spec esecutiva" della Fase 3:

- è **l'app a chiamare `register()`** — un demone/agente non può
  auto-registrarsi; la UI distingue due stati, "registered" e
  "approved", e su `requiresApproval` chiama
  `openSystemSettingsLoginItems()`;
- plist embedded con `MachServices`, `BundleProgram` relativo e
  *"Code Sign On Copy"*; il suo `RunAtLoad=true` è però il contrario
  della nostra scelta (e di quella Docker): noi on-demand, nessun
  KeepAlive (review B1);
- lato server: `setCodeSigningRequirement("identifier … and anchor
  apple generic and certificate leaf[subject.CN] = …")` — la forma
  esatta del nostro hook B2 futuro;
  *"as a daemon running with root permissions you should never allow
  just any process to call it"*;
- il predicate di log dell'Apple engineer per debuggare registrazione:
  `process in { '<app>', 'smd', 'backgroundtaskmanagementd' }`.

## 7. Howard Oakley — la realtà del database BTM

Dalla sua guida a Login/Background Items (dicembre 2025) e dai commenti:

- i **record fantasma** sono normalità: il sistema *"dovrebbe"* pulire
  durante la manutenzione notturna ma *"doesn't appear to be cleaning
  up as well as it might"* — e *"There's currently no way to change or
  correct those"* con gli strumenti a disposizione;
- `dumpbtm` **prima** di ogni reset (è la fotografia di ciò che c'è da
  ripristinare); `resetbtm` è nucleare, va seguito da riavvio, e *"if
  you don't then delete old apps and their components, many will
  return to haunt you"*.

**Conclusione per noi:** il DB sporco è una condizione del sistema, non
un nostro errore; le app serie portano il recovery **dentro il prodotto**
(stats, Docker — e noi: R2), mentre `resetbtm` resta l'ultima spiaggia
umana documentata, mai invocata dall'app.

## Tabella riassuntiva — cosa adottiamo

| Pratica (chi la usa) | Dove va nella soluzione 1 |
|---|---|
| Demone on-demand, niente KeepAlive/terminate_self (Docker ≥4.15, review B1, sonda SMProbe) | **Fase 1** (plist + skeleton helper) |
| Protocollo in lib condivisa, reply tipizzate (SecureXPC, review B11) | **Fase 1** |
| Peer via euid owner-of-bundle, fail closed; audit token → CS requirement dopo (SecureXPC, theevilbit, B2) | **Fase 1** (euid), hook futuro |
| Trasporto XPC mach service, non socket unix (Apple, SecureXPC — Docker fa l'eccezione) | **Fase 1** |
| Classe d'errore "unreachable" distinta (gancio del self-healing) | **Fase 1** (tassonomia errori) |
| `register()` dall'app UI, mai dal demone (dev.to, Apple) | Fase 3 (R3) |
| `openSystemSettingsLoginItems()` come guida all'approvazione (dev.to, Apple) | Fase 3 (R3) |
| Prompt al momento del bisogno, con spiegazione del task (Docker ≥4.15) | Fase 3 (R3) |
| Errore UI consapevole dell'approvazione (stats: "make sure X is allowed in the background") | Fase 3 (R3) |
| unregister→register come "reinstall mechanism" (stats, Docker `install --user`, Fase 0) | Fase 3 (R2) |
| `register()` da sola risana l'anomalia D (Fase 0 — scoperta nostra, non nel settore) | Fase 3 (R1) |
| Attenzione al nuovo prompt macOS 26+ " lasciare i task attivi?" a app-exit (Apple) | Fase 3 (comunicazione) |
| `resetbtm` mai nel prodotto; doc di debugging per umani (Apple, Oakley) | documentazione |
| Log predicate smd/backgroundtaskmanagementd (Apple engineer via dev.to) | documentazione |

## Non adottiamo

- `RunAtLoad=true` (dev.to): on-demand, come Docker e come la sonda;
- Unix domain socket (Docker): restiamo su XPC mach service;
- profili MDM (Apple): fuori dal caso d'uso personale;
- qualunque forma di `sfltool` invocato dall'app.

## Reference interno (nostra, già validata sul campo)

La sonda SMProbe (`smprobe/` sul branch `solution-1-smappservice`) è già
prior art meccanica della Fase 1: `xpc.rs` è il layer RAII con B7/B8/B9/B10
applicati (doc-comment: *"the layer the real Fase-2 daemon will reuse"*),
`peer.rs` la policy euid==owner fail-closed con test, `client.rs` il client
con reply async + timeout. La Fase 1 la porta dentro l'app crate con le
identità `dev.gpbar.*`, senza ripartire dal codice parked `fd2c1fb` (che
di quei difetti soffriva).

## Fonti primarie

- exelban/stats issue #3716 (28/10/2026): "Fan helper: SMAppService status
  .notFound and register() EPERM on macOS 27.0 release after update" —
  https://github.com/exelban/stats/issues/3716 (e i collegati #3419,
  #3606, #3193)
- SecureXPC (trilemma-dev): README + FAQ —
  https://github.com/trilemma-dev/SecureXPC
- Docker Desktop, "Understand permission requirements for Docker Desktop
  on Mac" — https://docs.docker.com/desktop/setup/install/mac-permission-requirements/
- dazwallace, "Changes to Docker Desktop for Mac" (12/2022, la transizione
  4.15) — https://dazwallace.wordpress.com/2022/12/02/changes-to-docker-desktop-for-mac/
- Apple, "Manage login items and background tasks on Mac" (Platform
  Deployment) — https://support.apple.com/en-gu/guide/deployment/depdca572563/web
- theevilbit (Csaba Fitzl), "macOS Service Management — The SMAppService
  API" (28/9/2023) — https://theevilbit.github.io/posts/smappservice/
- Bryson Tyrrell, "macOS Apps With Embedded Daemons" (28/4/2025) —
  https://dev.to/brysontyrrell/macos-apps-with-embedded-daemons-333a
- Howard Oakley, "Manage Login and Background items" (3/12/2025) —
  https://eclecticlight.co/2025/12/03/manage-login-and-background-items/

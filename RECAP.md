# Recap — Nuovo client GUI per la VPN aziendale (GP Bar)

**Data:** 20 ottobre 2026
**Richiesta:** riscrivere un client GUI come GP Connect.app ma **senza il limite di 1 ora** sulla durata della connessione VPN, usando REA per l'analisi.
**Esito:** ✅ `GP Bar.app` costruito, avviato e in esecuzione nella menu bar. Nessun limite di tempo. ⚠️ **In sospeso:** su macOS 27.0.1 beta il click sul tray non risponde — diario completo del debug e piano di escalation in **§8**.

---

## 1. Analisi REA del client esistente (`GP Connect.app`)

### Identificazione del target
- App: `/Applications/GP Connect.app` (v3.1.0, bundle id `com.yuezk.gpgui`, Tauri + Rust, arm64, 7.1 MB)
- Componenti nel bundle: `gpgui` (GUI), `gpclient` (CLI), `gpservice`, `GPServiceBroker` (demone root via LaunchDaemon `com.yuezk.gpgui.broker`), `gpauth`
- Stack del progetto pubblico: **`yuezk/GlobalProtect-openconnect`** — CLI `gpclient` **GPL-3.0 gratis senza limiti**, GUI "GP Connect" **proprietaria a pagamento** (trial 7 giorni, poi limite di durata connessione)

### Limiti dell'analisi
- L'import Ghidra del binario Rust (7 MB, decine di migliaia di funzioni) ha sforato il timeout del provider REA (330s, `REA_GHIDRA_STARTUP_TIMEOUT_MS` non modificabile a runtime). Sessione REA chiusa correttamente.
- Complemento: passata di stringhe sul binario (shell) + studio del sorgente pubblico (le parti open source sono la quasi totalità della logica di connessione). Il decompile completo non era necessario per l'obiettivo ingegneristico.

### Evidenze chiave (binario `gpgui`, SHA-256 `1f0d4988cdb135ca…`)

**Il limite di 1 ora è artificiale, lato GUI, non di backend:**

| Evidenza | Significato |
|---|---|
| Moduli `gpgui::license::{evaluation, lease, trial, license_manager, authorization}` | Sistema di "Evaluation Lease" nel solo processo GUI |
| `"Connection authorization ended, disconnecting…"` | La GUI **disconnette attivamente** quando la lease scade |
| `"Session extension is unavailable while license verification is pending"`, `"Session Expiring"`, `showSessionWarning` | Avvisi/negazioni di estensione lato client |
| `"The Trial Grant does not currently authorize a connection"`, `gp-license-lease:v1` | Gate di licenza prima ancora del tunnel |
| `"GPGUI_DEBUG_SESSION_WARNING_DELAY_SECS"` | Il warning è schedulato dalla GUI stessa |

**Architettura di orchestrazione (per interoperabilità):**
- La GUI usa `gpapi` (crate **MIT**) per l'autenticazione portale, poi delega il tunnel a un servizio locale via **WebSocket** (`gpgui::ws_connector`, lock `/var/run/gpservice.lock` formato `pid:port` + health HTTP) — per questo GP Connect non chiede password a ogni azione
- Browser aperto dal processo root via `/usr/bin/open` (funziona in sessione GUI anche sotto `sudo`)
- Tray + popover macOS via `tauri-nspanel`

**Conclusione:** il CLI (`gpclient`) che già uso da terminale non ha alcun limite; il limite esiste solo nel codice proprietario della GUI. Un client GUI nuovo che orchestra il CLI open source è **legalmente e tecnicamente pulito** — niente crack di GP Connect, niente copia di codice o asset proprietari.

---

## 2. Studio del sorgente open source (repo pubblica, clone in `reference/`)

Fatti estratti (tutti verificati nel codice, non dedotti):

- **Formato log JSON** (`crates/gpapi/src/log_format.rs`): una riga per record → `{"timestamp": RFC3339, "level": "INFO", "target": "gpclient::…", "message": "…"}`
- **PID lock file** (`apps/gpclient/src/connect/gateway.rs:671`): `gpclient` scrive il suo PID in `/var/run/gpclient.lock` (world-readable) quando il tunnel è attivo e **lo rimuove alla disconnessione** → segnale di stato affidabile da processo utente
- **`--auto-gateway`**: seleziona i gateway in ordine di priorità senza prompt interattivi (essenziale per un wrapper non-TTY)
- **`--cookie-cache`**: riusa il cookie di sessione del portale (root: cache in `/var/root/.config/gpclient/cookie.json`) → meno ri-autenticazioni browser
- **`--background`**: sign-in in foreground, poi detach con log interno 0600 root-owned → **scartato** in favore del redirect manuale (vedi §3)
- **`gpclient disconnect`**: teardown grazioso via segnale al PID del lock file
- **Browser launcher** (`crates/browser-launcher/src/platform/macos.rs`): usa `/usr/bin/open`/`webbrowser` → l'SSO funziona anche da processo root lanciato da osascript
- **gpservice (GPL-3.0)**: protocollo WebSocket + credenziale per utente (`credential_lease`) → modello di riferimento per la v2

---

## 3. Decisioni di design (confermate dall'utente)

| Decisione | Scelta | Motivo |
|---|---|---|
| Linguaggio | **Rust + Tauri 2** | `gpapi` è MIT/Rust, stesso stack del dominio, un solo linguaggio/build |
| Privilegi | **v1: prompt osascript; v2: demone** ("Entrambi") | v1 subito funzionante, v2 elimina i prompt |
| GUI | **Menu bar / tray con popover** | come GP Connect, leggera, sempre disponibile |
| Backend VPN | **orchestra `gpclient` via exec** (non linking) | zero ri-implementazioni, zero limiti, aggiornabile via brew |

**Trucco chiave del flusso connect (v1):**
`osascript "do shell script '… > log 2>&1 & echo $!' with administrator privileges"` →
- il redirect fa sì che `do shell script` ritorni subito (canone noto di AppleScript) mentre il worker root resta vivo, reparentato a launchd
- `umask 022` rende il log JSON **leggibile dall'utente** → streaming live nel pannello senza `--background` né chown
- `$!` restituisce il PID del worker → cancel/kill mirati

---

## 4. Implementazione — `~/Rust/gp-bar`

```
gp-bar/
├── index.html, src/main.ts, src/panel.css      # pannello (vanilla TS, no framework)
├── scripts/gen-app-icon.mjs                     # icona .icns generata via codice (PNG encoder a mano + iconutil)
├── src-tauri/
│   ├── src/main.rs                              # setup Tauri, tray+menu, finestra popover, comandi IPC
│   ├── src/vpn.rs                               # macchina a stati, poller lock file, tailer log JSON, script builder
│   ├── src/config.rs                            # portali/porte persistiti (default: vpn.example.com)
│   ├── src/osascript.rs                         # wrapper admin + escaping shell/AppleScript (testato)
│   └── src/icons.rs                             # icone tray template disegnate via SDF (22×22, antialiasing 4×)
└── reference/GlobalProtect-openconnect/        # clone shallow del sorgente open source (gitignored)
```

Comandi IPC: `get_state`, `get_config`, `get_versions`, `add/remove/select_portal`, `set_reuse_cookie`, `connect`, `disconnect`, `cancel`.

Stati: `Disconnected → Connecting → Connected → Disconnecting → Disconnected/Error` con rilevamento anche di VPN avviate fuori dall'app (lock file esistente).

---

## 5. Build, test e stato attuale

- ✅ `cargo test`: **7/7** (escaping shell/AppleScript, script builder, parsing gateway, icone)
- ✅ Build release (`npm run tauri build`): `src-tauri/target/release/bundle/macos/GP Bar.app` (6.9 MiB, arm64, ad-hoc signed)
- ✅ Smoke test: app avviata via `open`, processo stabile, tray visibile (lucchetto vuoto = disconnesso)
- ⏳ **Da collaudare a mano** (serve password admin + SSO nel browser): flusso connect completo, posizionamento popover sotto l'icona, connessione oltre le 2 ore per confermare l'assenza di limiti
- Git: repo inizializzato, 2 commit (`6e77964`, `de51d55`)

Note operative: 1 prompt admin per connect, 1 per disconnect (come `sudo` da terminale). Il VPN sopravvive al quit/riavvio dell'app.

---

## 6. Roadmap v2 (da concordare)

**Demone `gp-bard`** per azioni senza prompt:
- helper nel bundle registrato via `SMAppService` (macOS 13+), attivazione con password admin **una sola volta**
- socket Unix `/var/run/gp-bard.sock` + protocollo JSON (connect/disconnect/status + stream log)
- ~200 righe di Rust, senza adottare l'intero stack GPL di gpservice

---

## 7. Licensing

- GP Bar: codice originale **MIT** (`LICENSE`), nessun codice/asset/UI da GP Connect
- Il backend `gpclient` è GPL-3.0 ma invocato come **processo figlio** (exec, non linking né ridistribuzione) → nessuna obbligazione GPL sul codice di GP Bar
- GP Connect.app non è stato patchato né modificato: è semplicemente insostituibile per un uso senza limiti, perché il suo limite risiede solo nel suo codice proprietario

---

## 8. Debug tray su macOS 27 beta — diario completo e handoff

> **✅ CHIUSO — vedi §8.12.** La causa radice era un **deadlock nostro** (`vpn.rs::update` emetteva `vpn-state` tenendo il mutex; il listener sincrono lo ri-bloccava via `snapshot()`), **non** il tray né il seed macOS. Ipotesi D (firma Apple) **falsificata**. §§8.1–8.11 restano come diario del percorso di debug: tutte le letture "zero eventi" erano l'app congelata, non un problema di routing.

**Quando:** 20 ottobre 2026, sessione successiva a §§1–7 (stesso giorno).
**Sintomo:** il lucchetto compare nella barra menu e il tooltip funziona, ma **il click non fa nulla** (solo animazione di pressione): niente pannello, niente menu, zero eventi.
**Ambiente:** macOS **27.0.1 beta, build 26A434** (arm64, display 1440×900 @1x). Su questo seed il routing dei click sugli status item verso l'app è rotto o cambiato. *(nota post-mortem: il seed era una correlazione, non la causa — la diagnosi è in §8.12)*

### 8.1 Matrice dei test (tutti con ZERO eventi TrayTarget)

| # | Configurazione | Eventi TrayTarget | Menu nativo | Finestra item 685×30 (layer 1000) |
|---|---|---|---|---|
| 0 | v0.1.0 + build strumentata: `tauri::tray` + tray-icon **0.25.1**, menu passato, `menu_on_left_click=false` | **ZERO** | — | presente |
| 1 | fix v1: tray diretto **0.25.1** + attach nativo NSMenu **post-`create()`** + menu-first via `Builder::on_menu_event` | **ZERO** | no | presente |
| 2 | fix v2: tray diretto **`=0.25.0`** (attach persistente **dentro `create()`**) + `with_menu_on_left_click(false)` + handler proprio: Left Click Up → `show_panel` | **ZERO** | no | **assente** |
| 3 | idem #2 + identifier rinominato **`dev.gpbar.bar`** + re-sign ad-hoc allineata alla firma di GP Connect | **ZERO** | no | **presente di nuovo** |
| 4 | Test A: idem #3 + **rimozione subview `TaoTrayTarget`** dal bottone post-create (menu attached) | **ZERO** | no | presente |
| 5 | Test LSUIElement: idem #4 + **`LSUIElement=true`** nel plist del bundle (via `src-tauri/Info.plist`) | **ZERO** | no | **assente** (solo pannello) |
| 6 | Test B: idem #4 + **target/action nativo** su `NSStatusBarButton` (`define_class!` GPBarTrayTarget, azione `onTrayClick:` → `show_panel`) | **ZERO** (e anche l'azione non scatta) | no | presente |
| 7 | Test C: idem #6 ma app **REGULAR** (niente LSUIElement, niente `set_activation_policy` runtime — come GP Connect) | **ZERO** (azione non scatta nemmeno da Regular) | no | — |

Log sempre catturato via `open --stderr <file>`: **0 byte** in tutti i casi — nemmeno Enter/Move/Leave.

### 8.2 Fatti accertati

- **La TrayTarget di tray-icon non riceve MAI eventi mouse su questo seed**, in ogni versione/configurazione provata — inclusa quella meccanicamente identica a quella che usa GP Connect (0.24.2/0.25.0, attach in `create()`).
- **Nemmeno il menu nativo attached si presenta**: con l'attach persistente (0.25.0), su macOS normale un click sinistro presenta il menu via cell-tracking del bottone; qui solo animazione.
- L'**animazione di pressione è highlight system-side** (AppKit/WindowServer), non di tray-icon: l'`highlight(true)` di tray-icon richiederebbe `mouseDown` → produrrebbe eventi nel log.
- **Solo `open` (LaunchServices) crea lo status item**; eseguire il binario direttamente non lo crea.
- **GP Connect FUNZIONA su questo seed** (il suo click apre il popover). Differenziali accertati col nostro binario: tauri 2.12.3, `TaoTrayTarget` presente, `tauri_nspanel` (WebviewPanelManager), `TrayIconOptions` con chiavi `menuOnLeftClick`/`showMenuOnLeftClick`, firma hardened-runtime (`com.yuezk.gpgui`, CodeDirectory v20500 + runtime), **nessun LSUIElement** nel plist (app Regular). Quindi il meccanismo TrayTarget **può** funzionare su questo seed — ma non nella nostra app.
- Altre app menu bar di terze parti: funzionano su questo seed (confermato dall'utente).
- **tray-icon è assolta**: il diff 0.24.2 ↔ 0.25.0 (estratte entrambe e confrontate) è **solo refactoring cosmetic** (unsafe→safe, 205 righe); l'unica differenza funzionale con 0.25.1 è la rimozione dell'attach persistente (PR #365, comportamento su macOS 27 riportato in tauri-apps/tray-icon#355). A parità di meccanismo GP Connect funziona e noi no ⇒ **la differenza sta nella nostra app o nel suo contesto**, non nella crate.
- La **finestra status item 685×30 layer 1000** (verificata via CGWindowList, `winlist.swift`) compare/scompare a seconda della config (assente con 0.25.0 + vecchio id; presente con 0.25.1 e con 0.25.0 + id nuovo): utile come segnale strumentale del tipo di hosting.
- Identifier e firma sistemati (`dev.gpbar.bar`, firma ad-hoc allineata all'identifier come GP Connect) — igiene corretta e registrazione fresca nel sistema, ma **non ha risolto**. Il warning tauri sull'id è sparito.
- Log di sistema (`log show`, messaggi contenenti "GP Bar"/"gpbar"): **vuoti**, nessun errore evidente lato WindowServer.

### 8.3 Ipotesi residue (in ordine di probabilità)

**A. La subview TrayTarget copre il bottone e intercetta l'hit-test senza ricevere gli eventi** → il click non arriva né alla subview né al tracking nativo del bottone.
**Test:** rimuovere la subview TrayTarget dopo `create()` (enumerate `button.subviews()`, rimuovere la classe `TaoTrayTarget`) mantenendo il menu attached in `create()` (=0.25.0): il click dovrebbe finalmente presentare il menu nativo (UX menu-first).

**B. Target/action nativo sul bottone** (stile Electron/Docker, 100% in-process): `define_class!` objc2 con metodo azione + `button.setTarget/setAction` → il click passa da `NSApp.sendAction`, a prescindere da subview e hosting. Solido fallback se A fallisce; UX a un click → pannello.

**C. Activation policy:** GP Connect è Regular (nessun LSUIElement), noi Accessory (`set_activation_policy(Accessory)`). Test da una riga: omettere la chiamata → se gli eventi fluiscono il fattore è la policy. Poco probabile ma economico.

**D. Hosting WindowServer del seed** rotto per app non-firma-Developer: se A+B+C falliscono, riprodurre con un'app minimale tray-icon non-tauri e aprire un feedback Apple; la sessione con screencapture può raccogliere evidenze visive del comportamento.

### 8.4 Piano per la prossima sessione (con screencapture disponibile)

1. **Prova A** — rimozione subview TrayTarget: re-aggiungere `objc2-app-kit` in Cargo.toml e ~30 righe in `setup` dopo `build()`:
   ```rust
   // pseudocodice — API: NSStatusItem::button(mtm), NSView::subviews() -> Vec<Retained<NSView>>,
   // NSView::removeFromSuperview(), nome classe via NSStringFromClass / objc2 ClassRef
   let item = tray.ns_status_item().unwrap();
   let button = item.button(mtm).unwrap();
   for sub in button.subviews() {
     if class_name(sub) == "TaoTrayTarget" { sub.removeFromSuperview(); }
   }
   ```
   Se il menu si presenta al sinistro ⇒ risolto in menu-first: decidere se tenere menu-first oppure aggiungere B per il pannello a un click.
2. Se A fallisce ⇒ **B** (target/action + `define_class!`).
3. Se B fallisce ⇒ **C** — che ora include il **test LSUIElement**: aggiungere in `tauri.conf.json` → `bundle > macOS > infoPlist` la chiave `"LSUIElement": true` (il nostro bundle **non** la dichiara: la policy è settata a runtime via `set_activation_policy(Accessory)`; GP Connect non la dichiara ma è Regular). La registrazione come *agent* fin dal lancio è una variabile d'identità non ancora testata — poi **D**.
4. Strumentazione pronta: `winlist.swift` (presenza finestra item), log via `open --stderr`, probe già nel codice (marker `TODO(debug)`), `log stream` di sistema durante i click, screencapture per osservare animazione/menu/pannello.
5. **Procedura di test:** SEMPRE via `open "<bundle>" --stdout/--stderr <log>` (mai binario diretto: non crea lo status item); `pkill -f "GP Bar.app/Contents/MacOS/gp-bar"` prima di rilanciare (istanza attuale: PID 3186).

### 8.5 Stato del codice (handoff)

- `src-tauri/src/main.rs` — **v2 completa**: tray diretto tray-icon (pin `=0.25.0`), `with_menu` + `with_menu_on_left_click(false)`, handler globale `TrayIconEvent::set_event_handler` (sostituisce quello di tauri; Left Click Up → `show_panel`), menu-first via `Builder::on_menu_event` (muda 0.20 unificata con tauri), `show_panel` ancorata al cursore (`(cursor.y + 18.0).max(32.0)`), `sync_tray_with_state` via `run_on_main_thread`, `thread_local! TRAY` tiene vive le handle (Rc-based, solo main thread).
  - **Probe `TODO(debug)` da rimuovere a fix confermato** (3): log ingresso `show_panel`, log focus finestra, log eventi tray non-Click.
- `src-tauri/Cargo.toml` — `tray-icon = { version = "=0.25.0", … }` con commento esaustivo; `objc2-app-kit` rimosso in v2 (**riaggiungerlo per il test A**).
- `src-tauri/tauri.conf.json` — identifier **`dev.gpbar.bar`** (rinominato da `dev.gpbar.app`, warning tauri sparito).
- **Dopo ogni `npm run tauri build`:** re-sign manuale `codesign --force --sign - --identifier dev.gpbar.bar "<bundle>"` (il bundle esce linker-signed con identifier derivato ≠ bundle id).
- Git: modifiche v2 **non committate** (Cargo.toml, main.rs, tauri.conf.json); HEAD `9285742`.
- API 0.25.0 verificate: `TrayIconBuilder::new().with_id(..)` (**with_id è metodo d'istanza**), `with_menu`, `with_menu_on_left_click`, `TrayIconEvent::Click { id, position, rect, button, button_state }` (campi extra rispetto alla 0.25.1: usare `..` nel match), `MouseButton`/`MouseButtonState` definite in tray-icon stesso (export root), `set_event_handler` globale con firma `Fn(TrayIconEvent) + Send + Sync`.

### 8.6 Errori di build già pagati (non ripeterli)

1. `TrayIconBuilder::with_id(TRAY_ID)` → **E0061**: `with_id` è metodo d'istanza → serve `TrayIconBuilder::new().with_id(..)`.
2. `slot.borrow().as_ref()` inline in un let-else → **E0716** (temporaneo droppato): binding esplicito `let guard = slot.borrow();`.
3. Doc comment (`///`) su `thread_local!` → warning `unused_doc_comments`: usare commento normale.

### 8.7 Artefatti di questa sessione

- Bundle attivo: `src-tauri/target/release/bundle/macos/GP Bar.app` (v2 + identifier nuovo, PID 3186, lucchetto visibile ma inerte).
- Log probe: `/private/var/folders/jx/6bnhh0757wsb15mg0h3jgzq80000gn/T/opencode/gpbar-open-debug.log` (0 byte = zero eventi).
- Cartella temp `/private/var/folders/jx/…/opencode/`: `winlist.swift`, `tray-icon-0.24.2/`, `tray-icon-0.25.0/` (estratte per i diff; riscaricabili da `https://static.crates.io/crates/tray-icon/tray-icon-<ver>.crate`). **NB:** cartella temporanea, potrebbe non sopravvivere al riavvio.

### 8.8 Analisi esterna (post-handoff) — validazione critica contro la matrice empirica

Un'analisi esterna ha proposto cause e soluzioni per il tray; confrontata con la matrice §8.1:

- **Opzione 1 — "rimuovere `with_menu`, il bottone torna pulsante normale e i Click fluiscono": FALSIFICATA dai test #0/#1.** Zero eventi anche SENZA alcun menu attached (0.25.1 non attacca mai il menu). La teoria "deadlock `with_menu_on_left_click(false)` + menu natively attached" non regge meccanicamente (il flag è interno a tray-icon: AppKit non lo conosce) e non spiega i test senza menu.
- **Opzione 2 — target/action su `NSStatusBarButton`: coincide con l'ipotesi B.** Condivisa: è il percorso 100% in-process (`NSApp.sendAction`), quello usato da Electron/Docker.
- **Opzione 3 — `LSUIElement` nel plist del bundle: test NUOVO valido**, aggiunto al ladder in §8.4 (via `tauri.conf.json` → `bundle.macOS.infoPlist`). Da verificare anche il claim collaterale (risolverebbe l'assenza di status item con il binario diretto).
- **Root-cause narrativa** ("il sistema assume il controllo del click per presentare il menu e non inoltra mouseDown/mouseUp"): coerente col seed di agosto (issue #355) ma NON col nostro, dove il menu nativo **non si presenta affatto**.

### 8.9 Hardening sicurezza (dall'analisi esterna, validato sul codice)

1. **Log connect root-owned world-readable** — il log è già in `~/Library/Logs/gp-bar/gpclient-connect.log` (`config.rs:55-56`), NON in /tmp (claim esterno inesatto), MA il worker **root** lo crea con umask 022 → file `0644` di root in una directory utente: leggibile da qualunque utente locale. Fix economico: `umask 026` nello script di connect (o permessi `0640 root:admin`). La FIFO proposta dall'analisi è fragile (semantica blocking, permessi FIFO) e viene sconsigliata.
2. **Validazione FQDN del portale** prima di `osascript` (defense-in-depth oltre l'escaping già testato 7/7): whitelist regex hostname in `config.rs`, da implementare.
3. **Verifica del path del binario prima del kill** — `build_kill_script(pid)` fa `kill -INT {pid}` eseguito come root; `process_alive(pid)` esiste già (vpn.rs, via `ps -p`) ma verifica solo l'esistenza. Aggiungere `proc_pidpath`/`ps -o comm=` per confermare che il PID sia ancora gpclient (PID recycling) e gestire il lock stale (crash senza cleanup).

### 8.10 Conclusioni dei test A / LSUIElement / B / C (sessione con screencapture)

- **Test A (rimozione subview `TaoTrayTarget`) fallito** (PID 11768): l'overlay non è il (solo) colpevole.
- **Test LSUIElement fallito** (PID 12374): dichiarare `LSUIElement=true` nel plist non cambia nulla; la finestra item è scomparsa dalla CGWindowList (solo pannello).
- **Test B (target/action nativo) fallito** (PID 13385): nemmeno `NSApp.sendAction` — il percorso di consegna eventi **più interno possibile** (quello di Electron/Docker) — riceve il click. Il metodo `onTrayClick:` non viene mai invocato.
- **Test C (app Regular, identità identica a GP Connect) fallito** (PID 13819): niente LSUIElement, nessuna policy runtime, bundle id pulito, firma ad-hoc allineata — ancora zero.
- **Conclusione forte**: TrayTarget (subview overlay), cell-tracking del bottone col menu attached, e target/action sono **tre vie indipendenti** per cui un click dovrebbe raggiungere l'app; tutte silenziose ⇒ **l'evento mouse non viene mai consegnato al processo dal WindowServer** su questo seed. Non è routing interno AppKit: è a monte (hosting/consegna della finestra status item).
- **Ipotesi D (ora dominante)**: il seed 27.0.1 beta (26A434) non consegna i click della barra menu alle app con firma ad-hoc. GP Connect funziona perché firmata da sviluppatore Apple (CodeDirectory v20500, hardened runtime, identifier coincidente). **Prossimo passo**: firmare con un certificato Apple di sviluppo (basta un Apple ID gratuito per test locale) e ritestare; se confermato, aprire un feedback Apple.

### 8.11 Stato del codice dopo i test (per la prossima sessione)

- `main.rs` `setup`: test A+B cumulativi (rimozione subview + `define_class! GPBarTrayTarget` con `setTarget/setAction` → `show_panel`); **test C attivo**: `set_activation_policy` commentata con `TODO(debug)` (solo `#[cfg(not(target_os = "macos"))]` la chiama).
- `src-tauri/Info.plist` → rinominato `Info.plist.LSUIElement.bak` (ripristinarlo o eliminarlo a fine debug).
- API objc2 0.6.5 verificate: `define_class!` con `#[ivars = Struct]` (struct `pub`), metodi in `impl` dentro la macro (path semplici non qualificati!), `Allocated` in `objc2::rc`, allocazione via `msg_send![Self::class(), alloc]` → `set_ivars` → `msg_send![super(this), init]`; `button.setTarget/setAction` sono `unsafe` (blocco `unsafe`).

### 8.12 CAUSA RADICE TROVATA E FIX (chiusura dell'incidente)

**Come è stata trovata.** Dopo che il test B "puro" (menu staccato) e l'ipotesi D furono falsificati — una repro minimale ad-hoc (`~/Rust/tray-minimal`, tao 0.37 + tray-icon 0.25.0, bundle manuale) **funziona** sullo stesso seed — il bisect è diventato: la colpa è nella nostra app. Con `windows: []` (nessuna webview) gli eventi CONSEGNAVANO (6 `show_panel` nel log, prima consegna della sessione); con la finestra visibile l'app si apriva **inanimata** (nemmeno la X rispondeva). Il `sample` del processo ha mostrato il **main thread bloccato in `pthread_mutex_wait` dentro `WebURLSchemeHandler::startTask`** (il custom protocol di wry/tauri, durante il load della pagina). Rebuild in debug (`npm run tauri build -- --debug`) e simbolizzazione: ecco il quadro completo.

**Il deadlock (self-deadlock nel nostro codice, latente da v0.1.0):**

```
tokio worker (spawn_status_poller, tick ogni 2s, update() CHIAMA SEMPRE emit)
  vpn.rs update(): prende inner.lock()  ← MUTEX TENUTO
  → app.emit("vpn-state")   ← EMIT SOTTO LOCK: tauri invoca i listener
                               RUST SINCRONICAMENTE SULLO STESSO THREAD
  → listener "vpn-state" (main.rs) → sync_tray_with_state()
  → controller.snapshot()   ← STESSO MUTEX, STESSO THREAD:
                               std::sync::Mutex NON è rientrante
  → ☠ il poller muore TENENDO il mutex (al primo tick, ~2s dal lancio)

main thread (poi, quando il frontend carica la pagina):
  WebURLSchemeTask → ipc get → invoke_handler → comando get_state
  → controller.snapshot() → inner.lock()  ← BLOCCATO PER SEMPRE
  → NSApplication run non torna MAI al run loop
  → ☠ NESSUN evento servito: tray, finestra, X, tutto morto
```

**Perché quadra con ogni sintomo:**
- tooltip e animazione di pressione funzionavano → disegnati dal WindowServer, non richiedevano l'app viva;
- senza finestra il tray **funzionava** (nessun load pagina → nessun `get_state` → main thread libero → i click arrivavano e `show_panel` loggava 6 volte);
- tutte le 8 configurazioni della matrice §8.1 davano "zero eventi" → erano tutte **app congelate**: ogni conclusione di routing su tray-icon/TaoTrayTarget/menu era un artefatto del deadlock (la minimale ha confermato che gli eventi overlay fluiscono regolarmente sul seed);
- l'evento non doveva "raggiungere l'app": l'app non serviva **nulla di nessuno**.

**Ipotesi D falsificata.** Nessuna identità di firma Apple sul sistema (solo CommandLineTools) e la minimale ad-hoc funziona: il seed consegna i click anche ai binari ad-hoc. GP Connect funzionava perché era viva, non perché firmata.

**Perché v0.1.0 "funzionava" sul vecchio macOS:** il bug è **identico in HEAD `9285742`** (stesso `update()` con emit-sotto-lock, stesso listener→`snapshot`). Il `Cargo.lock` però è **ignorato da git** (`.gitignore`), quindi il build originale risolse una tauri più vecchia; il rebuild di questa sessione ha pescato tauri **2.12.4**, dove il percorso emit→listener sincrono sul thread emittente ha fatto esplodere il bug latente. Il seed macOS era **correlazione, non causa**. → *Raccomandazione: tracciare `Cargo.lock` in git.*

**Il fix (doppio strato):**
1. `vpn.rs::update()`: il guard del mutex viene **droppato prima** dell'emit; inoltre l'evento parte **solo se lo stato è davvero cambiato** (niente più risvegli ogni tick).
2. `main.rs`: il listener `vpn-state` legge la fase **dal payload dell'evento** invece di ri-prendere il lock con `snapshot()` — elimina la rientranza alla radice anche se qualcuno re-introducesse un emit-sotto-lock.

**Verifica:** build release, lancio, `sample` → main thread nel run loop (`nextEvent`/`BlockUntilNextEvent`), **zero mutex**; finestra interattiva e click sinistro sul lucchetto → pannello a fuoco (conferma visiva dell'utente). Config di produzione ripristinata: `visible: false`, policy Accessory, menu attached, pin `=0.25.0` mantenuto (unica config verificata sul campo; rilassarlo a 0.25.1 è lavoro futuro), deps objc2 di test rimosse, identifier ripristinato a `dev.gpbar.app`.

**Residui aperti:** hardening §8.9 (umask log connect, validazione FQDN, verifica PID→path pre-kill); rivalutare il pin `=0.25.0` ora che la causa è nota; tracciare `Cargo.lock`.

---

## 9. Bug connect: SAML "cannot be run as root" con gpclient 3.1.0 (risolto)

**Sintomo** (20 ottobre 2026, cookie SSO scaduto): il connect dal pannello fallisce; log applicativo:

```
INFO  SAML auth launch: gateway=false, clean=false, browser=external
WARN  Failed to connect portal with prelogin: Command { std: "/Applications/GP Connect.app/Contents/Helpers/gpauth" "vpn.example.com" "--saml-request" "…" } cannot be run as root
```

I connect precedenti riuscivano solo grazie a `--cookie-cache` (cookie valido in `/var/root/.config/gpclient/`): con il cookie scaduto emerge il path SAML browser, mai esercitato prima.

**Causa** (verificata nel sorgente, clone aggiornato in `reference/`): gpclient 3.1.0 su macOS gestisce l'auth browser esterna in **tre attori**: spawna l'helper `gpauth` di GP Connect con l'HTML `SAMLRequest`, l'helper apre il browser **come utente loggato**, e il redirect `globalprotectcallback:` è gestito da "GP Connect Login.app" (`crates/auth/src/callback/macos.rs` registra `com.yuezk.gpauth` come handler dello scheme). Lo spawn passa per `CommandExt::into_non_root` (`crates/gpapi/src/process/command_traits.rs`), che risolve l'utente "reale" da `SUDO_UID`/`PKEXEC_UID`/`DOAS_USER` (`crates/gpapi/src/process/users.rs`). Il contesto `osascript … with administrator privileges` **non imposta nessuna** delle tre → `get_non_root_user()` fallisce → "cannot be run as root". `desktop_session_env::apply` su macOS è un no-op: `SUDO_UID` è l'unico tassello mancante.

**Fix**: `vpn.rs::build_connect_script` prefissa il comando con `SUDO_UID=$(stat -f %u /dev/console)` — l'UID dell'utente console è la controparte esatta di ciò che `sudo` metterebbe nell'env. Test unitario aggiornato.

**Nota architetturale**: con gpclient 3.1.0 l'SSO browser su macOS **dipende da GP Connect installato** (helper `gpauth` + Login.app per il callback scheme). È il comportamento upstream (`GP_AUTH_BINARY` è comunque overridabile via env). Alternativa futura: modalità webview embedded di gpclient (`browser=embedded`) — anch'essa richiede però il drop a utente non-root, quindi `SUDO_UID` resta necessario.

---

## 10. Bug disconnect: "Disconnecting" per sempre — root figli di authtrampoline non ricevono segnali (risolto via launchd)

**Sintomo**: Disconnect dal pannello → fase "Disconnecting" congelata, tunnel attivo, nessun errore nel log. Stesso identico comportamento nella GUI di GP Connect contro i worker avviati da GP Bar.

**Cosa NON era** (tutto falsificato empiricamente):
- Non era `gpclient disconnect`: SIGINT via sysinfo esce 0 anche a vuoto, ma `sudo kill -INT <pid>` come root non aveva **nessun** effetto.
- Non era l'orfanizzazione (PPID 1): sonda root con parent vivo → ancora sorda (test B2).
- Non era la sessione: sonda con `setsid` → ancora sorda (test C).
- Non era il runtime tokio del worker (sample: driver su kevent, pompa su `__select`, ping nel tunnel ok) — e il problema si riproduceva identico con una sonda C di 20 righe senza tokio.
- Non era il mittente: anche il `sudo kill` dell'utente da Terminal non aveva effetto.

**Causa** (matrice di esperimenti con sonda C `sigtest.c` + `kinfo_proc` via `sysctl`):

| Contesto di nascita | uid | Ascolta i segnali? |
|---|---|---|
| shell utente | 501 | ✅ |
| authtrampoline/AEWP (osascript admin) | 501 | ✅ |
| **authtrampoline/AEWP** | **root** | ❌ sordo (INT/TERM/HUP; solo KILL) |
| **launchd (LaunchDaemon)** | **root** | ✅ |

Su macOS 27.0.1 (26A434) beta, i processi **root generati da `AuthorizationExecuteWithPrivileges`** (il trampoline di `osascript … with administrator privileges`) non agiscono su nessun segnale catturabile: le disposition dicono "caught" (`kinfo_proc.p_sigcatch`), il `kill` ritorna 0, ma l'handler non viene mai eseguito — perfino un SIGHUP a disposition default non termina il processo. Ogni worker VPN nato in quel contesto è **indisconnettabile** per chiunque (GP Bar, GP Connect GUI, sudo manuale). Il fallback SIGKILL funziona ma **bypassa il teardown del vpnc-script**, lasciando le impostazioni DNS di macOS puntate sui resolver VPN e le rotte iniettate — rete rotta.

**Fix** (commit): GP Bar non fa più nascere il worker da AEWP. Il connect installa e carica un **job launchd one-shot** (`/Library/Application Support/GP Bar/gp-worker.plist`, `RunAtLoad`, no KeepAlive, fuori da `/Library/LaunchDaemons` → niente autostart al boot): il worker nasce figlio di launchd, root e **signalable**. `SUDO_UID`/`SUDO_USER` dell'utente console passati via `EnvironmentVariables` del job (per il drop dell'helper SAML, vedi §9). Il disconnect è fail-loud: `gpclient disconnect` + polling del lock file (max 10s) + `exit 1` se il worker non scende; **nessun SIGKILL automatico** per design. Nota: `install` non crea le directory padre — serve `mkdir -p`.

**Verifica end-to-end**: giro completo da GP Bar — disconnect del worker CLI-born ✓, connect via launchd ✓ (job `dev.gpbar.worker`, worker session leader), disconnect del worker launchd-born ✓ (DNS ripristinati dal vpnc-script, lock rimosso, fase Disconnected).

**Residui**: (a) la sordità AEWP-root è un difetto della beta macOS da segnalare ad Apple (FB); (b) TOCTOU locale sul file staged prima dell'`install` root (rilevante solo su macchine multi-utente ostili); (c) il log connect resta root-owned (umask) — vedi §8.9; (d) `Cargo.lock` ancora ignorato.

---

## 13. BTM: teoria quarantena cdhash e turnover automatico

**Anomalia D — teoria superstite: quarantena BTM per re-sign runtime.** Il daemon `dev.gpbar.helper` registrato via SMAppService spariva dai record BTM con `NotFound` spontanei. Teoria superstite: l'app scriveva a runtime la `Contents/Library/LaunchDaemons/dev.gpbar.helper.plist` e poi ri-firmava ad-hoc l'intero bundle (`sync_daemon_plist` + `re_sign_bundle`), cambiando il **cdhash a ogni avvio**; un record BTM che punta a un cdhash ormai non corrispondente viene considerato corrotto → quarantena → `NotFound` spontaneo e `register` che fallisce con -67054 ("Codesigning failure loading plist"). La teoria alternativa del flag `disabled` di launchd è stata **falsificata empiricamente**: `launchctl print-disabled` mostra il servizio `enabled`, eppure non riparte. 

**Fix strutturale (questo branch):** la plist del LaunchDaemon è ora generata a **build time** da `scripts/finalize-bundle.mjs` (step 3, PRIMA del codesign ad-hoc: la plist è coperta dalla firma del bundle) e l'app non scrive/né ri-firma più nulla a runtime — niente deriva del cdhash dopo l'installazione. Il turnover del daemon residente è ora gestito via protocollo (sotto), non più via unregister+register forzato.

**Test di sblocco previsto:** `sudo sfltool resetbtm` — da eseguire **da Terminal da parte dell'utente** (non dall'agente), svuota i record BTM quarantinati e permette una registrazione pulita successiva. Su questa beta di macOS 27.0.1 BTM resta fragile: NON eseguire `sfltool resetbtm` né `launchctl load/bootout` in automazione.

**Turnover automatico del daemon via BUILD_REV handshake.** Il daemon è `KeepAlive`/residente: un rebuild del binario NON lo sostituisce. Entrambi i binari del crate (app e `gp-helper`) ricevono da `build.rs` lo stesso `cargo:rustc-env=BUILD_REV=<git rev-parse --short HEAD>` (fallback: `CARGO_PKG_VERSION`). Protocollo: il client, prima di ogni operazione XPC, manda `get_version`; se la `BUILD_REV` del daemon differisce dalla propria manda `terminate_self` — il daemon risponde Ok e POI esce (`exit(0)` dopo ~100ms su un thread, così il reply parte), launchd KeepAlive lo ri-spawna dal binario NUOVO su disco; il client attende ~300ms (`proto::TURNOVER_SETTLE_MS`) e riprova l'operazione su una connessione fresca. Un daemon pre-handshake (build senza l'op) risponde "unknown operation": il client logga e prosegue, senza turnover possibile.

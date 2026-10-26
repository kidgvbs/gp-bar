# Recap — Nuovo client GUI per la VPN aziendale (GP Bar)

**Data:** 20–22 ottobre 2026
**Richiesta:** riscrivere un client GUI come GP Connect.app ma **senza il limite di 1 ora** sulla durata della connessione VPN, usando REA per l'analisi.
**Esito:** ✅ `GP Bar.app` in produzione nella menu bar, nessun limite di tempo, **zero prompt** su connect/disconnect (soluzione 3, wrapper sudoers NOPASSWD). La soluzione 1 (SMAppService+XPC) è validata meccanicamente ma **parcheggiata su branch** per via di comportamenti instabili di BTM su questa beta — vedi **§12**.

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
│   ├── src/vpn.rs                               # macchina a stati, poller lock file, tailer log JSON, helper admin (`run_admin`)
│   ├── src/config.rs                            # portali/porte persistiti (default: vpn.example.com)
│   ├── scripts/gp-admin                         # wrapper root-owned: operazioni fisse (il confine di sicurezza)
│   ├── scripts/setup-admin.sh                   # installer una-tantum (wrapper + sudoers NOPASSWD)
│   └── src/icons.rs                             # icone tray template disegnate via SDF (22×22, antialiasing 4×)
└── reference/GlobalProtect-openconnect/        # clone shallow del sorgente open source (gitignored)
```

Comandi IPC: `get_state`, `get_panel_effect`, `get_config`, `get_versions`, `add/remove/select_portal`, `set_reuse_cookie`, `select/preferred/remove/add_gateway`, `connect`, `disconnect`, `cancel`, `get_network_health`, `repair_network`, `quit`.

**Gateway nel picker** — come si riempie la lista, in ordine di affidabilità:

1. **Inserimento manuale** (`add_gateway` + riga input nell'editor portali): la lista dei gateway è stabile, si legge una volta dal prompt interattivo di gpclient (`gpclient connect <portal>` → "Which gateway do you want to connect to?") e si scrive nel picker. Valida il charset come tutte le altre path gateway (il nome finisce nel plist del job).
   **Il valore giusto da salvare è il NOME, non l'FQDN**: il prompt mostra `nome (indirizzo)` (Display di gpapi `Gateway`), e il connect accetta entrambi (`PortalConfig::find_gateway`: `name == x || address == x`) — con i soli nomi il picker è pulito e l'harvest non reintroduce duplicati in forma FQDN (il log stampa comunque `nome (indirizzo)` e `parse_gateway` cattura il primo token).
2. **Harvest dal log di connect** (`vpn::parse_gateway` → `harvest_gateway`): "Connecting to the selected/only available gateway", "Auto-gateway: attempting gateway X".

Il tentativo di **lista completa pre-connect via cookie** (commit 8475510: `gp-admin show-cookie` + replica della `POST /global-protect/getconfig.esp` in `portal_config.rs`) è stato **rimosso** dopo la verifica empirica: il portale reale è a password (prelogin "Enter login credentials", panos-version 1) e rifiuta la getconfig senza la password vera (`512 auth-failed-password-empty`) — GP Bar per design non tocca credenziali. Il cookie cache serve solo al re-auth del **gateway** (login diretto `ssl-vpn/getconfig.esp`, come mostra il log di connect), non al portale. Considerazione operativa maturata sul campo: ripetuti tentativi di auth falliti contro il portale (dall'app a ogni apertura del pannello, e dai probe di debug) hanno plausibilmente contribuito a invalidazioni di sessione durante i connect — la classe di rischio è stata eliminata togliendo del tutto la replica di rete.

Stati: `Disconnected → Connecting → Connected → Disconnecting → Disconnected/Error` con rilevamento anche di VPN avviate fuori dall'app (lock file esistente).

*(Nota storica: `osascript.rs` — il wrapper admin di v0.1 — è stato rimosso in `52d9767` quando l'elevazione è passata al wrapper sudoers; il §12 racconta il percorso.)*

---

## 5. Build, test e stato attuale

- ✅ `cargo test`: **16/16** (escaping shell/AppleScript, script builder, parsing gateway, cookie portale/form getconfig/XML config, icone)
- ✅ Build release (`npm run tauri build`): `src-tauri/target/release/bundle/macos/GP Bar.app` (6.9 MiB, arm64, ad-hoc signed)
- ✅ Smoke test: app avviata via `open`, processo stabile, tray visibile (lucchetto vuoto = disconnesso)
- ⏳ **Da collaudare a mano** (serve password admin + SSO nel browser): flusso connect completo, posizionamento popover sotto l'icona, connessione oltre le 2 ore per confermare l'assenza di limiti
- Git: repo inizializzato, 2 commit (`6e77964`, `de51d55`)

Note operative: 1 prompt admin per connect, 1 per disconnect (come `sudo` da terminale). Il VPN sopravvive al quit/riavvio dell'app.

---

## 6. Daemons senza prompt — le due strade percorse

La vecchia roadmap ("demone via SMAppService + socket Unix") è stata realizzata, in due versioni alternative:

- **Soluzione 3 (produzione, `main`)**: wrapper root-owned + sudoers NOPASSWD — §11.
- **Soluzione 1 (branch `solution-1-smappservice`)**: LaunchDaemon via SMAppService + XPC al posto del socket Unix — §12. Parcheggiata per l'instabilità di BTM sulla beta corrente, con la nota di investigate in `docs/btm-27.0.1-beta.md`.

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

**Residui**: (a) la sordità AEWP-root è un difetto della beta macOS da segnalare ad Apple (FB); (b) TOCTOU locale sul file staged prima dell'`install` root — **chiuso** dalla soluzione 3 (§11: la plist è generata internamente al wrapper, niente staging); (c) il log connect root-owned — vedi §8.9 (mitigabile con umask, non critico per tool personale); (d) `Cargo.lock` ancora ignorato da `.gitignore` — raccomandazione §8.12 in sospeso.

---

## 11. Elevazione admin senza prompt: perché sudo -A non basta, e le due soluzioni in campo (21 ottobre 2026)

**Obiettivo**: zero prompt su connect/disconnect. Le tre soluzioni candidate, con esiti empirici:

### Matrice pam_tid / `-A` (soluzione 2, chiusa)

| Contesto | Prompt | Touch ID? |
|---|---|---|
| Terminal: `sudo <cmd>` | password | ✅ sì (`pam_tid` via `/etc/pam.d/sudo_local`) |
| Terminal: `sudo -A <cmd>` (askpass) | dialog password | ❌ no |
| App/pty + `sudo -A` | dialog password | ❌ no |

Il flag `-A`/askpass **sopprime il modulo biometrico** (la conversazione askpass bypassa `pam_tid`): nessuna via askpass potrà mai dare Touch ID. Lo anticipava la nota del doc utente; verificato empiricamente su questo seed. La password sarebbe comunque gestita dal nostro processo — da evitare.

### Soluzione 3 (scelta per `main`): wrapper root-owned + sudoers NOPASSWD

- `/usr/local/lib/gp-bar/admin`: wrapper shell root-owned 0755, **operazioni fisse** (`selftest|start <portal> <0|1>|stop|cancel`), argomenti validati internamente (charset portale `[A-Za-z0-9._-]`, flag 0|1), **nessun path dal caller** (sarebbe privesc), plist del job launchd **generato internamente** (chiude il residuo TOCTOU §10(b)).
- `/etc/sudoers.d/gp-bar`: `<utente> ALL=(root) NOPASSWD: /usr/local/lib/gp-bar/admin` — solo quel path. L'app usa `sudo -n`: nessun prompt, nessuna credenziale nel processo, fallimento mappato in istruzioni di setup una-tantum.
- Installazione: `sudo bash "<bundle>/Contents/Resources/scripts/setup-admin.sh"` (in Terminal, dove Touch ID funziona) — idempotente, selftest end-to-end incluso.
- Rischio residuo accettato e documentato: qualunque processo utente può attivare/disattivare la VPN, nulla di più.

### Soluzione 1 (in test su branch `solution-1-smappservice`)

SMAppService daemon + XPC (via `objc2-service-management` 0.3.2 + bindiggi `xpc_*` raw). Sviluppata su worktree `~/Rust/gp-bar-solution1` da un sub-agent.

**Probe `SMProbe.app` validato end-to-end su questa beta (21/10/2026):** registration OK da bundle in path stabile (`~/Applications`) firmato ad-hoc → `SMAppServiceStatus` 1 (Enabled); ping → `ok=true daemon_uid=0 echo=ping`. Il daemon parte on-demand via Mach service come root. Lezioni del probe:
- **Register da binario nudo in cartella temp → NotFound/EPERM**: SMAppService/BTM non riconosce l'app (non in LaunchServices, percorso non di installazione). Il record BTM resta in `RequiresApproval`. Serve: bundle in path stabile + `open` (registrazione LS).
- **L'enum status in questa SDK**: 0=NotRegistered, 1=Enabled, 2=RequiresApproval, 3=NotFound (il `Debug` di objc2 stampa il raw quando non matcha — in questa beta 3 non è mappato? in realtà 3=NotFound matcha; i valori vanno letti dall'header, non intuiti).
- **"Consenti in background"**: il toggle in Impostazioni di Sistema → Elementi di login è la controparte manuale dell'approvazione; con l'approvazione già attiva, `register` va a Enabled senza nuovo prompt.
- **Client XPC**: dopo `send_with_reply_sync` + reply, la `xpc_connection_cancel` genera un event `XPC_ERROR_CONNECTION_CANCELLED` — benigno, non un errore del daemon.
- **Nota di sicurezza valida per la soluzione 1 reale**: il bundle col daemon root deve vivere in un percorso **non riscrivibile dall'utente** (idealmente `/Applications`): un helper root dentro `~/Applications` è sostituibile dall'utente = privesc. Fix applicata sul branch anche per la risoluzione gpclient (`3a6d75b`): il daemon esegue solo i candidate Homebrew fissi, mai path dal config utente.

### Bug del pulsante Connect (risolto, commit 15bbed7)

`state.portal` nasceva `None` e solo `begin_connect` lo impostava; il menu nativo del tray (rimosso in 707cc8a) era l'unica path che lo seedava da `config.selected`. Risultato: app appena avviata → Connect disabilitato per sempre, segnalato "bloccato dopo il primo disconnect". Fix: `note_selected_portal`/`apply_selected_portal` (solo fasi idle Disconnected/Error), chiamati da setup e add/remove/select portal.

---

## 12. Giornata 22/10: la soluzione 3 in produzione, la 1 parcheggiata (con tre lezioni di debugging pagate)

### 12.1 Soluzione 3 chiusa e in produzione (commit `52d9767`, `f133a9f`)

Wrapper root-owned + sudoers NOPASSWD: setup una-tantum in Terminal (Touch ID), poi `sudo -n` su `/usr/local/lib/gp-bar/admin` per ogni operazione — **zero prompt, zero credenziali nel processo**, verificato anche dal canale dell'agente (sudoers concede solo quel path: il tentativo di `sudo -n cat` sul sudoers stesso viene rifiutato correttamente). Nel commit è finita anche la cancellazione di `HANDOFF-TRAY.md` (post-mortem del §8, già assorbito qui).

### 12.2 Soluzione 1: meccanica validata, ma quattro bug pagati di persona

Sviluppata sul branch da un sub-agent e testata da `~/Applications/GP Bar S1.app` (identità separata `dev.gpbar.s1`). Ogni bug di seguito è **fissato e committato** sul branch:

1. **Bundle identity duplicata** (`d5e1513`): S1 e GP Bar condividevano `dev.gpbar.app` → SMAppService/BTM risolveva il chiamante in modo ambiguo, la registration falliva **senza prompt** e l'errore era **invisibile** (la `ensure_ready` girava prima di `begin_connect` → `Err` mostrato da nessuno). Fix: identità propria (`dev.gpbar.s1` / "GP Bar S1") + il fallimento atterra nello stato Error del pannello (con wrapping: niente più errori ellipsizzati) + log su stderr.
2. **Privesc via gpclient_path utente** (`3a6d75b`, fix applicata in review): il daemon root leggeva `gpclient_path` dal config **user-writable** e lo eseguiva → privesc completa. Fix: il daemon risolve gpclient **solo** dai path Homebrew fissi; l'override del config resta user-side (display versione).
3. **BTM `-67054` "Codesigning failure loading plist"** (`831e58f`): la plist del LaunchDaemon scritta a runtime non è coperta dalla firma del bundle → register rifiutato. Fix: l'app ri-firma ad-hoc il bundle (user-writable) dopo la scrittura, come fa il build.
4. **EPERM silenzioso da app Accessory** (`999df9a`): il prompt di autorizzazione non può essere presentato da un'app mai frontmost. Fix: step-up a policy Regular per la chiamata, poi ritorno. **Confermato dall'utente: il prompt offre Touch ID.** ✅

Con questi fix la catena completa è validata: registration → launchd avvia il daemon on-demand → XPC → worker root → tunnel su.

### 12.3 Il muro: root signal-deaf esteso ai figli del daemon, e BTM instabile

- **Matrice segnali aggiornata** (estende il §10): su questa beta **anche i root figli di un LaunchDaemon** (launchd → daemon → worker) sono sordi ai segnali — non solo i figli di authtrampoline. `gpclient disconnect` esce 0, il lock resta, il Disconnect si congela. **Solo i figli diretti di launchd restano signalable.** Il daemon è stato ridisegnato di conseguenza (commit `0838ab3`): `start` genera e carica un **job launchd one-shot** per il worker (label `dev.gpbar.s1.worker`, come il wrapper della soluzione 3); `stop`/`cancel` scaricano il job.
- **Turnover del daemon** (lezione operativa): essendo il daemon KeepAlive/residente, **un rebuild del binario non lo sostituisce** — il vecchio processo continua a servire le connect dalla memoria. Metà dei "fallimenti" di testing della giornata erano in realtà la v1 ancora viva (individuata verificando che il worker aveva PPID = vecchio gp-helper, non 1). Serve unregister+register forzato (l'app lo fa se la plist del bundle non è current — si innesca toccando la plist); non testato fino in fondo a causa del punto dopo.
- **BTM instabile su questa beta** (anomalie C e D del `docs/btm-27.0.1-beta.md`): registrazione validata che ore dopo risulta **NotFound spontaneo**; e dopo un `launchctl bootout` manuale + re-register (accettata senza prompt), **il job non riparte mai più** (`active count = 0`, client XPC unreachable, daemon mai avviato). Nessun unregister/register successivo ha sanato lo stato. Da riprendere dopo un reboot del sistema.

### 12.4 Stato finale

- **Produzione: soluzione 3** in `~/Applications/GP Bar.app`, funzionante, zero prompt. `README.md` allineato (`1bf2526`).
- **Branch `solution-1-smappservice`** (worktree `~/Rust/gp-bar-solution1`, HEAD `0838ab3`): meccanicamente completa, parcheggiata per BTM. Da fare se ripresa: turnover automatico del daemon a ogni rebuild (check versione), test dopo reboot, spostare il bundle in `/Applications` se si vuole chiudere la falla del percorso user-writable.
- Cartelle della 1 ripulite (bundle S1, SMProbe, plist `GP Bar S1`); la nota per il collega su BTM è in `docs/btm-27.0.1-beta.md` (commit `9b98301`).

## 13. Popover "metà schermo": tre spazi coordinate che non si parlano (risolto, 26/10/2026)

**Sintomo**: il pannello si apriva circa a metà schermo su qualunque configurazione (mixed-DPI e singolo monitor), mai sotto l'icona. `global logical` + `set_position(Logical)` (a1c1d24) e `visibleOnAllWorkspaces` (ba72fa2) non cambiavano nulla.

**La catena di evidenza**: i lanci GUI non danno stderr → strumentazione su file (`panel_log`, `~/Library/Logs/gp-bar/panel.log`, 7759b5f): payload del tray, anchor, monitor, work area, posizione intesa vs effettiva. Dieci click reali hanno mostrato **ogni** volta:

```
tray click: anchor=(2118.0,60.0)
positioning: monitor_from_point(2118.0,60.0) found NO monitor — panel unpositioned
shown: actual outer physical=Ok((1080, 826))   ← il default centrato = "metà schermo"
```

Il posizionamento non veniva **mai** eseguito: non era un problema di set_position, era l'early-out del lookup.

**Causa radice** (dai sorgenti: tray-icon 0.25 `platform_impl/macos/mod.rs`, tao 0.37.1 `platform_impl/macos/{monitor.rs,util/mod.rs,window.rs}`, tauri-runtime-wry 2.12.2 `monitor/macos.rs`): tre spazi che non coincidono su macOS.

1. **Payload tray** (tray-icon): punti logici × scala dello schermo del tray, origine **alto-sinistra** (`mouseLocation` flippato con `CGDisplayPixelsHigh(main)` poi `.to_physical`). Sul laptop: (2118, 60).
2. **`monitor_from_point`** (tao): hit-test su `CGRectContainsPoint(CGDisplayBounds)` in **punti logici puri**, spazio CG **basso-sinistra**. 2118 > 1440 → fuori da ogni schermo → "nessun monitor". Un payload "physical" non può mai combaciare; anche uno logico sarebbe fuori del flip.
3. **`work_area`** di tauri: NSScreen `visibleFrame` nello spazio CG **basso-sinistra** × scala — mentre il codice la trattava come alto-sinistra.

**Fix** (`96489f1`): risoluzione **manuale** del monitor in UNO spazio canonico (punti logici, origine in alto). Bounds e visibleFrame di ogni monitor ricondotti lì; il punto del payload viene un-scaled per ogni scala candidata e hit-testato; clamp nella visibleFrame del vincitore; `set_position(LogicalPosition)` (in tao `to_logical` è identità per Logical → indipendente dalla scala, mixed-DPI incluso). Fallback: cursore con l'un-scale della scala primary (il to_physical col primary-scale di tao è noto-bug cross-monitor), poi angolo alto-destra del primario. **Verificato dall'utente** sul singolo monitor.

**Reopening mixed-DPI (26/10, stesso giorno — `5b4eb0e`)**: con l'esterno riattaccato il pannello tornava sul primario ("NO monitor matched"). La tabella monitor del fix sopra era SBAGLIATA: flipava le posizioni di tao con l'altezza del main, **specchiando ogni monitor non-primario rispetto al primario** — l'estraneo, che sta SOPRA il laptop, risultava sotto → l'anchor della sua menu bar (y≈−1430) non matchava niente. Il singolo monitor funzionava **per coincidenza**: l'origine del primario (0,0) è invariante sotto quel flip. La verità sui due spazi (validata contro i payload di entrambe le sessioni): **AppKit** (mouseLocation, frame NSScreen/NSWindow) è basso-sinistra; il payload tray nasce lì e viene flippato a top-left da tray-icon/tao con `CGDisplayPixelsHigh` — che **su questa build vale PUNTI** (900: con 1800 pixel il mouse risulterebbe fuori schermo in entrambi i dataset) — poi ×scala della finestra tray: **pre-scala il payload è già canonico**. Lo **spazio Quartz dei display** (`CGDisplayBounds`, passthrough **nudo** in `Monitor::position()` di tao; `work_area` di tauri vi aggiunge solo gli inset AppKit) è **già top-left**: la tabella è ora pura divisione per scala, zero flip. Bonus dello stesso fix: il clamp sul primario ora rispetta la menu bar da 30pt (col vecchio flip la work area partiva da y=0, 30pt dentro la barra). **Verificato dall'utente su entrambi i display**: anchor esterno (2070,−1410) → #30596 → set_position (1890,−1410); anchor laptop (1059,30) → #41033 → (879,30).

**Lezione**: quando su macOS un layer espone "physical" e un altro "logical", verificare SEMPRE origine e unità nel sorgente prima di convertire — ma anche: **un fix verificato su un monitor solo non chiude un incidente di geometria**; la simmetria (origine 0,0 del primario) può rendere un bug invisibile finché il caso multi-monitor non lo specchia. E quando due fonti (payload tray e tabella monitor) discordano, fidarsi di quella radente l'evento (AppKit) e ricavarne la convenzione dell'altra per falsificazione, non assumere che "CG" significhi una cosa sola: **lo spazio Quartz dei display e lo spazio AppKit delle finestre sono opposti in y** sulla stessa macchina. La strumentazione su file ha pagato per sé di nuovo: i click dell'utente senza terminale hanno chiuso anche il reopening.

## 14. Menu del tray al click destro senza attaccarlo (26/10/2026)

**Sintomo**: il README prometteva "Right click — open the tray menu: Connect / Disconnect / Quit", ma il menu era stato **rimosso** per salvare il pannello: la §8 aveva dimostrato che con un menu attaccato il sistema apre il menu al click **sinistro** e l'evento non arriva mai all'app (icona morta). Il right-click non faceva nulla — giustamente: nessun menu esisteva più.

**Causa strutturale** (dai sorgenti): tray-icon 0.25 attacca il menu in modo **incondizionato** (`setMenu` su NSStatusItem in `build`, nessun "attach on demand"), e `on_tray_click` al click sinistro col menu attaccato fa `performClick` (apre il menu nativo) invece di emettere l'evento. Su questa beta il sequestro è totale; su macOS stabili invece è il comportamento standard di `menu_on_left_click`. Nessuna via di mezzo: o il menu è attaccato e il sinistro è del sistema, o non lo è.

**Fix** (`ebd1b74`): il menu non si attacca **mai** — si **costruisce** una volta in setup (`tauri::menu`: Menu + MenuItem con id `connect`/`disconnect`/`quit`, handles in `TRAY_MENU: OnceLock`) e si apre **a mano** al click destro:

- **Perché il right-click arriva comunque**: `rightMouseDown:` emette `TrayIconEvent::Click{Right, Down}` **prima** di qualunque logica di menu (`send_mouse_event` precede `on_tray_click`), menu attaccato o no; senza menu il resto è solo un `highlight` innocuo.
- **Perché il menu appare all'icona**: `Menu::popup(window)` **senza posizione** percorre il ramo mouse-location di muda (`show_context_menu_for_nsview(view, None)` → `NSEvent::mouseLocation`, `in_view: nil`): coordinate schermo, zero dipendenza dalla geometria del pannello nascosto. L'UX è quella nativa dello status item.
- **Enable/disable per fase** in `sync_tray_with_state` (Connect in Disconnected/Error, Disconnect in Connected, entrambi spenti durante le transizioni — le stesse guardie di `do_connect`/`do_disconnect`, riusati **esattamente** dal menu: portale selezionato da config, reuse-cookie, gateway `None` = Automatico). Sync iniziale in setup: lock sicuro, il poller non è ancora partito.
- **Dettaglio API pagato**: in tauri 2.12 `AppHandle` non espone `get_window` (`Manager::manager()` è sealed); la `Window` per `popup` si ottiene scartando la `WebviewWindow`: `AsRef<Webview>` → `Webview::window()`.
- Ogni popup e ogni azione del menu lascia traccia in `panel.log`.

**Verificato dall'utente**: menu all'icona al destro, azioni coerenti col pannello, click sinistro intatto. **Lezione**: su questa beta il menu nativo dello status item è irrecuperabile dal lato sinistro, ma il popup manuale con ramo mouse-location di muda lo replica al 100% — e il diario di §8 aveva già tutte le coordinate per dirlo: bastava cercare il ramo giusto invece di arrendersi al menu attaccato.

## 15. Pubblicazione del repo: pulito l'albero non basta, conta la storia (26/10/2026)

**Contesto**: repo reso pubblico. Lo scrub dell'**albero** (hostname del portale reale → placeholder, lista di app menu-bar che rivelava l'IdP → genericizzata) non è sufficiente quando i dati sensibili vivono nella **storia**: `git log --all -S` mostrava l'hostname in 9 commit (2 anche nei messaggi) e l'identità git reale (nome + email aziendale) era su ogni commit di ogni branch.

**Scelta**: repo **nuovo** con root commit orfani (alberi ripuliti, identità `kidgvbs <noreply GitHub>` configurata a livello **repo**, così anche i commit futuri restano puliti senza toccare la config globale) invece di riscrittura filter-repo del vecchio. Il vecchio repo diventa `gp-bar-archived`: privato + `archived` GitHub (read-only), tutta la storia pre-scrub conservata lì — è il suo ruolo. Branch pubblicati: `main` + `solution-1-smappservice` (parcheggiata: da ribasare su main se ripresa).

**Lezione**: prima di pubblicare, controllare **storia** (`git log --all -S<termine>`, tutti i branch e i messaggi) e **identità** (`git log --format='%an <%ae>'`), non solo `grep` sull'albero corrente; il gateway FQDN non è mai entrato in git solo perché vive nella config fuori repo — la segregazione dei dati fuori dal repo è la difesa che funziona, lo scrub è la seconda linea.

## 16. Il gateway "connesso" non è quello selezionato: il cached-cookie bypassa `--gateway` (26/10/2026)

**Sintomo**: il pannello mostrava il gateway sbagliato accanto al portale — la selezione del picker sembrava ignorata.

**Diagnosi in due parti**:

1. **Il display diceva la VERITÀ**: il log del worker conteneva un solo `POST https://<fqdn>/ssl-vpn/getconfig.esp` e nessuna riga di selezione. Nei sorgenti gpclient (3.2.0): finché il cookie cached è valido, `try_cached_cookie` ritorna subito e gpclient riconnette a `stored.last_gateway` **ignorando `--gateway` del tutto** (mod.rs:234-238). Il flusso portale — l'unico che onora `--gateway`/`--auto-gateway` — parte solo a cookie scaduto. La selezione era una richiesta bypassata dal cookie.
2. **La catena di display era integra ma sorda a quel flusso**: `parse_gateway` (vpn.rs) sapeva solo le righe di selezione ("Connecting to the selected/only available gateway", "Auto-gateway: attempting"), che nel percorso cookie non esistono.

**Fix (backend, tre pezzi)**:

- `parse_gateway` → enum `GatewayMention`: `Selected { name, address }` (le tre righe note, ora parsando anche la forma Display di gpapi `name (address)` — primo token = nome, secondo tra parentesi = indirizzo) e `Host(fqdn)` dalla riga `POST https://<gw>/ssl-vpn/getconfig.esp` — **universale**, appare in entrambi i flussi. Esclusi con test dedicati il portale (`/global-protect/getconfig.esp`) e l'HIP (`/ssl-vpn/hipreportcheck.esp`).
- Config: `gateway_hosts: BTreeMap<addr, name>` (FQDN → nome). Seminata a mano con le 5 coppie dell'utente; **auto-appresa** da `learn_gateway_host` quando compare una riga `name (address)`.
- Handler del tailer: `Selected` → impara la coppia, osserva e harvesta il **nome**; `Host` → traduce via mappa (`gateway_display_name`), display del FQDN raw se sconosciuto, harvest **solo** se il nome è noto (mai l'FQDN). Zero modifiche frontend: `main.ts` renderizzava già `portal · gateway`.
- **La selezione ora vince**: `do_connect` azzera la reuse (`reuse_cookie && resolved_gateway.is_none()`) quando c'è un gateway esplicito (pick o preferred) — senza `--cookie-cache` gpclient passa dal flusso portale, che onora `--gateway`. Auto resta con la reuse (l'auto-gateway è ciò che il percorso cookie farebbe comunque). Nessuna modifica al wrapper root-owned: il flag `reuse` di `gp-admin start` esisteva già, era il caller che passava sempre 1.

**Conseguenza del bypass chiperrata, accettata**: col gateway esplicito il connect non è più silenzioso — niente re-auth via cookie, gpclient rifà l'autenticazione portale (può aprire il browser per le credenziali). È il prezzo per onorare la selezione.

## 17. Switch gateway a caldo: Reconnect ambra + le due trappole (26/10/2026)

**Feature**: mentre Connesso la select gateway resta **abilitata** (★/× restano congelati); un pick diverso dal tunnel attivo arma il pulsante in **Reconnect ambra** (`#ff9500`) → click = disconnect + connect sul nuovo pick. Il flag armed sopravvive agli eventi di stato mentre Connesso e si disarmare da solo appena la fase lascia Connected. Riselezionare il tunnel attivo (o Automatic che vi risolve) disarma e torna a Disconnect.

**Le due trappole trovate sul campo**:

1. **Il primo Reconnect era solo visivo** — race: `gp-admin stop` ritorna quando il lock è sparito, ma la fase nel **controller** passa a Disconnected via **poller** → il `connect` frontend subito dopo veniva rifiutato ("cannot connect while Disconnecting"). Fix frontend: il Reconnect polla `get_state` (250ms, timeout 15s) finché la fase non lascia Disconnecting, poi connette.
2. **Il colore non cambiava** — specificità CSS: `.status-block[data-phase="Connected"] ~ .controls .btn.primary` (4 classi + 1 attr) batteva `.btn.primary.reconnect` (3 classi). Fix: stesso selettore completo + `.reconnect`, così vince dopo nell'ordine.

**Lezioni**: (1) una command backend che ritorna non implica che lo **stato observable** sia già aggiornato — tra wrapper e poller c'è una finestra, e un client che concatena due comandi deve **aspettare l'osservazione**, non solo il ritorno; (2) quando una regola per fase e una per classe concorrono sullo stesso elemento, scrivere la seconda **dentro** il selettore della prima: la specificità si eredita, il colore no.


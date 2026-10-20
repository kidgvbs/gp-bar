# Recap — Nuovo client GUI per la VPN aziendale (GP Bar)

**Data:** 20 ottobre 2026
**Richiesta:** riscrivere un client GUI come GP Connect.app ma **senza il limite di 1 ora** sulla durata della connessione VPN, usando REA per l'analisi.
**Esito:** ✅ `GP Bar.app` costruito, avviato e in esecuzione nella menu bar. Nessun limite di tempo.

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
│   ├── src/config.rs                            # portali/porte persistiti (default: gp.imm.re)
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

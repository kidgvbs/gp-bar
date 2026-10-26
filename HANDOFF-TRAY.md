# GP Bar — incidente "tray morto" su macOS 27 beta: POST-MORTEM (CHIUSO)

Documento autonomo di sintesi. Diario completo (matrice, fatti, artefatti, comandi): `RECAP.md` §8.1–8.12.

**Esito: RISOLTO.** Non era il tray, non era il seed macOS, non era la firma: era un **deadlock nostro** che congelava l'intera app dal secondo secondo di vita.

---

## 1. Il sintomo (come si presentava)

Su macOS 27.0.1 beta (26A434) il lucchetto compariva nella barra menu, tooltip e animazione di pressione funzionavano, ma **nessun click faceva nulla**: niente pannello, niente menu, log stderr a 0 byte in ogni configurazione provata (8+ build). GP Connect e ogni altra app menu bar funzionavano sullo stesso seed.

## 2. La causa radice

`vpn.rs::update()` applicava la mutazione sotto `inner.lock()` e chiamava **`app.emit("vpn-state")` ancora dentro il guard**. L'emit di tauri invoca i listener Rust **sincronicamente sullo stesso thread**; il listener `vpn-state` (`main.rs`) chiamava `sync_tray_with_state()` → `controller.snapshot()` → **lo stesso mutex**. `std::sync::Mutex` non è rientrante:

```
tokio worker (poller, tick ogni 2s — update() emette SEMPRE, anche senza cambi)
  update() → inner.lock() … emit("vpn-state") [SOTTO LOCK]
    → listener → sync_tray_with_state() → snapshot() → inner.lock() ☠

main thread (appena il frontend carica la pagina e chiama get_state)
  WebURLSchemeTask → invoke get_state → snapshot() → inner.lock() ☠ (tenuto dal poller morto)
  → NSApplication run non torna mai al run loop → NESSUN evento servito (tray, finestra, X)
```

Quindi: il poller si auto-deadlockava al **primo tick** (~2s dal lancio) e il main thread lo raggiungeva nella tomba al primo `get_state`. Da lì in poi l'app era **totalmente inanimata** — e ogni test sul tray misurava un cadavere.

## 3. Perché sembrava il seed macOS (e non era)

- Il bug è **latente e identico da v0.1.0** (HEAD `9285742`), ma `Cargo.lock` è ignorato da git: il build originale risolse una tauri più vecchia; il rebuild di questa sessione ha preso **tauri 2.12.4**, dove emit→listener sincrono ha innescato il deadlock. L'aggiornamento del seed era **correlazione**.
- Tooltip/animazione funzionavano perché disegnati dal WindowServer (system-side).
- Senza finestra webview (`windows: []`) il tray **funzionava** (nessun load pagina → nessun `get_state` → main thread libero): il primo log con eventi della sessione (6 `show_panel`).
- La repro minimale ad-hoc (`~/Rust/tray-minimal`: tao 0.37 + tray-icon =0.25.0, bundle manuale, **zero tauri**) funzionava sul seed → **ipotesi "firma Apple Developer" (D) falsificata**: il seed consegna i click anche ai binari ad-hoc; sul sistema non esistono nemmeno identità Apple (solo CommandLineTools).
- Il log di quella minimale conferma che gli eventi overlay `TrayIconEvent` (Enter/Move/Leave/Click) **fluiscono regolarmente** sul seed: anche la teoria "TaoTrayTarget intercetta senza ricevere" era un artefatto del congelamento.

**Come è stata trovata:** `sample <pid>` con finestra visibile → main thread in `pthread_mutex_wait` dentro `WebURLSchemeHandler::startTask` → rebuild `--debug` con simboli → `atos`/stack simbolizzato → le funzioni nostre (`get_state` → `snapshot` da un lato; `spawn_status_poller` → `update` → `emit` → listener → `snapshot` dall'altro).

## 4. Il fix (due strati)

1. **`vpn.rs::update()`** — il guard del mutex viene **droppato prima dell'emit**; inoltre emette **solo se lo stato è cambiato** (niente eventi ridondanti a ogni tick).
2. **`main.rs`** — il listener `vpn-state` legge la fase **dal payload dell'evento** (`serde_json::from_str::<VpnState>(event.payload())`), non più via `snapshot()`: la rientranza è eliminata alla radice.

Verifica: `sample` → main thread nel run loop (`BlockUntilNextEvent`, zero mutex); finestra interattiva; click sinistro → pannello (conferma utente, PID 17865).

## 5. Stato del codice a fine incidente

- Config di produzione ripristinata: finestra `visible: false`, `set_activation_policy(Accessory)`, menu attached al tray, click sinistro → pannello, destro → menu nativo.
- Rimossi tutti gli esperimenti: `GPBarTrayTarget` (define_class objc2), rimozione subview (test A), target/action (test B), `setMenu(None)`, probe `TODO(debug)` (3), plist `LSUIElement.bak`, deps `objc2`/`objc2-app-kit` da `Cargo.toml`.
- `tray-icon` resta **pinnato a `=0.25.0`**: è la configurazione verificata sul campo; le teorie che motivavano il pin erano sbagliate, ma rilassarlo a 0.25.1 non è ancora stato testato esplicitamente (lavoro futuro, low priority).
- Identifier ripristinato a `dev.gpbar.app` (il cambio a `dev.gpbar.bar` era un esperimento di firma; la config non dipende dall'identifier: path fisso `gp-bar`).

## 6. Residui aperti

1. **Hardening sicurezza** (da analisi esterna validata — vedi RECAP §8.9): `umask` restrittivo per il log di connect (oggi `0644` root), validazione FQDN del portale in `config.rs`, verifica del path del binario via PID prima del `kill` privileged.
2. **Tracciare `Cargo.lock` in git** (lezione dell'incidente: il drift silenzioso delle dipendenze ha reso irriconoscibile la causa).
3. Rivalutare il pin `=0.25.0` di `tray-icon` dopo un test esplicito con 0.25.1.

## 7. Note operative (restano valide)

- Test sempre via `open "<bundle>" --stdout/--stderr <log>` (mai binario diretto: non crea lo status item); `pkill -f "GP Bar.app/Contents/MacOS/gp-bar"` prima di rilanciare.
- Re-sign dopo ogni build: `codesign --force --sign - --identifier dev.gpbar.app "src-tauri/target/release/bundle/macos/GP Bar.app"`.
- `sample <pid> 2` + build `--debug` + simboli sono lo strumento giusto per app "che non rispondono": **prima di ipotizzare guai al sistema, verificare che il processo sia vivo** (`sample` del main thread).
- La repro minimale vive in `~/Rust/tray-minimal` (fuori dal repo).

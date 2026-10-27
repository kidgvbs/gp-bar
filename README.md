# GP Bar

A macOS menu bar client for the GlobalProtect VPN, built around the free and
open-source [`gpclient`](https://github.com/yuezk/GlobalProtect-openconnect)
CLI. Connections last as long as the VPN portal allows — GP Bar adds **no
client-side time limits**.

## How it works

GP Bar never implements the VPN protocol itself and contains no licensing
machinery. It orchestrates the open-source components, exactly like running
the CLI by hand:

```
sudo gpclient connect vpn.example.com --browser --auto-gateway --cookie-cache
```

* **Connect** — GP Bar asks a **root-owned helper** to start the VPN worker
  (one `sudo -n`, no dialog, no credentials in the app — see
  [Privileges](#privileges)). Gateway selection mirrors the CLI: the panel's
  "Auto" maps to `--auto-gateway` (with an optional preferred gateway), a
  picked gateway passes `--gateway <name>`. The worker runs as root, born
  from **launchd** so it stays signalable (see the incident notes in
  `RECAP.md`); its SSO login opens in your default browser.
* **Status** — `gpclient` writes its PID to `/var/run/gpclient.lock` once the
  tunnel is up and removes it on disconnect; GP Bar watches that file to know
  whether the VPN is up (even if it was started from a terminal, or before GP
  Bar launched).
* **Live activity** — gpclient runs with `--log-format json`, so its records
  (`{timestamp, level, target, message}`) stream into the panel in real time.
* **Disconnect / Cancel** — the helper runs `gpclient disconnect` for a
  graceful teardown, verified against the lock file. Failures are loud; GP Bar
  **never SIGKILLs** the worker (a killed worker skips the vpnc-script
  teardown and leaves macOS DNS pointing at VPN resolvers).

Because the root `gpclient` worker belongs to a one-shot launchd job, the VPN
survives GP Bar restarts and logouts of the app.

## Privileges

Creating the tunnel requires root. GP Bar never handles a password itself:

* The app only ever runs
  `sudo -n /Library/PrivilegedHelperTools/gp-bar/admin <op>` — a
  **root-owned wrapper** with a fixed set of operations
  (`selftest | start <portal> <reuse 0|1> | stop | cancel | repair`) and
  self-validated arguments. It generates the worker's launchd job itself:
  **no caller-controlled paths, no eval, no user-writable staging files**.
* The one-time setup installs, all root-owned under
  `/Library/PrivilegedHelperTools/gp-bar` (a prefix whose every parent
  directory is system-owned):

  * `admin` — the wrapper (0755),
  * `bin/gpclient` — a **root-owned copy** of the worker binary, resolved
    from the Homebrew locations, plus `Frameworks/*.dylib` — the
    `@executable_path` dylibs gpclient links (libopenconnect and its
    closure), copied layout-preserving so the whole load chain is
    root-owned too. The wrapper executes these only, never anything under
    `/opt/homebrew`, `/usr/local` or an app bundle, and refuses to run
    unless the file, its realpath and every parent directory are
    root-owned,
  * `GP Connect Login.app` and `bin/gpauth` — gpauth (the SAML drop-in,
    resolved by gpclient as a sibling of its own executable) is
    **Hardened Runtime**: its code signature validates against its bundle
    context, and the kernel kills a bare Mach-O copy at exec. So the whole
    bundle is copied root-owned and `bin/gpauth` is a relative symlink into
    it — the exec happens in bundle context, exactly like upstream.
  * the sudoers grant, written atomically: a dotted, inert temp file in
    `/etc/sudoers.d`, validated on its own with `visudo -cf`, then renamed
    into place. A malformed file can never be live.

  ```sh
  sudo bash "/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh"
  ```

  Run it in **Terminal**, where the admin prompt offers Touch ID (the same
  `pam_tid` as a regular `sudo`). After the setup,
  connect/disconnect/cancel need **no prompt and no password, ever**.

* The connect log (`gpclient-connect.log`, which the panel tails live) is
  written by the ROOT worker into the root-owned, world-readable
  `/Library/Logs/gp-bar/` — never into `~/Library` (a user symlink there
  would let any user process truncate system files as root). The app only
  reads it, as the user.

* Residual risk, stated honestly: anything running as your user can toggle
  the VPN on/off through the wrapper, and `repair` restores/clears the
  manual DNS of network services — but only from the pre-VPN snapshot the
  wrapper records at connect time (same service only; no snapshot or a
  service mismatch = loud refusal, never a guess). IPv6 manual DNS is
  snapshotted and restored too. The root exec chain itself is closed
  on this machine: wrapper, binaries and their `@executable_path` dylibs
  are all root-owned copies outside Homebrew. If a future gpclient build
  were to link user-writable dylibs by absolute path, that residual would
  re-open — re-running the setup script after every `brew upgrade`
  refreshes the copies, and the wrapper's ownership checks fail loudly if
  anything drifts.

An alternative elevation design (SMAppService LaunchDaemon + XPC, the
macOS-blessed road) exists on the `solution-1-smappservice` branch: the daemon
is registered from the app with a single system prompt, then everything runs
promptless over XPC. It works, but on this macOS beta (27.0.1, 26A434) the
Background Task Management layer proved unstable around it — see
`docs/btm-27.0.1-beta.md` for the open observations. The sudoers design above
is the one in production.

## Install

```sh
brew install gpclient   # the CLI backend (GPL-3.0), if not already present
npm install
npm run tauri build     # produces src-tauri/target/release/bundle/macos/GP Bar.app
```

Re-sign the release bundle (it comes out linker-signed with a derived
identifier) and copy it somewhere stable, then run the one-time setup from
Terminal:

```sh
codesign --force --sign - --identifier dev.gpbar.app \
  "src-tauri/target/release/bundle/macos/GP Bar.app"
cp -R "src-tauri/target/release/bundle/macos/GP Bar.app" ~/Applications/
sudo bash "$HOME/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh"
```

## Usage

The padlock lives in the menu bar:

* **Left click** — open the GP Bar panel: VPN status, connect/disconnect,
  live activity log, portal and gateway pickers. The panel shows the
  **live gateway** next to the portal (`portal · gateway`); while
  connected, picking a different gateway arms an amber **Reconnect**
  button that swaps the tunnel to it (disconnect + connect through the
  pick).
* **Right click** — open the tray menu: Connect / Disconnect / Quit (each
  enabled per VPN state; the menu is popped up by hand, never attached to
  the status item — an attached one swallows left clicks on the macOS 27
  beta, see `RECAP.md` §8 and §14).

The panel needs the selected portal seeded before Connect works; picking a
portal in the picker (or adding one) enables it. Errors show in the panel
instead of vanishing.

## Development

```sh
npm run tauri dev   # vite + cargo debug run
cargo test           # unit tests (portal charset, hint mapping, log parsing, icons)
npm run icons        # regenerate src-tauri/icons/* (drawn in scripts/gen-app-icon.mjs)
npm run tauri build  # release bundle
```

After changing `src-tauri/scripts/gp-admin` — or after `brew upgrade` /
a GP Connect update (the source of the `gpclient` binary) — re-run the
setup script to refresh the installed wrapper and the root-owned binary
copies (it is idempotent, and it also performs the end-to-end selftest).

The tray icon is built through `tauri::tray`, so `tray-icon` comes in only as
a transitive dependency of tauri itself (no direct pin; GP Bar never attaches
a tray menu).

## Configuration

`~/Library/Application Support/gp-bar/config.json`:

```json
{
  "portals": ["vpn.example.com"],
  "selected": "vpn.example.com",
  "reuse_cookie": true,
  "gpclient_path": null,
  "gateways": [],
  "preferred_gateway": null,
  "selected_gateway": null,
  "liquid_glass": null
}
```

* `reuse_cookie` — passes `--cookie-cache` so gpclient reuses the portal
  session cookie; fewer browser logins while the session stays valid.
* `gpclient_path` — force a specific `gpclient` binary for the version
  display only (dev setups, non-standard installs); `null` resolves the
  version from the root-owned copy the admin wrapper actually runs
  (`/Library/PrivilegedHelperTools/gp-bar/bin/gpclient`), falling back to
  Homebrew autodetect. The worker itself always executes that root-owned
  copy: a root-owned component must not run binaries from
  user-configurable paths.
* `gateways` — entries of the gateway picker: added from the panel or
  harvested from connect logs, never fetched over the network. GP Bar makes
  no network calls of its own (a portal-side gateway fetch was removed —
  see `RECAP.md`).
* `preferred_gateway` / `selected_gateway` — the picker's "Auto"
  (`selected_gateway: null`) connects to the preferred gateway when one is
  set, else lets gpclient choose (`--auto-gateway`); a picked gateway forces
  `--gateway <name>` — and disables the cached-cookie reuse for that
  connect, because while the cookie is valid gpclient reconnects to the
  last gateway and ignores `--gateway` entirely.
* `gateway_hosts` — reverse display map (gateway FQDN → picker name). The
  cached-cookie re-auth path logs no gateway *name*, only the FQDN; GP Bar
  translates it through this map to show the picker name. Seeded by hand,
  learned automatically whenever a `name (address)` selection line appears
  in the connect log.
* `liquid_glass` — `null` = auto: native liquid-glass panel backdrop on
  macOS 26+, CSS vibrancy below; `false` forces the CSS backdrop everywhere.

## Licensing

GP Bar is original code under the MIT license (see `LICENSE`). It is a
separate program that invokes the GPL-3.0 `gpclient` binary as a child
process; it does not link or distribute gpclient, and it contains no code,
assets, or UI from the proprietary GP Connect application.
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
sudo gpclient connect gp.imm.re --browser --auto-gateway --cookie-cache
```

* **Connect** — GP Bar asks a **root-owned helper** to start the VPN worker
  (one `sudo -n`, no dialog, no credentials in the app — see
  [Privileges](#privileges)). The worker runs as root, born from **launchd**
  so it stays signalable (see the incident notes in `RECAP.md`); its SSO
  login opens in your default browser.
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

* The app only ever runs `sudo -n /usr/local/lib/gp-bar/admin <op>` — a
  **root-owned wrapper** with a fixed set of operations
  (`selftest | start <portal> <reuse 0|1> | stop | cancel`) and self-validated
  arguments. It generates the worker's launchd job itself: **no
  caller-controlled paths, no eval, no user-writable staging files**.
* The one-time setup installs the wrapper (root-owned, 0755) and grants
  passwordless sudo on that exact path only, via `/etc/sudoers.d/gp-bar`:

  ```sh
  sudo bash "/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh"
  ```

  Run it in **Terminal**, where the admin prompt offers Touch ID (the same
  `pam_tid` as a regular `sudo`). After the setup, connect/disconnect/cancel
  need **no prompt and no password, ever**.
* Residual risk, deliberate and documented: anything running as your user can
  toggle the VPN on/off through the wrapper — nothing more.

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
  live activity log, portal picker.
* **Right click** — open the tray menu: Connect / Disconnect / Quit.

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

After changing `src-tauri/scripts/gp-admin`, re-run the setup script to
refresh the installed wrapper (it is idempotent).

`tray-icon` is pinned to `=0.25.0` as the configuration verified on this
machine. (The early "menu attach" theories about the macOS 27 beta turned out
to be red herrings — the app was frozen by a poller deadlock, since fixed in
`vpn.rs`; see `RECAP.md` §8.12. The pin stays only until 0.25.1 is explicitly
re-verified.)

## Configuration

`~/Library/Application Support/gp-bar/config.json`:

```json
{
  "portals": ["gp.imm.re"],
  "selected": "gp.imm.re",
  "reuse_cookie": true,
  "gpclient_path": null
}
```

* `reuse_cookie` — passes `--cookie-cache` so gpclient reuses the portal
  session cookie; fewer browser logins while the session stays valid.
* `gpclient_path` — force a specific `gpclient` binary for the version
  display; `null` auto-detects the Homebrew locations. (The root helper
  always resolves gpclient from the fixed Homebrew paths: a root-owned
  component must not execute user-configured paths.)

## Licensing

GP Bar is original code under the MIT license (see `LICENSE`). It is a
separate program that invokes the GPL-3.0 `gpclient` binary as a child
process; it does not link or distribute gpclient, and it contains no code,
assets, or UI from the proprietary GP Connect application.
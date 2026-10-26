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

* **Connect** — asks the `gp-helper` LaunchDaemon (root, registered once
  with SMAppService) over XPC to start `gpclient` as root, detached. The
  SSO login opens in your default browser.
* **Status** — `gpclient` writes its PID to `/var/run/gpclient.lock` once the
  tunnel is up and removes it on disconnect; GP Bar watches that file to know
  whether the VPN is up (even if it was started from a terminal, or before GP
  Bar launched).
* **Live activity** — gpclient runs with `--log-format json`, so its records
  (`{timestamp, level, target, message}`) stream into the panel in real time.
* **Disconnect** — asks the daemon to run `gpclient disconnect` for a
  graceful teardown and verifies it against the PID lock file.

Because the root `gpclient` worker runs detached (reparented to launchd),
the VPN survives GP Bar restarts and logouts of the app.

## Privileges

Creating the tunnel requires root. GP Bar ships a small `gp-helper`
LaunchDaemon and registers it **once** through `SMAppService`: on the
first connect macOS shows a single system authorization prompt, and from
then on every connect/disconnect/cancel runs promptless over XPC (the
mach service `dev.gpbar.helper`).

* The daemon only executes fixed operations (`start`/`stop`/`cancel`/
  `selftest`) with scalar, re-validated arguments — no paths travel over
  the wire, and `gpclient` is always spawned directly (`Command`, never
  `sh -c`).
* The daemon plist lives inside the app bundle
  (`Contents/Library/LaunchDaemons/dev.gpbar.helper.plist`) and is
  written by the app at registration time, so the bundle must live in a
  **stable, user-writable** location (`~/Applications` is ideal).
* Because `Program` in the plist is an absolute path into the bundle, a
  rebuild that keeps the same path does **not** need re-registration:
  just re-copy the bundle over the old one (the finalize step re-signs
  it) and relaunch. Re-register only if the bundle moves.
* Registration requires the whole bundle to be code-signed — the
  `build:bundle` script ad-hoc re-signs it with the bundle identifier.

Dev cycle:

```sh
npm run build:bundle               # tauri build + sidecar + ad-hoc re-sign
rm -rf ~/Applications/"GP Bar.app" # optional: clean copy
cp -R "src-tauri/target/release/bundle/macos/GP Bar.app" ~/Applications/
```

Verify the daemon after the first connect:

```sh
sudo launchctl print system/dev.gpbar.helper   # state, pid (root), mach service
```

To unregister (e.g. after moving the bundle): run GP Bar's binary with the
daemon gone — `SMAppService::unregister` is exercised by the code path in
`src-tauri/src/helper.rs` when the plist goes stale on the next connect.

## Install

```sh
brew install gpclient   # the CLI backend (GPL-3.0), if not already present
npm install
npm run tauri build     # produces src-tauri/target/release/bundle/macos/GP Bar.app
```

Drag `GP Bar.app` into `/Applications` and launch it; its icon appears in the
menu bar.

## Usage

The padlock lives in the menu bar:

* **Left click** — open the GP Bar panel: VPN status, connect/disconnect,
  live gateway log, portal picker.
* **Right click** — open the tray menu: Connect / Disconnect / Quit.

## Development

```sh
npm run tauri dev   # vite + cargo debug run (helper registration needs the bundle: dev runs unprivileged)
cargo test           # unit tests (protocol validation, log parsing, icons)
npm run icons        # regenerate src-tauri/icons/* (drawn in scripts/gen-app-icon.mjs)
npm run build:bundle # release bundle + gp-helper sidecar + ad-hoc re-sign
```

The re-sign is part of `build:bundle` (`scripts/finalize-bundle.mjs`):
the release bundle comes out linker-signed with a derived identifier that
does not match the bundle id, and both the bundle and the `gp-helper`
sidecar must be signed for `SMAppService` registration.

`tray-icon` is pinned to `=0.25.0` as the configuration verified on this
machine. (The early "menu attach" theories about the macOS 27 beta turned out
to be red herrings — the app was frozen by a poller deadlock, since fixed in
`vpn.rs`; see `RECAP.md` §8.12. The pin stays only until 0.25.1 is explicitly
re-verified.)

## Configuration

`~/Library/Application Support/gp-bar/config.json`:

```json
{
  "portals": ["vpn.example.com"],
  "selected": "vpn.example.com",
  "reuse_cookie": true,
  "gpclient_path": null
}
```

* `reuse_cookie` — passes `--cookie-cache` so gpclient reuses the portal
  session cookie; fewer browser logins while the session stays valid.
* `gpclient_path` — force a specific `gpclient` binary; `null` auto-detects
  the Homebrew locations.

## Licensing

GP Bar is original code under the MIT license (see `LICENSE`). It is a
separate program that invokes the GPL-3.0 `gpclient` binary as a child
process; it does not link or distribute gpclient, and it contains no code,
assets, or UI from the proprietary GP Connect application.

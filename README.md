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

* **Connect** — shows the native macOS administrator password dialog
  (`osascript ... with administrator privileges`), then starts
  `gpclient` as root, detached. The SSO login opens in your default browser.
* **Status** — `gpclient` writes its PID to `/var/run/gpclient.lock` once the
  tunnel is up and removes it on disconnect; GP Bar watches that file to know
  whether the VPN is up (even if it was started from a terminal, or before GP
  Bar launched).
* **Live activity** — gpclient runs with `--log-format json`, so its records
  (`{timestamp, level, target, message}`) stream into the panel in real time.
* **Disconnect** — runs `gpclient disconnect` for a graceful teardown
  (one admin prompt).

Because the root `gpclient` worker runs detached (reparented to launchd),
the VPN survives GP Bar restarts and logouts of the app.

## Privileges

Creating the tunnel requires root. The current version shows the native
admin dialog once per connect and once per disconnect — the same prompts you
would get with `sudo` in a terminal. A follow-up version will add an optional
launchd daemon so the password is only needed at install time.

## Install

```sh
brew install gpclient   # the CLI backend (GPL-3.0), if not already present
npm install
npm run tauri build     # produces src-tauri/target/release/bundle/macos/GP Bar.app
```

Drag `GP Bar.app` into `/Applications` and launch it; its icon appears in the
menu bar.

## Development

```sh
npm run tauri dev   # vite + cargo debug run
cargo test          # unit tests (shell escaping, log parsing, icons)
npm run icons       # regenerate src-tauri/icons/* (drawn in scripts/gen-app-icon.mjs)
```

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
* `gpclient_path` — force a specific `gpclient` binary; `null` auto-detects
  the Homebrew locations.

## Licensing

GP Bar is original code under the MIT license (see `LICENSE`). It is a
separate program that invokes the GPL-3.0 `gpclient` binary as a child
process; it does not link or distribute gpclient, and it contains no code,
assets, or UI from the proprietary GP Connect application.

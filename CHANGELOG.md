# Changelog

## 1.0.0 — 26 October 2026

First official release: a macOS menu-bar client for the GlobalProtect
VPN, built around the open-source
[`gpclient`](https://github.com/yuezk/GlobalProtect-openconnect) CLI.

### Added

- **Menu-bar tray + click-anchored popover panel** — VPN status,
  connect/disconnect/cancel, portal and gateway pickers, live activity
  log. Panel positioning is verified on single- and mixed-DPI desks
  (anchor under the tray icon on whichever display was clicked).
- **Right-click tray menu** (Connect / Disconnect / Quit), each entry
  enabled per VPN state. The menu is popped up by hand and never
  attached to the status item: on the macOS 27 beta an attached menu
  makes the system swallow left clicks.
- **Native liquid-glass panel backdrop** on macOS 26+ (CSS vibrancy
  fallback below, or forced via `liquid_glass: false`).
- **Network health check + DNS repair** — detects VPN resolver
  leftovers after a crash and restores DHCP DNS through the privileged
  helper (refuses while the VPN is legitimately up).
- **Privilege design**: a root-owned, fixed-operation wrapper
  (`scripts/gp-admin`) with a one-time passwordless sudoers grant on
  that exact path — no prompts after setup, no credentials in the app,
  no caller-controlled paths. The VPN worker is born from a one-shot
  launchd job: it survives app restarts and logouts, stays signalable
  (graceful teardown, never SIGKILL).

### Notes

- The app makes **no network calls of its own**: everything goes
  through `gpclient` (gateway entries come from the picker and from
  connect-log harvesting — a portal-side gateway fetch was removed in
  development after it caused repeated failed portal authentications).
- Bundles are ad-hoc signed and **not notarized**: clear the
  quarantine attribute after downloading, or build from source.
- `RECAP.md` carries the full incident diary, including the macOS 27
  beta workarounds this release is built around.

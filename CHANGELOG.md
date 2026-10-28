# Changelog

## 1.2.0 — 28 October 2026

Hardening round 2: a second full code review (16 findings) implemented
in four user-verified waves — security/CI, structure, typing/idioms,
and async correctness.

### Async correctness

- Async completions carry the identity of the attempt that spawned
  them (`AttemptId`): a completion arriving for a superseded attempt
  is a no-op by construction. The race tests were written first, red,
  then made green.
- Privileged operations are serialized end-to-end. **Cancelling during
  the first second of a connect now actually cancels** — the old code
  could lose the cancel while the start was still in flight, and the
  VPN (and the SSO browser) came up anyway.
- Disconnect and Cancel return the real error when teardown fails (the
  panel shows it) instead of silently succeeding; Cancel is only
  valid while connecting.
- The wrapper bounds its own stop path (20s worst case) inside the
  app's 30s admin timeout, inside launchd's 60s `ExitTimeOut` —
  coherent, documented arithmetic, with an honest note that
  kill-on-drop only reaps the sudo parent.
- `launchctl bootstrap/bootout` replace `load/unload -w`, with an
  `enable` bridge for machines whose last stop was made by the old
  wrapper (`unload -w` leaves a persistent *disabled* override that
  bootstrap honors silently).
- The connect-log tailer starts at EOF: a previous session's log is
  no longer replayed at launch, and gateway harvesting is gated to the
  active attempt.

### Structure & typing

- Config mutations go through a single chokepoint (`update_config`:
  clone → apply → save → commit only if the save succeeded), fixing
  two background call sites that mutated before saving; an unchanged
  config is never re-written.
- Reconnect is one backend command: the panel's polling loop is gone
  and the armed flag comes from the backend state payload.
- Typed admin/action errors on the wire (`{kind, detail}`) with
  byte-identical user-facing texts; the panel can now distinguish
  "helper not installed" from "operation failed" and "timed out".
- Panel positioning is a pure function tested with the real numbers
  of the mixed-DPI incident; state is published over a watch channel,
  so tray sync and frontend emission never run under a lock.
- Honest config load: a file the app cannot read is never overwritten
  (saves become no-ops and the panel says why); corrupt files are
  preserved with a timestamp so nothing is lost twice.
- `libc::proc_pidpath` behind a `Pid` newtype (parse, don't validate);
  mutating commands moved off the main thread (`spawn_blocking`).

### Security & CI

- `setup-admin.sh` verifies the signature and Team ID of the source
  `gpclient`/`gpauth` before copying them into the root prefix.
- The wrapper documents — and its selftest enforces — that the default
  connect path uses the vpnc script embedded in `gpclient` itself
  (installing an external one would disable the native handler).
- DNS-repair refusals name the servers that were recorded.
- CI: `--locked` everywhere, SHA-pinned actions, ubuntu-24.04, and a
  macOS job — clippy and tests now also run on the platform the app
  ships to.

### Notes

- The wrapper changed in this release: re-run `setup-admin.sh` once
  after updating so the deployed root copy matches.
- Tests: 44 (21 before the hardening effort started).

## 1.1.0 — 27 October 2026

Security, robustness and hygiene hardening from a full code review.

### Security (privileged helper)

- **Root-owned toolchain**: `setup-admin.sh` now copies the whole
  `gpclient`/`gpauth` toolchain (worker binary, frameworks, and the
  complete `GP Connect Login.app` bundle) into a fixed root-owned prefix
  (`/Library/PrivilegedHelperTools/gp-bar/`), and `gp-admin` verifies at
  runtime that everything it executes is owned by root — a tampered or
  user-writable path aborts the operation. `gpauth` lives as a relative
  symlink into the copied bundle: it is a Hardened-Runtime binary whose
  signature is validated in bundle context, and a bare file copy is
  CS-killed with SIGKILL at exec.
- **DNS repair refuses to guess**: the snapshot is keyed to the network
  service it was taken on; a repair on a different service or with no
  snapshot is refused with exact instructions instead of restoring.
  Reconnecting over a dirty state no longer poisons the snapshot, and
  IPv6 resolvers are snapshotted and restored alongside IPv4. A manual
  DNS equal to the pre-VPN snapshot is treated as the user's own, not
  stale.
- **Content Security Policy** on the panel webview.

### Robustness

- Atomic config persistence (temp file + rename, corrupt file
  preserved as `.corrupt`), admin operations run under a timeout, the
  connect-log tailer survives truncation and handles UTF-8 line
  splits, worker liveness is checked by PID path (not just PID reuse),
  and sudo error diagnosis is locale-independent.

### State machine

- Connect/disconnect transitions are atomic (TOCTOU-safe), a
  mid-connect second request is rejected instead of racing, and the
  tray offers a **Reconnect** action for hot gateway switches. A
  `Reconnect` after a gateway switch now works (was broken by the
  gpauth bundle issue above).

### Fixed

- Inline command errors in the panel (auto-hide after 6s) instead of
  silent failures; double-click connect guard; stale-DNS banner no
  longer fires on the user's own manual resolvers.

### Changed

- Logging moved to `tracing`-style leveled output via
  `tauri-plugin-log` (same `panel.log` path, 512 KiB rotation, keep 2);
  positioning/tray forensics stay in the file at debug level.
- Dead code removed (`worker_pid` bookkeeping, vestigial panel pinning,
  redundant `tray-icon` direct dependency, `chrono`); Rust edition 2024.
- **CI** (GitHub Actions): rust fmt/clippy `-D warnings`/test,
  shellcheck on the two scripts, tsc + vite build on every push and PR.

### Notes

- The npm lockfile now resolves against the public registry (a project
  `.npmrc` pins it): a machine configured with an internal mirror
  produced lockfile URLs GitHub runners cannot reach.

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

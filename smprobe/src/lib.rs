//! SMProbe v2 — a purpose-built probe for macOS **Background Task Management
//! (BTM)** stability on macOS 27.x, plus the XPC echo daemon it talks to.
//!
//! # What this crate is for
//!
//! GP Bar's parked "solution 1" registers a privileged LaunchDaemon through
//! `SMAppService` and talks to it over XPC. On macOS 27.0.1 beta (26A434) the
//! mechanism showed four anomalies (see `docs/btm-27.0.1-beta.md` in the main
//! checkout):
//!
//! * **A** — `register()` returns `EPERM` with no prompt when the caller is an
//!   Accessory-policy app. Avoided **by construction**: the probe bundle has no
//!   `LSUIElement` and never touches activation policies, so it defaults to
//!   Regular — the known-good path validated on 21/10.
//! * **B** — `-67054` "Codesigning failure loading plist" when the daemon
//!   plist is written after the bundle is signed. Avoided **by construction**:
//!   the plist is generated at build time by `make-plist` and sealed into the
//!   bundle's ad-hoc signature (verified in `scripts/build-probe.sh`).
//! * **C** — a valid registration spontaneously turns `NotFound` hours later.
//!   This is what the sampler detects: it records `SMAppService.status()` plus
//!   one XPC ping every 5 minutes to `~/Library/Logs/smprobe.log`, so a flip
//!   gets a timestamp without anyone watching.
//! * **D** — after `launchctl bootout`, re-register reports `Enabled` but the
//!   job never starts again (`active count = 0`). The daemon is deliberately
//!   on-demand with an idle-exit watchdog, so every 5-minute ping forces
//!   launchd to start the job fresh — the exact mechanism that died.
//!
//! A similar case surfaced publicly on macOS 27.0 release (exelban/stats
//! issue #3716: status `.notFound` + `register()` EPERM after an app
//! auto-update), which is why this probe targets macOS 27.2 (26B123).
//!
//! # macOS only, by design
//!
//! This crate uses `SMAppService`, libxpc and libdispatch directly. It is
//! deliberately free of `#![cfg_attr]`/`#[cfg(target_os)]` dance: it is
//! macOS-only by construction, lives on the parked `solution-1-smappservice`
//! branch as a standalone crate outside any workspace, and never enters the
//! Linux CI. It does not pretend to compile anywhere else.
//!
//! # Human-in-the-loop rule
//!
//! Nothing in this crate self-registers: building and the unit tests never
//! touch `SMAppService`. Registration/unregistration is a *human* action, run
//! manually per `PROTOCOL.md` (the system prompt with Touch ID is part of the
//! experiment). The agent workflow builds and verifies the bundle; the human
//! runs it.
//!
//! # Review discipline implemented from day one
//!
//! The XPC layer is the first live validation of the pattern the real
//! Fase-2 daemon will use, so it applies the findings of
//! `gp-bar-review-smappservice.md` from the start:
//!
//! * **B7** — every C-callback body is wrapped in `catch_unwind`; nothing that
//!   runs inside an XPC handler can panic (NUL bytes are stripped, never
//!   `expect`ed into an abort).
//! * **B8** — no safe function accepts a raw pointer. Owned handles are
//!   RAII types (`XpcObject`, `XpcConnection`); objects the runtime lends us
//!   are borrowed views (`XpcEvent`) created in one audited place.
//! * **B9** — handler blocks are not leaked: libxpc retains its own copy, and
//!   our `RcBlock` is dropped right after each `xpc_*` call that takes it.
//! * **B11** — no module-level `#[allow(dead_code)]`: the shared code is a
//!   library consumed by both binaries, the daemon's main loop is
//!   `dispatch_main()`, and every test can fail.
//!
//! # Layout
//!
//! ```text
//! smprobe/src/
//! ├── lib.rs       this documentation and the module wiring
//! ├── identity.rs  bundle/service identifiers and paths (the one place)
//! ├── proto.rs     wire protocol: keys, the ping decision, plist generation
//! ├── status.rs    SMAppServiceStatus numeric decode, defensive
//! ├── smapp.rs     the SMAppService calls (register/unregister/status)
//! ├── xpc.rs       RAII libxpc bindings (B8/B9/B10)
//! ├── client.rs    the ping client (async reply + timeout, never hangs)
//! ├── sampler.rs   the sampler log-line format (the anomaly-C detector)
//! ├── peer.rs      bundle-owner peer policy (pure decision + stat)
//! ├── time.rs      epoch -> RFC3339 UTC, no chrono dependency
//! └── tail.rs      last-N-lines log tail
//! ```

pub mod client;
pub mod identity;
pub mod peer;
pub mod proto;
pub mod sampler;
pub mod smapp;
pub mod status;
pub mod tail;
pub mod time;
pub mod xpc;

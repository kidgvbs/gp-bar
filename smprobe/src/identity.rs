//! The single source of truth for every identifier and path the probe uses.
//!
//! Identity rules (RECAP.md §11-12, paid for on 21-22/10):
//!
//! * The bundle id is **unique** (`dev.gpbar.smprobe`): it is never shared
//!   with the main app (`dev.gpbar.app`) or the parked S1 app
//!   (`dev.gpbar.s1`). A shared id made SMAppService/BTM resolve the caller
//!   ambiguously and registrations failed silently.
//! * The daemon label and the Mach service name are the same string, and
//!   both binaries look them up here, so the plist, the daemon listener and
//!   the client can never drift apart.

use std::ffi::CStr;

/// Bundle identifier of `SMProbe.app` (also used as the codesign
/// `--identifier`). Unique to the probe; never reuse another app's id.
pub const BUNDLE_IDENTIFIER: &str = "dev.gpbar.smprobe";

/// Name of the app binary inside `Contents/MacOS/` (CFBundleExecutable).
pub const APP_EXECUTABLE: &str = "smprobe";

/// Name of the daemon binary inside `Contents/MacOS/`.
pub const DAEMON_EXECUTABLE: &str = "smprobe-helper";

/// LaunchDaemon label == Mach service name the daemon listens on and the
/// client connects to.
pub const SERVICE: &CStr = c"dev.gpbar.smprobe.helper";

/// Same as [`SERVICE`], for places that need a `&str` (plist generation).
pub const SERVICE_STR: &str = "dev.gpbar.smprobe.helper";

/// File name of the LaunchDaemon plist inside
/// `SMProbe.app/Contents/Library/LaunchDaemons/`, as SMAppService expects it.
pub const DAEMON_PLIST_NAME: &str = "dev.gpbar.smprobe.helper.plist";

/// Sampler log, written by the app (which runs as the user, so the user's
/// home is the right place; the "root never writes into user-writable
/// paths" rule applies to the daemon side only).
pub const SAMPLER_LOG_RELATIVE: &str = "Library/Logs/smprobe.log";

/// Daemon stdout/stderr, opened by launchd as root. `/Library/Logs/gp-bar/`
/// is the root-owned directory the production toolchain already uses
/// (review S1/B3: no user-writable paths for root logs).
pub const DAEMON_LOG: &str = "/Library/Logs/gp-bar/smprobe-helper.log";

/// How often the sampler records one line (status + one ping).
pub const SAMPLER_PERIOD_SECS: u64 = 5 * 60;

/// The daemon exits after this many consecutive seconds with zero open
/// peer connections. On-demand only: with the daemon gone, every ping makes
/// launchd start the job fresh — continuous validation of the mechanism
/// that died in anomaly D.
pub const DAEMON_IDLE_EXIT_SECS: u64 = 60;

/// How long the client waits for the ping reply before giving up (the
/// daemon may be cold-starting via launchd).
pub const PING_TIMEOUT_SECS: u64 = 30;

/// Default number of tail lines printed by `smprobe log`.
pub const LOG_TAIL_DEFAULT: usize = 100;

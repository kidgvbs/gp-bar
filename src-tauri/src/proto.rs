//! The GP Bar ↔ gp-helper XPC protocol: message keys, operation names and
//! the small pieces of *policy* both sides must agree on.
//!
//! Shared between the app (`helper.rs`) and the daemon (`bin/gp-helper.rs`)
//! so the wire format lives in exactly one place. Everything here is pure
//! and unit-testable.
//!
//! Rules:
//!
//! * The daemon executes only fixed operations with scalar arguments.
//!   No paths travel over the wire: the daemon resolves `gpclient` and the
//!   log location itself, from the console user.
//! * The portal is re-validated daemon-side with the same charset as
//!   `add_portal` in main.rs (defense in depth: the app validates first,
//!   but the daemon trusts nothing).

// The app and the daemon each use a different subset of this module.
#![allow(dead_code)]

use std::{
  ffi::CStr,
  path::{Path, PathBuf},
};

/// The Mach service name both the LaunchDaemon plist (`MachServices`) and
/// the app-side client look up.
pub const SERVICE: &CStr = c"dev.gpbar.helper";

/// The LaunchDaemon plist file name inside
/// `<GP Bar.app>/Contents/Library/LaunchDaemons/`, as SMAppService expects
/// it (see `helper::register`).
pub const DAEMON_PLIST: &str = "dev.gpbar.helper.plist";

// -- message keys ----------------------------------------------------------------

pub const KEY_OP: &CStr = c"op";
pub const KEY_PORTAL: &CStr = c"portal";
pub const KEY_REUSE_COOKIE: &CStr = c"reuse_cookie";
pub const KEY_OK: &CStr = c"ok";
pub const KEY_ERROR: &CStr = c"error";
/// PID of the spawned gpclient worker (`start` reply).
pub const KEY_PID: &CStr = c"pid";
/// uid of the daemon (`selftest` reply): 0 proves the whole chain.
pub const KEY_UID: &CStr = c"uid";

// -- operations --------------------------------------------------------------------

pub const OP_START: &str = "start";
pub const OP_STOP: &str = "stop";
pub const OP_CANCEL: &str = "cancel";
pub const OP_SELFTEST: &str = "selftest";

// -- shared policy -----------------------------------------------------------------

/// The PID lock file gpclient maintains: written once the tunnel is up,
/// removed on disconnect. World-readable, so both the unprivileged app
/// (status poller) and the root daemon (stop verification) can watch it.
pub const LOCK_FILE: &str = "/var/run/gpclient.lock";

/// How long the daemon waits for the worker's own teardown after
/// `gpclient disconnect` (and after SIGTERM on cancel) before failing loud.
pub const GRACEFUL_STOP_TIMEOUT_SECS: u64 = 10;

/// Portal charset, identical to `add_portal` in main.rs. Re-checked in the
/// daemon so a malicious client cannot smuggle shell metacharacters or
/// traversal segments into argv.
pub fn validate_portal(portal: &str) -> Result<(), String> {
  if portal.is_empty() {
    return Err("Portal is empty".to_string());
  }
  if !portal
    .chars()
    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
  {
    return Err(
      "Portal may only contain letters, digits, dots, dashes and underscores".to_string(),
    );
  }
  Ok(())
}

/// The usual Homebrew locations, mirroring `config::Config::detect_gpclient`.
pub const GPCLIENT_CANDIDATES: [&str; 2] =
  ["/opt/homebrew/bin/gpclient", "/usr/local/bin/gpclient"];

/// Resolve the gpclient executable: a configured absolute path wins (when
/// the file exists), otherwise the Homebrew candidates. Mirrors
/// `config::Config::resolve_gpclient` for the daemon, which must not depend
/// on tauri or the app's Config type.
pub fn resolve_gpclient(configured: Option<&str>) -> Option<PathBuf> {
  if let Some(configured) = configured {
    let path = Path::new(configured);
    if path.is_file() {
      return Some(path.to_path_buf());
    }
  }
  GPCLIENT_CANDIDATES
    .iter()
    .map(PathBuf::from)
    .find(|candidate| candidate.is_file())
}

/// The `gpclient connect` argv the daemon spawns, as a plain vector — never
/// a shell string. `reuse_cookie` appends `--cookie-cache`.
pub fn connect_argv(gpclient: &Path, portal: &str, reuse_cookie: bool) -> Vec<String> {
  let mut argv = vec![
    gpclient.to_string_lossy().into_owned(),
    "--log-format".to_string(),
    "json".to_string(),
    "connect".to_string(),
    portal.to_string(),
    "--browser".to_string(),
    "--auto-gateway".to_string(),
  ];
  if reuse_cookie {
    argv.push("--cookie-cache".to_string());
  }
  argv
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn portals_are_validated_with_the_add_portal_charset() {
    assert!(validate_portal("gp.imm.re").is_ok());
    assert!(validate_portal("portal-1_example").is_ok());
    assert!(validate_portal("").is_err());
    // Shell metacharacters, spaces, traversal segments: all rejected.
    assert!(validate_portal("a b").is_err());
    assert!(validate_portal("x;rm").is_err());
    assert!(validate_portal("../../etc").is_err());
    assert!(validate_portal("p\n").is_err());
  }

  #[test]
  fn connect_argv_matches_the_documented_cli_invocation() {
    let argv = connect_argv(Path::new("/opt/homebrew/bin/gpclient"), "gp.imm.re", true);
    assert_eq!(
      argv,
      vec![
        "/opt/homebrew/bin/gpclient",
        "--log-format",
        "json",
        "connect",
        "gp.imm.re",
        "--browser",
        "--auto-gateway",
        "--cookie-cache",
      ]
    );
    assert!(!connect_argv(Path::new("/g"), "p", false).contains(&"--cookie-cache".to_string()));
  }

  #[test]
  fn operation_names_stay_stable() {
    // The daemon dispatches on these exact strings; a typo desyncs app and
    // helper silently (every op would fall through to the failure path).
    assert_eq!(OP_START, "start");
    assert_eq!(OP_STOP, "stop");
    assert_eq!(OP_CANCEL, "cancel");
    assert_eq!(OP_SELFTEST, "selftest");
  }

  #[test]
  fn resolves_gpclient_like_the_app_config() {
    // Pure check: an absolute path to an existing file wins.
    let exe = std::env::current_exe().unwrap();
    assert_eq!(
      resolve_gpclient(Some(exe.to_string_lossy().into_owned().as_str())),
      Some(exe.clone())
    );
    // A configured path that does not exist falls back to detection, which
    // is empty on machines without Homebrew gpclient.
    assert_eq!(
      resolve_gpclient(Some("/nonexistent/gpclient")),
      resolve_gpclient(None)
    );
  }
}

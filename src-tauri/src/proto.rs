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
/// The app bundle identifier (tauri.conf.json `identifier`), used by
/// `scripts/finalize-bundle.mjs` when signing the bundle ad-hoc at build
/// time (the only signing step: nothing is re-signed at runtime anymore).
pub const BUNDLE_IDENTIFIER: &str = "dev.gpbar.s1";

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
/// `BUILD_REV` of the answering daemon (`get_version` reply): the client
/// compares it against its own to turn a stale daemon over.
pub const KEY_VERSION: &CStr = c"version";

// -- operations --------------------------------------------------------------------

pub const OP_START: &str = "start";
pub const OP_STOP: &str = "stop";
pub const OP_CANCEL: &str = "cancel";
pub const OP_SELFTEST: &str = "selftest";
/// Handshake op: the daemon replies with its `BUILD_REV` (see `build.rs`).
/// The client uses it to detect a daemon from a stale build.
pub const OP_GET_VERSION: &str = "get_version";
/// Ask the daemon to exit(0): the reply is sent FIRST, then the daemon
/// exits and launchd `KeepAlive` re-spawns it from the new binary on disk
/// (the daemon is resident: nothing but a rebuild-aware exit replaces it).
pub const OP_TERMINATE_SELF: &str = "terminate_self";

/// How long the client waits after `terminate_self` before reconnecting:
/// launchd must re-spawn the daemon from the fresh binary for the retry to
/// land on the NEW code and not on a dying socket.
pub const TURNOVER_SETTLE_MS: u64 = 300;

// -- shared policy -----------------------------------------------------------------

/// The PID lock file gpclient maintains: written once the tunnel is up,
/// removed on disconnect. World-readable, so both the unprivileged app
/// (status poller) and the root daemon (stop verification) can watch it.
pub const LOCK_FILE: &str = "/var/run/gpclient.lock";

/// How long the daemon waits for the worker's own teardown after
/// `gpclient disconnect` (and after SIGTERM on cancel) before failing loud.
pub const GRACEFUL_STOP_TIMEOUT_SECS: u64 = 10;

// -- the one-shot launchd job for the VPN worker --------------------------------------
//
// On macOS 27.0.1 (26A434) a root worker born as a child of the gp-helper
// daemon (itself a launchd child) is signal-deaf, exactly like the
// authtrampoline/AEWP children of the October incident: `gpclient
// disconnect` exits 0, the lock file never goes, the phase sticks. Only
// workers born DIRECTLY of launchd stay signalable (RECAP.md). The daemon
// therefore generates and loads a one-shot launchd job per connect — the
// same design as the sudoers wrapper of solution 3, executed with root
// privileges from the XPC daemon instead of sudo.

/// Label of the one-shot worker job.
pub const WORKER_LABEL: &str = "dev.gpbar.s1.worker";

/// Where the daemon installs the worker job. Distinct from solution 3's
/// `/Library/Application Support/GP Bar/` so the two installs never clash.
pub const WORKER_PLIST: &str = "/Library/Application Support/GP Bar S1/gp-worker.plist";

/// XML-escape a value for a plist. The portal is charset-validated, but the
/// paths come from system APIs: stay safe by construction.
pub fn xml_escape(value: &str) -> String {
  let mut escaped = String::with_capacity(value.len());
  for ch in value.chars() {
    match ch {
      '&' => escaped.push_str("&amp;"),
      '<' => escaped.push_str("&lt;"),
      '>' => escaped.push_str("&gt;"),
      '"' => escaped.push_str("&quot;"),
      '\'' => escaped.push_str("&apos;"),
      _ => escaped.push(ch),
    }
  }
  escaped
}

/// The one-shot launchd job plist the daemon loads per connect.
///
/// `RunAtLoad` + no `KeepAlive`, outside `/Library/LaunchDaemons` (no boot
/// autostart): loaded only when the daemon asks launchd to run it.
pub fn build_worker_plist(
  gpclient: &Path,
  portal: &str,
  reuse_cookie: bool,
  log_path: &Path,
  uid: &str,
  name: &str,
) -> String {
  let args = proto_connect_argv_xml(gpclient, portal, reuse_cookie);
  format!(
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{label}</string>
	<key>ProgramArguments</key><array>{args}</array>
	<key>EnvironmentVariables</key><dict>
		<key>HOME</key><string>/var/root</string>
		<key>PATH</key><string>/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
		<key>SUDO_UID</key><string>{uid}</string>
		<key>SUDO_USER</key><string>{name}</string>
	</dict>
	<key>StandardOutPath</key><string>{log}</string>
	<key>StandardErrorPath</key><string>{log}</string>
	<key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
    label = WORKER_LABEL,
    args = args,
    uid = xml_escape(uid),
    name = xml_escape(name),
    log = xml_escape(&log_path.to_string_lossy()),
  )
}

/// ProgramArguments as XML strings (shares `connect_argv`'s ordering).
fn proto_connect_argv_xml(gpclient: &Path, portal: &str, reuse_cookie: bool) -> String {
  connect_argv(gpclient, portal, reuse_cookie)
    .iter()
    .map(|arg| format!("<string>{}</string>", xml_escape(arg)))
    .collect()
}

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

/// Resolve the gpclient executable from the fixed Homebrew candidates only.
///
/// Deliberately NOT honoring the user config's `gpclient_path` (unlike the
/// app-side `config::Config::resolve_gpclient`, which stays user-side for
/// the version display): the config file is user-writable, and a root
/// daemon executing a path from it would be a full privilege escalation for
/// any process running as the user — far beyond the documented residual
/// risk of "any user process can toggle the VPN". No caller-controlled
/// paths, ever.
pub fn resolve_gpclient() -> Option<PathBuf> {
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
  fn worker_plist_carries_the_connect_job_and_env() {
    let plist = build_worker_plist(
      Path::new("/opt/homebrew/bin/gpclient"),
      "vpn.example.com",
      true,
      Path::new("/Users/test/Library/Logs/gp-bar/gpclient-connect.log"),
      "501",
      "test",
    );
    assert!(plist.contains("<string>dev.gpbar.s1.worker</string>"));
    assert!(plist.contains("<string>--log-format</string><string>json</string>"));
    assert!(plist.contains("<string>vpn.example.com</string>"));
    assert!(plist.contains("<string>--cookie-cache</string>"));
    assert!(plist.contains("<key>SUDO_UID</key><string>501</string>"));
    assert!(plist.contains("<key>StandardOutPath</key>"));
    assert!(plist.contains("<key>RunAtLoad</key><true/>"));
    // KeepAlive must stay absent: one-shot only, no autorestart.
    assert!(!plist.contains("KeepAlive"));
  }

  #[test]
  fn portals_are_validated_with_the_add_portal_charset() {
    assert!(validate_portal("vpn.example.com").is_ok());
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
    let argv = connect_argv(Path::new("/opt/homebrew/bin/gpclient"), "vpn.example.com", true);
    assert_eq!(
      argv,
      vec![
        "/opt/homebrew/bin/gpclient",
        "--log-format",
        "json",
        "connect",
        "vpn.example.com",
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
    assert_eq!(OP_GET_VERSION, "get_version");
    assert_eq!(OP_TERMINATE_SELF, "terminate_self");
    // The turnover settle delay only needs to outlive a daemon exit + spawn;
    // a third of a second is plenty for launchd KeepAlive.
    assert!(TURNOVER_SETTLE_MS >= 300);
  }

  #[test]
  fn resolves_gpclient_from_fixed_candidates_only() {
    // The result is one of the fixed candidates (or none): a user-writable
    // config path must never influence what the root daemon executes.
    let resolved = resolve_gpclient();
    let expected = GPCLIENT_CANDIDATES
      .iter()
      .map(PathBuf::from)
      .find(|candidate: &PathBuf| candidate.is_file());
    assert_eq!(resolved, expected);
    assert!(
      resolved.is_none()
        || GPCLIENT_CANDIDATES
          .iter()
          .any(|c| resolved.as_deref() == Some(Path::new(c)))
    );
  }
}

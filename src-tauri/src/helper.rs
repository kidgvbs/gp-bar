//! The app side of the privileged helper: registering the `gp-helper`
//! LaunchDaemon with SMAppService and talking to it over XPC.
//!
//! Architecture (see also `bin/gp-helper.rs`):
//!
//! ```text
//! app (user, Tauri) ──XPC (mach service dev.gpbar.helper)──> gp-helper (root daemon)
//! ```
//!
//! * The daemon is registered ONCE through `SMAppService`: the plist must
//!   live inside the app bundle (`Contents/Library/LaunchDaemons/`) at
//!   registration time. GP Bar ships it as a plain resource, and copies it
//!   into place at runtime — the app lives in `~/Applications`, which is
//!   user-writable.
//! * `Program` in the daemon plist is an ABSOLUTE path into the bundle, so
//!   a registered app must not move; a rebuild that keeps the same path
//!   does NOT need re-registration (launchd re-reads the plist at every
//!   daemon start, and the helper binary itself is re-read from disk).
//! * Every privileged operation is a fixed XPC message with scalar
//!   arguments; nothing arbitrary travels over the wire.
//!
//! XPC sync replies must not be requested from event handlers
//! (`connection.h`); every call here runs through `spawn_blocking`.

use std::{
  fs,
  path::{Path, PathBuf},
};

use block2::RcBlock;
use objc2_foundation::NSString;
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use crate::{proto, xpc};

/// Human-facing helper status for the panel footer.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperStatus {
  Enabled,
  RequiresApproval,
  NotFound,
  NotRegistered,
}

impl HelperStatus {
  #[allow(dead_code)] // kept for a future "test helper" panel action
  pub fn label(&self) -> &'static str {
    match self {
      Self::Enabled => "enabled",
      Self::RequiresApproval => "needs approval",
      Self::NotFound => "app bundle missing",
      Self::NotRegistered => "not installed",
    }
  }

  pub fn is_ready(&self) -> bool {
    matches!(self, Self::Enabled)
  }
}

// -- bundle plumbing ----------------------------------------------------------------

/// `…/GP Bar.app`, from the running executable path.
///
/// `None` outside a real bundle (`cargo run` / `tauri dev`): helper
/// registration only makes sense from the installed app.
pub fn app_bundle() -> Option<PathBuf> {
  let exe = std::env::current_exe().ok()?;
  let macos = exe.parent()?; // Contents/MacOS
  let contents = macos.parent()?; // Contents
  if !contents.join("Info.plist").is_file() {
    return None;
  }
  Some(contents.parent()?.to_path_buf())
}

/// `…/GP Bar.app/Contents/Library/LaunchDaemons/dev.gpbar.helper.plist` —
/// where SMAppService looks for `DAEMON_PLIST`.
pub fn daemon_plist_path() -> Option<PathBuf> {
  app_bundle().map(|bundle| {
    bundle
      .join("Contents")
      .join("Library")
      .join("LaunchDaemons")
      .join(proto::DAEMON_PLIST)
  })
}

/// The `gp-helper` sidecar inside the bundle (Tauri strips the target-triple
/// suffix from `externalBin` entries when copying into `Contents/MacOS`).
pub fn helper_binary() -> Option<PathBuf> {
  app_bundle().map(|bundle| bundle.join("Contents").join("MacOS").join("gp-helper"))
}

/// Re-sign the app bundle ad-hoc, covering the just-written LaunchDaemons
/// plist (see `sync_daemon_plist`). `codesign` walks nested executables, so
/// the `gp-helper` sidecar is signed too. User-owned bundle: no admin needed.
fn re_sign_bundle() -> Result<(), String> {
  let bundle =
    app_bundle().ok_or_else(|| "GP Bar is not running from an app bundle".to_string())?;
  let output = std::process::Command::new("/usr/bin/codesign")
    .arg("--force")
    .arg("--sign")
    .arg("-")
    .arg("--identifier")
    .arg(proto::BUNDLE_IDENTIFIER)
    .arg(&bundle)
    .output()
    .map_err(|e| format!("cannot run codesign: {e}"))?;
  if !output.status.success() {
    return Err(format!(
      "re-signing the bundle failed: {}",
      String::from_utf8_lossy(&output.stderr).trim()
    ));
  }
  Ok(())
}

/// The LaunchDaemon plist SMAppService installs.
///
/// * `MachServices` + `RunAtLoad false`: launchd boots the daemon on demand
///   when the first client looks up the Mach service.
/// * `KeepAlive true`: once booted it STAYS up. Decision: the worker must
///   remain manageable via XPC (stop/cancel) for the whole tunnel lifetime,
///   and `cancel` needs the child PID only the live daemon remembers — a
///   daemon that exits between `start` and `cancel` would lose it. Cost: a
///   sleeping root process between connections (personal tool: accepted).
pub fn build_daemon_plist(helper: &Path) -> String {
  format!(
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>dev.gpbar.helper</string>
	<key>ProgramArguments</key>
	<array><string>{helper}</string></array>
	<key>MachServices</key>
	<dict><key>dev.gpbar.helper</key><true/></dict>
	<key>KeepAlive</key><true/>
	<key>RunAtLoad</key><false/>
</dict>
</plist>
"#,
    helper = xml_escape(&helper.to_string_lossy())
  )
}

fn xml_escape(value: &str) -> String {
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

/// The plist the bundle should carry right now (`None` outside a bundle).
fn expected_plist() -> Option<String> {
  helper_binary().map(|helper| build_daemon_plist(&helper))
}

/// Make sure the in-bundle plist exists and matches the current bundle path.
///
/// The app runs from a user-writable location (`~/Applications`), so a plain
/// file write is enough — no admin privileges. A stale plist (bundle moved)
/// is detected by the caller through `status`/`plist_is_current` and fixed
/// by `unregister` + `register`.
///
/// BTM requires the LaunchDaemons plist to be **covered by the bundle
/// signature** when `register` loads it (error -67054, "Codesigning failure
/// loading plist"): a plist written at runtime lands *outside* the sealed
/// resources, so the bundle is re-signed ad-hoc here (the same step
/// `scripts/finalize-bundle.mjs` performs at build time; the user owns the
/// bundle, so no admin prompt). With a real Developer ID distribution the
/// plist ships at build time and this becomes a no-op via `plist_is_current`.
fn sync_daemon_plist() -> Result<PathBuf, String> {
  let (path, content) = match (daemon_plist_path(), expected_plist()) {
    (Some(path), Some(content)) => (path, content),
    _ => return Err("GP Bar is not running from an app bundle: install it (e.g. into ~/Applications) to use the privileged helper".to_string()),
  };
  if let Some(dir) = path.parent() {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
  }
  fs::write(&path, &content).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
  re_sign_bundle()?;
  Ok(path)
}

fn plist_is_current() -> bool {
  match (daemon_plist_path(), expected_plist()) {
    (Some(path), Some(content)) => {
      fs::read_to_string(&path).ok().as_deref() == Some(content.as_str())
    }
    _ => false,
  }
}

// -- SMAppService ------------------------------------------------------------------

fn daemon_service() -> objc2::rc::Retained<SMAppService> {
  unsafe { SMAppService::daemonServiceWithPlistName(&NSString::from_str(proto::DAEMON_PLIST)) }
}

/// Current registration status.
pub fn status() -> HelperStatus {
  let raw = unsafe { daemon_service().status() };
  match raw {
    SMAppServiceStatus::Enabled => HelperStatus::Enabled,
    SMAppServiceStatus::RequiresApproval => HelperStatus::RequiresApproval,
    SMAppServiceStatus::NotFound => HelperStatus::NotFound,
    _ => HelperStatus::NotRegistered,
  }
}

/// Blocking register (system authorization prompt when needed). Call through
/// `spawn_blocking`.
pub fn register() -> Result<(), String> {
  if plist_is_current() && status().is_ready() {
    return Ok(());
  }
  // The plist changed since registration (bundle moved/rebuilt in place):
  // launchd would keep booting the stale Program, so re-register.
  if !plist_is_current() && status().is_ready() {
    if let Err(error) = unsafe { daemon_service().unregisterAndReturnError() } {
      eprintln!(
        "gp-bar: unregistering the moved helper failed: {}",
        error.localizedDescription()
      );
    }
  }
  sync_daemon_plist()?;
  match unsafe { daemon_service().registerAndReturnError() } {
    Ok(()) => Ok(()),
    Err(error) => Err(format!(
      "registering the helper daemon failed: {}",
      error.localizedDescription()
    )),
  }
}

/// Make sure the helper is registered and ready; the ONE blocking point that
/// can show a system prompt (first connect only).
pub async fn ensure_ready() -> Result<(), String> {
  if status().is_ready() {
    return Ok(());
  }
  tauri::async_runtime::spawn_blocking(register)
    .await
    .map_err(|e| format!("join error: {e}"))?
}

// -- XPC client --------------------------------------------------------------------

/// What a successful `start` reply carries.
#[derive(Debug)]
pub struct StartReply {
  /// Reported for diagnostics; the status poller adopts the worker from
  /// the lock file, GP Bar does not track the PID itself.
  #[allow(dead_code)]
  pub worker_pid: i64,
}

fn call(op: &str, portal: Option<&str>, reuse_cookie: bool) -> Result<serde_json::Value, String> {
  let conn = xpc::create_client(proto::SERVICE);
  if conn.is_null() {
    return Err("cannot create a connection to the GP Bar helper".to_string());
  }
  let on_error: RcBlock<dyn Fn(xpc::xpc_object_t)> = RcBlock::new(|event| {
    if xpc::is_error(event) {
      eprintln!("gp-bar: helper connection error event");
    }
  });
  xpc::set_event_handler(conn, on_error);
  xpc::resume(conn);

  let msg = xpc::dict_new();
  xpc::dict_set_string(msg, proto::KEY_OP, op);
  if let Some(portal) = portal {
    xpc::dict_set_string(msg, proto::KEY_PORTAL, portal);
  }
  xpc::dict_set_bool(msg, proto::KEY_REUSE_COOKIE, reuse_cookie);

  let reply = xpc::send_with_reply_sync(conn, msg);
  xpc::release(msg);

  let result = if xpc::is_dictionary(reply) {
    let ok = xpc::dict_get_bool(reply, proto::KEY_OK);
    let error = xpc::dict_get_string(reply, proto::KEY_ERROR);
    let pid = xpc::dict_get_int64(reply, proto::KEY_PID);
    let uid = xpc::dict_get_int64(reply, proto::KEY_UID);
    let value = serde_json::json!({ "pid": pid, "uid": uid });
    if ok {
      Ok(value)
    } else {
      Err(error.unwrap_or_else(|| "the helper reported a failure".to_string()))
    }
  } else {
    Err("the GP Bar helper daemon is unreachable".to_string())
  };
  xpc::release(reply);
  xpc::cancel(conn);
  result
}

/// Spawn the gpclient worker as a child of the root daemon.
pub async fn start(portal: &str, reuse_cookie: bool) -> Result<StartReply, String> {
  let portal = portal.to_string();
  tauri::async_runtime::spawn_blocking(move || call(proto::OP_START, Some(&portal), reuse_cookie))
    .await
    .map_err(|e| format!("join error: {e}"))?
    .map(|value| StartReply {
      worker_pid: value.get("pid").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

/// Tear the tunnel down gracefully (`gpclient disconnect`), verified against
/// the lock file, failing loud if the worker does not stop.
pub async fn stop() -> Result<(), String> {
  tauri::async_runtime::spawn_blocking(move || call(proto::OP_STOP, None, false))
    .await
    .map_err(|e| format!("join error: {e}"))?
    .map(|_| ())
}

/// Interrupt a connecting worker with SIGTERM (never SIGKILL: a killed
/// worker skips the vpnc-script teardown).
pub async fn cancel() -> Result<(), String> {
  tauri::async_runtime::spawn_blocking(move || call(proto::OP_CANCEL, None, false))
    .await
    .map_err(|e| format!("join error: {e}"))?
    .map(|_| ())
}

/// Round-trip proof: the daemon replies with its (root) uid.
#[allow(dead_code)] // manual debugging aid
pub async fn selftest() -> Result<i64, String> {
  tauri::async_runtime::spawn_blocking(move || call(proto::OP_SELFTEST, None, false))
    .await
    .map_err(|e| format!("join error: {e}"))?
    .map(|value| value.get("uid").and_then(|v| v.as_i64()).unwrap_or(-1))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn plist_carries_mach_services_and_the_absolute_helper_path() {
    let plist = build_daemon_plist(Path::new(
      "/Users/me/Applications/GP Bar.app/Contents/MacOS/gp-helper",
    ));
    assert!(plist.contains("<key>Label</key><string>dev.gpbar.helper</string>"));
    assert!(plist.contains(
      "<array><string>/Users/me/Applications/GP Bar.app/Contents/MacOS/gp-helper</string></array>"
    ));
    assert!(plist.contains("<key>MachServices</key>"));
    assert!(plist.contains("<key>dev.gpbar.helper</key><true/>"));
    assert!(plist.contains("<key>RunAtLoad</key><false/>"));
    assert!(plist.contains("<key>KeepAlive</key><true/>"));
  }

  #[test]
  fn status_labels_are_short_and_stable() {
    assert_eq!(HelperStatus::Enabled.label(), "enabled");
    assert_eq!(HelperStatus::NotRegistered.label(), "not installed");
    assert!(HelperStatus::Enabled.is_ready());
    assert!(!HelperStatus::NotRegistered.is_ready());
  }
}

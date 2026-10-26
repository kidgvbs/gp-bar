//! The app side of the privileged helper: registering the `gp-helper`
//! LaunchDaemon with SMAppService and talking to it over XPC.
//!
//! Architecture (see also `bin/gp-helper.rs`):
//!
//! ```text
//! app (user, Tauri) ──XPC (mach service dev.gpbar.helper)──> gp-helper (root daemon)
//! ```
//!
//! * The daemon is registered ONCE through `SMAppService`: the plist ships
//!   INSIDE the app bundle at build time
//!   (`Contents/Library/LaunchDaemons/dev.gpbar.helper.plist`, written by
//!   `scripts/finalize-bundle.mjs` BEFORE the ad-hoc codesign step, so the
//!   plist is covered by the bundle signature — BTM requirement). Nothing
//!   is written into the bundle at runtime: a runtime re-sign would change
//!   the cdhash at every launch and is our prime suspect for the BTM
//!   quarantina (RECAP.md §13).
//! * `Program` in the daemon plist is an ABSOLUTE path into the bundle, so
//!   a registered app must not move; a rebuild that keeps the same path
//!   does NOT need re-registration (launchd re-reads the plist at every
//!   daemon start, and the helper binary itself is re-read from disk).
//! * Every privileged operation is a fixed XPC message with scalar
//!   arguments; nothing arbitrary travels over the wire.
//!
//! Turnover: the daemon is `KeepAlive`, so a rebuild does NOT replace a
//! resident one. Before every operation the client handshakes on
//! `BUILD_REV` (baked into both binaries at build time by `build.rs`) and
//! asks a stale daemon to terminate itself; launchd then re-spawns the
//! helper from the NEW binary on disk.
//!
//! XPC sync replies must not be requested from event handlers
//! (`connection.h`); every call here runs through `spawn_blocking`.

use std::time::Duration;

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
/// `spawn_blocking`. The LaunchDaemons plist ships with the bundle (written
/// at build time by `scripts/finalize-bundle.mjs`), so there is nothing to
/// sync here — and no runtime re-sign: a plist re-written (and re-signed)
/// after the fact would land outside the sealed BTM record.
pub fn register() -> Result<(), String> {
  if status().is_ready() {
    return Ok(());
  }
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

/// Open a client connection to the helper's Mach service.
fn open() -> Result<xpc::xpc_connection_t, String> {
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
  Ok(conn)
}

/// Send one operation and parse the sync reply (`{ok, error?, pid, uid,
/// version?}`).
fn request(
  conn: xpc::xpc_connection_t,
  op: &str,
  portal: Option<&str>,
  reuse_cookie: bool,
) -> Result<serde_json::Value, String> {
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
    let version = xpc::dict_get_string(reply, proto::KEY_VERSION);
    let mut value = serde_json::json!({ "pid": pid, "uid": uid });
    if let Some(version) = version {
      value["version"] = serde_json::json!(version);
    }
    if ok {
      Ok(value)
    } else {
      Err(error.unwrap_or_else(|| "the helper reported a failure".to_string()))
    }
  } else {
    Err("the GP Bar helper daemon is unreachable".to_string())
  };
  xpc::release(reply);
  result
}

/// Does a daemon reporting `remote` need to be turned over? A daemon from a
/// different build (stale `BUILD_REV`) must be replaced before serving.
fn daemon_is_stale(remote: &str) -> bool {
  remote != env!("BUILD_REV")
}

/// One full operation: handshake on `BUILD_REV`, turn over a stale daemon,
/// then run the requested op on a fresh connection.
fn call(op: &str, portal: Option<&str>, reuse_cookie: bool) -> Result<serde_json::Value, String> {
  let conn = open()?;
  // Version handshake: the KeepAlive daemon survives rebuilds, so make sure
  // the resident binary matches this app before trusting it with the op.
  let daemon_rev = request(conn, proto::OP_GET_VERSION, None, false)
    .ok()
    .and_then(|value| {
      value
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    });
  if let Some(rev) = &daemon_rev {
    if daemon_is_stale(rev) {
      eprintln!(
        "gp-bar: helper daemon rev {rev} != app rev {}: requesting turnover",
        env!("BUILD_REV")
      );
      let _ = request(conn, proto::OP_TERMINATE_SELF, None, false);
      xpc::cancel(conn);
      // launchd KeepAlive re-spawns the daemon from the NEW binary on disk.
      std::thread::sleep(Duration::from_millis(proto::TURNOVER_SETTLE_MS));
      let conn = open()?;
      let result = request(conn, op, portal, reuse_cookie);
      xpc::cancel(conn);
      return result;
    }
  } else {
    // Pre-handshake daemon (older build): nothing we can ask it to do that
    // it does not know; just proceed with the op.
    eprintln!("gp-bar: helper daemon did not report a version; skipping turnover handshake");
  }
  let result = request(conn, op, portal, reuse_cookie);
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
  fn status_labels_are_short_and_stable() {
    assert_eq!(HelperStatus::Enabled.label(), "enabled");
    assert_eq!(HelperStatus::NotRegistered.label(), "not installed");
    assert!(HelperStatus::Enabled.is_ready());
    assert!(!HelperStatus::NotRegistered.is_ready());
  }

  #[test]
  fn turnover_compares_against_the_baked_build_rev() {
    // The daemon rev is a build identifier baked at compile time: any
    // mismatch turns the daemon over, an exact match does not.
    assert!(daemon_is_stale("0000000"));
    assert!(daemon_is_stale(""));
    assert!(!daemon_is_stale(env!("BUILD_REV")));
    assert!(!env!("BUILD_REV").is_empty());
  }
}
//! The only place that touches `SMAppService` — register, unregister,
//! status.
//!
//! Mirrors the app crate's `helper.rs` (same objc2 pins, same call shapes):
//!
//! * `daemonServiceWithPlistName` — the plist must be the one inside the
//!   app bundle, sealed into its signature (anomaly B).
//! * `registerAndReturnError` — the call that shows the system prompt. It
//!   only presents a prompt from a **Regular-policy** app process: that is
//!   why the protocol has the human `open` the bundle (the sampler) instead
//!   of running `smprobe register` from Terminal, where the prompt cannot
//!   anchor and the call may return EPERM with no dialog (anomaly A).
//! * `status` — handled numerically in `status.rs` (the enum is a newtype
//!   over `NSInteger`, so the raw value is always readable).
//!
//! Nothing here runs at build time or in tests: registration is a human
//! action per PROTOCOL.md.

use objc2_foundation::NSString;
use objc2_service_management::SMAppService;

use crate::identity;

fn daemon_service() -> objc2::rc::Retained<SMAppService> {
  unsafe {
    SMAppService::daemonServiceWithPlistName(&NSString::from_str(identity::DAEMON_PLIST_NAME))
  }
}

/// Raw `SMAppServiceStatus` of the probe daemon (decode with
/// `status::decode`).
pub fn read_status() -> isize {
  unsafe { daemon_service().status().0 }
}

/// Blocking register; may present the system authorization prompt (Touch
/// ID). Call only from the sampler launched via `open`.
pub fn register() -> Result<(), String> {
  match unsafe { daemon_service().registerAndReturnError() } {
    Ok(()) => Ok(()),
    Err(error) => Err(error.localizedDescription().to_string()),
  }
}

/// Blocking unregister; the record goes away and (with the bundle deleted)
/// the daemon with it.
pub fn unregister() -> Result<(), String> {
  match unsafe { daemon_service().unregisterAndReturnError() } {
    Ok(()) => Ok(()),
    Err(error) => Err(error.localizedDescription().to_string()),
  }
}

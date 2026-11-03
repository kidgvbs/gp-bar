//! The only place that touches `SMAppService` — status, register, unregister —
//! plus the pure decision that drives R1 self-healing.
//!
//! Mirrors the Fase-0 `smprobe` module (same objc2 pins, same call shapes),
//! with GP Bar's own identity ([`proto::DAEMON_PLIST`]):
//!
//! * `daemonServiceWithPlistName` — the plist must be the one inside the app
//!   bundle, sealed into its signature (anomaly B).
//! * `registerAndReturnError` — the call that shows the system prompt. It only
//!   presents a prompt from a **Regular-policy** app process: that is why the
//!   human opens the bundle instead of registering from a Terminal, where the
//!   prompt cannot anchor and the call may return EPERM with no dialog
//!   (anomaly A). EPERM therefore covers only the prompt — the registration
//!   lands anyway (Fase 0) — so a `register()` error is not by itself proof
//!   that nothing changed.
//! * `status` — a newtype over `NSInteger`, so the raw value is always
//!   readable; the decoding lives here and keeps unknown values visible
//!   instead of collapsing them (RECAP §11).
//!
//! Nothing here runs at build time or in tests: registration is a human
//! action. No test and no self-check may ever call [`register`],
//! [`unregister`] or [`read_status`] (a runtime SMAppService call). Only the
//! pure decoder and [`should_self_heal`] are testable.
//!
//! ## R1 honesty note (Fase 3a)
//!
//! `SMAppService.daemon(plistName:).status()` reads the registration **of the
//! calling app's bundle**. Until Fase 3b packages the daemon plist into
//! `GP Bar.app`, the real app's status is [`NOT_FOUND`] and R1 stays dormant
//! **by design**: what ships now is the decision core and the plumbing, not a
//! behaviour that can be turned on. (The Fase-2 manual test used a separate
//! bundle with its own identity: the app's status cannot see that
//! registration — correct and expected.)

use objc2::rc::Retained;
use objc2_foundation::NSString;
use objc2_service_management::SMAppService;

use crate::proto;

/// Raw `SMAppServiceStatus` values, from the SDK header (do not intuit).
pub const NOT_REGISTERED: isize = 0;
pub const ENABLED: isize = 1;
pub const REQUIRES_APPROVAL: isize = 2;
pub const NOT_FOUND: isize = 3;

/// What the heal path should do, decided from a raw status alone (pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealAction {
    /// The status is not a healable signature — return the original error.
    None,
    /// ENABLED + Unreachable = anomaly D: try a single re-register (R1).
    ReRegister,
    /// NOT_FOUND: the bundle path changed (or this is a fresh install in a
    /// new location) — unregister the old record and re-register (R2).
    Reinstall,
}

/// The shared, pure decision: maps a raw SMAppServiceStatus to the heal
/// action both the path-op R1/R2 logic and the register command use.
///
/// * `ENABLED` → `ReRegister` (anomaly D).
/// * `NOT_FOUND` → `Reinstall` (bundle changed location — exelban/stats case).
/// * Everything else (`NOT_REGISTERED`, `REQUIRES_APPROVAL`, `Unknown`) →
///   `None`: `RequiresApproval` is approval territory (hint, never
///   automatic register); `NotRegistered` needs the explicit R3 gesture.
pub fn heal_action(raw: isize) -> HealAction {
    match raw {
        ENABLED => HealAction::ReRegister,
        NOT_FOUND => HealAction::Reinstall,
        _ => HealAction::None,
    }
}

/// The daemon service of this app's bundle: the plist name, resolved against
/// `Contents/Library/LaunchDaemons` by ServiceManagement.
fn daemon_service() -> Retained<SMAppService> {
    unsafe { SMAppService::daemonServiceWithPlistName(&NSString::from_str(proto::DAEMON_PLIST)) }
}

/// Raw `SMAppServiceStatus` of the daemon (decode with [`decode`]). It reads
/// the CALLING bundle's registration — see the module-level honesty note.
pub fn read_status() -> isize {
    unsafe { daemon_service().status().0 }
}

/// Blocking register; may present the system authorization prompt (Touch ID).
/// Human action only — never call this from a test or a self-check.
pub fn register() -> Result<(), String> {
    match unsafe { daemon_service().registerAndReturnError() } {
        Ok(()) => Ok(()),
        Err(error) => Err(error.localizedDescription().to_string()),
    }
}

/// Blocking unregister; the record goes away and (with the bundle deleted)
/// the daemon with it. Human action only, like [`register`].
pub fn unregister() -> Result<(), String> {
    match unsafe { daemon_service().unregisterAndReturnError() } {
        Ok(()) => Ok(()),
        Err(error) => Err(error.localizedDescription().to_string()),
    }
}

/// Opens System Settings → General → Login Items & Extensions, the panel
/// where the user can approve the daemon. Class method on `SMAppService`
/// (objc2-service-management 0.3.2: `+openSystemSettingsLoginItems`).
pub fn open_system_settings() {
    unsafe { SMAppService::openSystemSettingsLoginItems() };
}

/// Decode a raw status into a stable name. Unknown values (including negative
/// ones or anything a future SDK adds) stay visible.
pub fn decode(raw: isize) -> String {
    match raw {
        NOT_REGISTERED => "NotRegistered".to_string(),
        ENABLED => "Enabled".to_string(),
        REQUIRES_APPROVAL => "RequiresApproval".to_string(),
        NOT_FOUND => "NotFound".to_string(),
        other => format!("Unknown({other})"),
    }
}

/// `<raw> (<name>)` — the format the Fase-0 probe's log and protocol use
/// (e.g. `1 (Enabled)`).
pub fn status_line(raw: isize) -> String {
    format!("{raw} ({})", decode(raw))
}

/// Whether an unreachable daemon on this raw status is the anomaly-D
/// signature: the job is missing from the launchd domain while BTM still
/// reports the service enabled (RECAP §20/§22) — the one case a single
/// re-register can repair.
///
/// Delegates to [`heal_action`] for the decision: `ReRegister` is the only
/// case where a single re-register is the right thing.
pub fn should_self_heal(raw: isize) -> bool {
    heal_action(raw) == HealAction::ReRegister
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_decode_to_the_sdk_names() {
        assert_eq!(decode(NOT_REGISTERED), "NotRegistered");
        assert_eq!(decode(ENABLED), "Enabled");
        assert_eq!(decode(REQUIRES_APPROVAL), "RequiresApproval");
        assert_eq!(decode(NOT_FOUND), "NotFound");
    }

    #[test]
    fn unknown_values_stay_visible_instead_of_collapsing() {
        // A value outside the SDK map must not silently decode as one of the
        // known names: anomaly hunting lives or dies on this.
        assert_eq!(decode(4), "Unknown(4)");
        assert_eq!(decode(-1), "Unknown(-1)");
        assert_eq!(decode(isize::MAX), format!("Unknown({})", isize::MAX));
    }

    #[test]
    fn status_line_matches_the_protocol_format() {
        assert_eq!(status_line(ENABLED), "1 (Enabled)");
        assert_eq!(status_line(NOT_FOUND), "3 (NotFound)");
        assert_eq!(status_line(7), "7 (Unknown(7))");
    }

    #[test]
    fn only_enabled_triggers_self_healing() {
        // The full matrix, unknowns included: Enabled is the ONLY raw value
        // whose "daemon unreachable" pairs with a registration BTM considers
        // live (anomaly D). Everything else is R2/R3 territory (Fase 3b).
        for raw in [0, 1, 2, 3, 4, -1] {
            assert_eq!(
                should_self_heal(raw),
                raw == ENABLED,
                "raw status {raw} must {} trigger a re-register",
                if raw == ENABLED { "" } else { "not" }
            );
        }
    }

    #[test]
    fn heal_action_matrix_covers_every_known_and_unknown_status() {
        // 0: NotRegistered → None (needs the explicit R3 gesture)
        assert_eq!(heal_action(NOT_REGISTERED), HealAction::None);
        // 1: Enabled → ReRegister (anomaly D, R1)
        assert_eq!(heal_action(ENABLED), HealAction::ReRegister);
        // 2: RequiresApproval → None (approval territory, hint only)
        assert_eq!(heal_action(REQUIRES_APPROVAL), HealAction::None);
        // 3: NotFound → Reinstall (bundle path changed, R2)
        assert_eq!(heal_action(NOT_FOUND), HealAction::Reinstall);
        // 4, -1: Unknown → None
        assert_eq!(heal_action(4), HealAction::None);
        assert_eq!(heal_action(-1), HealAction::None);
    }
}

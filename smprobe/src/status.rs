//! Numeric decoding of `SMAppServiceStatus`, defensively.
//!
//! RECAP §11 lesson: in this SDK the enum values are
//! 0=NotRegistered, 1=Enabled, 2=RequiresApproval, 3=NotFound, and they must
//! be read from the header, not intuited. `objc2-service-management` 0.3.2
//! models the enum as a newtype over `NSInteger` (`SMAppServiceStatus(pub
//! NSInteger)`), so the raw value is always available as `.0` — no match on
//! variants that could go stale. An unmapped value decodes to
//! `Unknown(<raw>)` instead of silently collapsing into another arm, which
//! is exactly what a probe wants to see in its log.

/// Raw `SMAppServiceStatus` values, from the SDK header (do not intuit).
pub const NOT_REGISTERED: isize = 0;
pub const ENABLED: isize = 1;
pub const REQUIRES_APPROVAL: isize = 2;
pub const NOT_FOUND: isize = 3;

/// Decode a raw status into a stable name. Unknown values (including
/// negative ones or anything a future SDK adds) stay visible.
pub fn decode(raw: isize) -> String {
  match raw {
    NOT_REGISTERED => "NotRegistered".to_string(),
    ENABLED => "Enabled".to_string(),
    REQUIRES_APPROVAL => "RequiresApproval".to_string(),
    NOT_FOUND => "NotFound".to_string(),
    other => format!("Unknown({other})"),
  }
}

/// `<raw> (<name>)` — the format the CLI prints and the protocol expects
/// (e.g. `1 (Enabled)`).
pub fn status_line(raw: isize) -> String {
  format!("{raw} ({})", decode(raw))
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
}

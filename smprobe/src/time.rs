//! Epoch seconds -> RFC3339 UTC, with no time dependency.
//!
//! The probe is only allowed the objc2/block2/libc dependency set, so the
//! timestamp is derived from `SystemTime` with the classic civil-from-days
//! algorithm (Howard Hinnant, "chrono-compatible low-level date algorithms").
//! The math is pure and pinned by unit tests against values verified with
//! `date -u -r`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 -> (year, month, day) in the proleptic Gregorian
/// calendar. Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
  let z = z + 719_468;
  let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
  let doe = (z - era * 146_097) as u64; // [0, 146096]
  let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
  let y = yoe as i64 + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
  let mp = (5 * doy + 2) / 153; // [0, 11]
  let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
  let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
  (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format epoch seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn format_epoch_utc(secs: u64) -> String {
  let days = (secs / 86_400) as i64;
  let rem = secs % 86_400;
  let (year, month, day) = civil_from_days(days);
  let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
  format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Timestamp for "now". Panics only on a clock before the epoch.
pub fn now_utc() -> String {
  let secs = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("system clock before 1970")
    .as_secs();
  format_epoch_utc(secs)
}

#[cfg(test)]
mod tests {
  use super::*;

  // Expected values verified with `date -u -r <epoch> +%Y-%m-%dT%H:%M:%SZ`.

  #[test]
  fn formats_the_epoch_origin() {
    assert_eq!(format_epoch_utc(0), "1970-01-01T00:00:00Z");
  }

  #[test]
  fn end_of_day_rolls_over_at_midnight() {
    assert_eq!(format_epoch_utc(86_399), "1970-01-01T23:59:59Z");
    assert_eq!(format_epoch_utc(86_400), "1970-01-02T00:00:00Z");
  }

  #[test]
  fn handles_leap_days() {
    assert_eq!(format_epoch_utc(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(format_epoch_utc(1_709_164_800), "2024-02-29T00:00:00Z");
  }

  #[test]
  fn formats_far_future_and_current_era_dates() {
    assert_eq!(format_epoch_utc(2_000_000_000), "2033-05-18T03:33:20Z");
    assert_eq!(format_epoch_utc(1_793_000_000), "2026-10-26T07:33:20Z");
  }
}

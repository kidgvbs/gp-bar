//! The sampler's log-line format — the anomaly-C detector.
//!
//! Every line is one observation with a timestamp, so a spontaneous
//! `NotFound` or a failed ping appears with its exact time, without anyone
//! watching. The format is fixed and greppable:
//!
//! ```text
//! 2026-10-30T12:00:00Z status=1(Enabled) ping=ok uid=0 pid=123 echo=ping
//! 2026-10-30T12:05:00Z status=3(NotFound) ping=error: the smprobe helper is unreachable (connection error)
//! 2026-10-30T11:59:00Z sampler=started
//! 2026-10-30T11:59:01Z register=ok status=1(Enabled)
//! ```
//!
//! `grep -v "status=1(Enabled) ping=ok"` over the log is the endurance
//! verdict: empty output means hours of clean samples.

use crate::client;
use crate::status;

/// One observation line: status (raw + decoded) and the ping outcome.
pub fn sample_line(ts: &str, status_raw: isize, ping: &Result<client::PingOk, String>) -> String {
  let name = status::decode(status_raw);
  let ping = match ping {
    Ok(reply) => format!(
      "ping=ok uid={} pid={} echo={}",
      reply.uid, reply.pid, reply.echo
    ),
    Err(error) => format!("ping=error: {error}"),
  };
  format!("{ts} status={status_raw}({name}) {ping}")
}

/// A one-off event line (`sampler=started`, `register=ok ...`).
pub fn event_line(ts: &str, body: &str) -> String {
  format!("{ts} {body}")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_clean_sample_line_is_stable_and_greppable() {
    let ok = client::PingOk {
      uid: 0,
      pid: 123,
      echo: "ping".to_string(),
    };
    assert_eq!(
      sample_line("2026-10-30T12:00:00Z", 1, &Ok(ok)),
      "2026-10-30T12:00:00Z status=1(Enabled) ping=ok uid=0 pid=123 echo=ping"
    );
  }

  #[test]
  fn an_anomaly_sample_carries_the_raw_status_and_the_error() {
    // Anomaly C shows up as status=3 with a raw number that must stay
    // visible, plus whatever the ping did.
    assert_eq!(
      sample_line("2026-10-30T12:05:00Z", 3, &Err("the smprobe helper is unreachable (connection error)".to_string())),
      "2026-10-30T12:05:00Z status=3(NotFound) ping=error: the smprobe helper is unreachable (connection error)"
    );
  }

  #[test]
  fn unmapped_status_values_stay_numeric_in_the_log() {
    // A value the SDK map does not know must not masquerade as a known
    // status in the log.
    let line = sample_line("2026-10-30T12:05:00Z", 4, &Err("x".to_string()));
    assert_eq!(
      line,
      "2026-10-30T12:05:00Z status=4(Unknown(4)) ping=error: x"
    );
  }

  #[test]
  fn event_lines_are_timestamped_bodies() {
    assert_eq!(
      event_line("2026-10-30T11:59:00Z", "sampler=started"),
      "2026-10-30T11:59:00Z sampler=started"
    );
  }
}

//! The wire protocol between `smprobe` and `smprobe-helper`, and the
//! LaunchDaemon plist generation — everything both sides must agree on.
//!
//! The protocol is deliberately minimal: the daemon accepts exactly one
//! operation (`ping`) and replies `{ok, uid, pid, echo}`. Every decision the
//! daemon makes per message lives in [`decide`], a pure function; the XPC
//! layer only transports what it says.
//!
//! The plist is generated **at install time** (`smprobe make-plist
//! <dest>` — by `scripts/build-probe.sh`, with the DESTINATION path) and
//! written into the bundle before the ad-hoc codesign step, so BTM always
//! sees it as a sealed resource (anomaly B avoided by construction). It is
//! XML-escaped by construction, reusing the discipline of the app crate's
//! `proto.rs`.

use std::ffi::CStr;

use crate::identity;

// -- message keys ----------------------------------------------------------------

pub const KEY_OP: &CStr = c"op";
pub const KEY_OK: &CStr = c"ok";
pub const KEY_ERROR: &CStr = c"error";
/// Real uid of the daemon (0 proves the whole chain).
pub const KEY_UID: &CStr = c"uid";
/// Pid of the daemon.
pub const KEY_PID: &CStr = c"pid";
/// Echo of the operation name (round-trip proof).
pub const KEY_ECHO: &CStr = c"echo";

/// The only operation the daemon accepts.
pub const OP_PING: &str = "ping";

// -- per-message decision -----------------------------------------------------------

/// What the daemon must do with one request. Pure: the daemon builds its
/// reply from this, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PingDecision {
  /// Reply `{ok: true, uid, pid, echo}`.
  Echo {
    /// The operation name echoed back to the client.
    echo: &'static str,
  },
  /// Reply `{ok: false, error}`.
  Refuse {
    /// Stable error text for the client.
    error: &'static str,
  },
}

/// Decide what to do with a request: `op` is the (optional) operation field,
/// `peer_allowed` is the bundle-owner policy verdict for the connection.
pub fn decide(op: Option<&str>, peer_allowed: bool) -> PingDecision {
  if !peer_allowed {
    // The peer's euid does not match the .app bundle owner: refuse with a
    // reply (so the client sees why) instead of dropping the connection.
    return PingDecision::Refuse {
      error: "peer not allowed (bundle-owner uid policy)",
    };
  }
  match op {
    Some(OP_PING) => PingDecision::Echo { echo: OP_PING },
    Some(_) => PingDecision::Refuse {
      error: "unknown operation (ping only)",
    },
    None => PingDecision::Refuse {
      error: "missing op (ping only)",
    },
  }
}

// -- plist generation ----------------------------------------------------------------

/// XML-escape a value for a plist. Inputs here are constants or paths from
/// build argv: stay safe by construction anyway (same helper as the app
/// crate's proto.rs).
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

/// The LaunchDaemon plist, generated at install time for the bundle at
/// `bundle_root` (an absolute path — the destination, never the build
/// tree).
///
/// Deliberately minimal:
///
/// * `Label` == Mach service name == [`identity::SERVICE`].
/// * `Program` is an ABSOLUTE path into the bundle (a registered app must
///   not move; same contract as the app crate's daemon plist).
/// * `MachServices` + **no `KeepAlive`** + **no `RunAtLoad`**: the daemon is
///   strictly on-demand — launchd starts it at the first lookup and the
///   daemon exits on its own after [`identity::DAEMON_IDLE_EXIT_SECS`] idle.
/// * Root logs go to [`identity::DAEMON_LOG`] (root-owned directory).
pub fn daemon_plist(bundle_root: &str) -> String {
  let root = bundle_root.trim_end_matches('/');
  let program = format!("{root}/Contents/MacOS/{}", identity::DAEMON_EXECUTABLE);
  format!(
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{label}</string>
	<key>Program</key><string>{program}</string>
	<key>MachServices</key>
	<dict>
		<key>{service}</key>
		<true/>
	</dict>
	<key>StandardOutPath</key><string>{log}</string>
	<key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
    label = xml_escape(identity::SERVICE_STR),
    program = xml_escape(&program),
    service = xml_escape(identity::SERVICE_STR),
    log = xml_escape(identity::DAEMON_LOG),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_only_accepted_op_is_ping() {
    assert_eq!(OP_PING, "ping");
  }

  #[test]
  fn allowed_peer_with_ping_gets_an_echo_reply() {
    assert_eq!(
      decide(Some("ping"), true),
      PingDecision::Echo { echo: "ping" }
    );
  }

  #[test]
  fn anything_else_is_refused_with_a_stable_error() {
    // A peer that failed the bundle-owner check never reaches the op match:
    // the refusal must not leak what operations exist.
    assert_eq!(
      decide(Some("ping"), false),
      PingDecision::Refuse {
        error: "peer not allowed (bundle-owner uid policy)",
      }
    );
    assert_eq!(
      decide(Some("start"), false),
      decide(Some("ping"), false),
      "refused peers all get the same error"
    );
    // Allowed peer, wrong/missing op.
    assert_eq!(
      decide(Some("start"), true),
      PingDecision::Refuse {
        error: "unknown operation (ping only)",
      }
    );
    assert_eq!(
      decide(Some(""), true),
      PingDecision::Refuse {
        error: "unknown operation (ping only)",
      }
    );
    assert_eq!(
      decide(None, true),
      PingDecision::Refuse {
        error: "missing op (ping only)",
      }
    );
  }

  #[test]
  fn xml_escape_covers_the_five_xml_metacharacters() {
    assert_eq!(
      xml_escape("a&b<c>d\"e'f"),
      "a&amp;b&lt;c&gt;d&quot;e&apos;f"
    );
    assert_eq!(xml_escape("plain/path-1.2"), "plain/path-1.2");
    assert_eq!(xml_escape(""), "");
  }

  #[test]
  fn plist_is_the_exact_expected_document() {
    // Verbatim expectation: any accidental change to the plist (a key, a
    // path, the on-demand semantics) must turn this red.
    let expected = concat!(
      "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
      "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
      "<plist version=\"1.0\">\n",
      "<dict>\n",
      "\t<key>Label</key><string>dev.gpbar.smprobe.helper</string>\n",
      "\t<key>Program</key><string>/Users/test/Applications/SMProbe.app/Contents/MacOS/smprobe-helper</string>\n",
      "\t<key>MachServices</key>\n",
      "\t<dict>\n",
      "\t\t<key>dev.gpbar.smprobe.helper</key>\n",
      "\t\t<true/>\n",
      "\t</dict>\n",
      "\t<key>StandardOutPath</key><string>/Library/Logs/gp-bar/smprobe-helper.log</string>\n",
      "\t<key>StandardErrorPath</key><string>/Library/Logs/gp-bar/smprobe-helper.log</string>\n",
      "</dict>\n",
      "</plist>\n",
    );
    assert_eq!(
      daemon_plist("/Users/test/Applications/SMProbe.app"),
      expected
    );
  }

  #[test]
  fn plist_keys_and_on_demand_semantics() {
    let plist = daemon_plist("/Users/test/Applications/SMProbe.app");
    assert!(plist.contains("<key>Label</key>"));
    assert!(plist.contains("<key>Program</key>"));
    assert!(plist.contains("<key>MachServices</key>"));
    assert!(plist.contains("<key>StandardOutPath</key>"));
    assert!(plist.contains("<key>StandardErrorPath</key>"));
    // On-demand only: neither autostart nor autorestart may appear, or the
    // probe stops exercising "launchd starts the job fresh" (anomaly D).
    assert!(!plist.contains("KeepAlive"));
    assert!(!plist.contains("RunAtLoad"));
  }

  #[test]
  fn plist_escapes_a_hostile_bundle_root_and_normalizes_trailing_slashes() {
    let plist = daemon_plist("/tmp/we&ird <path>/SMProbe.app/");
    assert!(plist.contains("we&amp;ird &lt;path&gt;/SMProbe.app/Contents/MacOS/smprobe-helper"));
    // The raw metacharacters must not leak into the XML text.
    assert!(!plist.contains("we&ird"));
    assert!(!plist.contains("<path>"));
    assert!(!plist.contains("SMProbe.app//Contents"));
    // Both slashes must produce byte-identical documents.
    assert_eq!(
      daemon_plist("/x/SMProbe.app"),
      daemon_plist("/x/SMProbe.app/")
    );
  }
}

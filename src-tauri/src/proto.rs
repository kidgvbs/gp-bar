//! The wire protocol between GP Bar and its `gp-helper` LaunchDaemon, plus
//! the daemon plist generation — everything both sides must agree on.
//!
//! This is the **transport phase** (Fase 1): the daemon answers `ping` and
//! refuses every privileged operation with a stable error. No privileged
//! work happens here, and [`generate_daemon_plist`] is a pure string — the
//! installation that consumes it is Fase 2.
//!
//! Messages are never handled as loose dictionaries outside this module:
//! [`HelperReply`] is the typed shape both directions convert through, so a
//! key typo cannot silently turn into `false`/`""` at a call site.

use std::ffi::CStr;

use crate::xpc::{XpcEvent, XpcObject};

// -- identity ----------------------------------------------------------------------

/// The daemon's Mach service name — the `MachServices` key, i.e. the name
/// the app looks up to make launchd start the job on demand.
pub const SERVICE: &str = "dev.gpbar.helper";

/// [`SERVICE`] in the C form the libxpc calls take.
pub const SERVICE_C: &CStr = c"dev.gpbar.helper";

/// The LaunchDaemon's plist file name (`Contents/Library/LaunchDaemons`).
pub const DAEMON_PLIST: &str = "dev.gpbar.helper.plist";

/// The daemon executable, relative to `Contents/MacOS`.
pub const DAEMON_EXECUTABLE: &str = "gp-helper";

/// Root-owned log file: launchd opens it as root, so it must never sit in a
/// user-writable directory (a user symlink there would let any user process
/// truncate a system file as root).
pub const DAEMON_LOG: &str = "/Library/Logs/gp-bar/gp-helper.log";

/// Idle seconds after which the on-demand daemon exits and lets launchd
/// start it fresh on the next request (no `KeepAlive`, no `RunAtLoad`).
pub const DAEMON_IDLE_EXIT_SECS: u64 = 60;

// -- message keys ------------------------------------------------------------------

pub const KEY_OP: &CStr = c"op";
pub const KEY_OK: &CStr = c"ok";
pub const KEY_ERROR: &CStr = c"error";
/// Real uid of the daemon (0 proves the whole chain).
pub const KEY_UID: &CStr = c"uid";
/// Pid of the daemon.
pub const KEY_PID: &CStr = c"pid";
/// The daemon's own version, so the app can tell a stale helper from a fresh
/// one after an update.
pub const KEY_VERSION: &CStr = c"version";
/// Echo of the operation name (round-trip proof).
pub const KEY_ECHO: &CStr = c"echo";

// -- operations --------------------------------------------------------------------

/// The one operation that does real work in this phase.
pub const OP_PING: &str = "ping";

/// The privileged operations the daemon will implement in Fase 3. Until then
/// it answers them with [`NOT_IMPLEMENTED`] instead of pretending.
pub const PRIVILEGED_OPS: [&str; 5] = ["selftest", "start", "stop", "cancel", "repair"];

/// The stable refusal for a privileged op: the transport works, the daemon is
/// deliberately inert. Worded so a Fase-2/3 log reader knows it is expected.
pub const NOT_IMPLEMENTED: &str = "not implemented: transport phase only (Fase 2)";

// -- payloads ----------------------------------------------------------------------

/// One reply from the daemon, typed. `error` is `Some` exactly when `ok` is
/// false; the unused fields stay at their empty defaults in that case.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HelperReply {
    pub ok: bool,
    pub error: Option<String>,
    pub uid: i64,
    pub pid: i64,
    pub version: String,
    pub echo: String,
}

impl HelperReply {
    /// A successful `ping` answer.
    pub fn ping(uid: i64, pid: i64, version: &str, echo: &str) -> Self {
        Self {
            ok: true,
            error: None,
            uid,
            pid,
            version: version.to_string(),
            echo: echo.to_string(),
        }
    }

    /// A refusal carrying `error` verbatim.
    pub fn refused(error: &str) -> Self {
        Self {
            ok: false,
            error: Some(error.to_string()),
            ..Self::default()
        }
    }

    /// Write this reply into an xpc dictionary (daemon side). Every field is
    /// written, so the client can rely on the full shape for both answers.
    pub fn write_into(&self, dict: &XpcObject) {
        dict.set_bool(KEY_OK, self.ok);
        dict.set_int64(KEY_UID, self.uid);
        dict.set_int64(KEY_PID, self.pid);
        dict.set_string(KEY_VERSION, &self.version);
        dict.set_string(KEY_ECHO, &self.echo);
        if let Some(error) = &self.error {
            dict.set_string(KEY_ERROR, error);
        }
    }

    /// Parse one reply event (client side). `XPC_ERROR_*` dictionaries are
    /// dictionaries too, so the error check comes first or a broken
    /// connection would read as a malformed `ok: false` reply.
    pub fn from_event(event: &XpcEvent) -> Result<Self, ReplyError> {
        if event.is_error() {
            return Err(ReplyError::Unreachable);
        }
        if !event.is_dictionary() {
            return Err(ReplyError::Malformed);
        }
        let reply = Self {
            ok: event.get_bool(KEY_OK),
            error: event.get_string(KEY_ERROR),
            uid: event.get_int64(KEY_UID),
            pid: event.get_int64(KEY_PID),
            version: event.get_string(KEY_VERSION).unwrap_or_default(),
            echo: event.get_string(KEY_ECHO).unwrap_or_default(),
        };
        if reply.ok {
            Ok(reply)
        } else {
            Err(ReplyError::Refused(reply.error.clone().unwrap_or_else(
                || "the privileged helper reported a failure".to_string(),
            )))
        }
    }
}

/// Why a reply could not be turned into a [`HelperReply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyError {
    /// The connection produced an error event: no daemon answered.
    Unreachable,
    /// The event was not a reply dictionary at all.
    Malformed,
    /// The daemon answered `ok: false`; carries its message.
    Refused(String),
}

// -- per-message decision ----------------------------------------------------------

/// What the daemon must do with one request. Pure: the daemon builds its
/// reply from this, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Reply with a `ping` payload.
    Ping,
    /// Reply `ok: false` with [`NOT_IMPLEMENTED`].
    NotImplemented,
    /// Reply `ok: false` with a stable error.
    Refuse { error: &'static str },
}

/// Decide what to do with a request: `op` is the (optional) operation field,
/// `peer_allowed` is the bundle-owner policy verdict for the connection.
pub fn decide(op: Option<&str>, peer_allowed: bool) -> Decision {
    if !peer_allowed {
        // The peer's euid does not match the .app bundle owner: refuse with a
        // reply (so the client sees why) instead of dropping the connection.
        return Decision::Refuse {
            error: "peer not allowed (bundle-owner uid policy)",
        };
    }
    match op {
        Some(OP_PING) => Decision::Ping,
        Some(op) if PRIVILEGED_OPS.contains(&op) => Decision::NotImplemented,
        Some(_) => Decision::Refuse {
            error: "unknown operation",
        },
        None => Decision::Refuse {
            error: "missing op",
        },
    }
}

// -- plist generation --------------------------------------------------------------

/// XML-escape a value for a plist. Inputs here are constants, but escaping by
/// construction means a future variable value cannot break the document.
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

/// The LaunchDaemon plist. Pure (zero I/O): the caller decides when and where
/// to write it; installation is Fase 2.
///
/// Deliberately minimal:
///
/// * `Label` == Mach service name == [`SERVICE`].
/// * `BundleProgram` is the path **relative to the bundle root**: the same
///   reason `SMAppService` exists — the plist is a sealed resource inside the
///   app, so it cannot carry an absolute path that changes when the app moves.
/// * `MachServices` + **no `KeepAlive`** + **no `RunAtLoad`**: the daemon is
///   strictly on-demand — launchd starts it at the first lookup and the
///   daemon exits on its own after [`DAEMON_IDLE_EXIT_SECS`] idle.
/// * Root logs go to [`DAEMON_LOG`] (root-owned directory).
pub fn generate_daemon_plist() -> String {
    let program = format!("Contents/MacOS/{DAEMON_EXECUTABLE}");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{label}</string>
	<key>BundleProgram</key><string>{program}</string>
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
        label = xml_escape(SERVICE),
        program = xml_escape(&program),
        service = xml_escape(SERVICE),
        log = xml_escape(DAEMON_LOG),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_is_answered_and_privileged_ops_are_not_implemented() {
        assert_eq!(decide(Some("ping"), true), Decision::Ping);
        for op in PRIVILEGED_OPS {
            assert_eq!(
                decide(Some(op), true),
                Decision::NotImplemented,
                "{op} must be refused as not implemented"
            );
        }
    }

    #[test]
    fn anything_else_is_refused_with_a_stable_error() {
        assert_eq!(
            decide(Some("ping"), false),
            Decision::Refuse {
                error: "peer not allowed (bundle-owner uid policy)",
            }
        );
        // A refused peer never learns which operations exist.
        assert_eq!(decide(Some("start"), false), decide(Some("ping"), false));
        assert_eq!(
            decide(Some("frobnicate"), true),
            Decision::Refuse {
                error: "unknown operation",
            }
        );
        assert_eq!(
            decide(Some(""), true),
            Decision::Refuse {
                error: "unknown operation",
            }
        );
        assert_eq!(
            decide(None, true),
            Decision::Refuse {
                error: "missing op"
            }
        );
    }

    #[test]
    fn a_refusal_roundtrips_through_the_typed_payload() {
        let refused = HelperReply::refused(NOT_IMPLEMENTED);
        assert!(!refused.ok);
        assert_eq!(refused.error.as_deref(), Some(NOT_IMPLEMENTED));
        // A ping answer carries the daemon's identity and no error.
        let ping = HelperReply::ping(0, 42, "1.2.0", "ping");
        assert!(ping.ok);
        assert_eq!(ping.error, None);
        assert_eq!(ping.uid, 0);
        assert_eq!(ping.pid, 42);
        assert_eq!(ping.version, "1.2.0");
        assert_eq!(ping.echo, "ping");
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
            "\t<key>Label</key><string>dev.gpbar.helper</string>\n",
            "\t<key>BundleProgram</key><string>Contents/MacOS/gp-helper</string>\n",
            "\t<key>MachServices</key>\n",
            "\t<dict>\n",
            "\t\t<key>dev.gpbar.helper</key>\n",
            "\t\t<true/>\n",
            "\t</dict>\n",
            "\t<key>StandardOutPath</key><string>/Library/Logs/gp-bar/gp-helper.log</string>\n",
            "\t<key>StandardErrorPath</key><string>/Library/Logs/gp-bar/gp-helper.log</string>\n",
            "</dict>\n",
            "</plist>\n",
        );
        assert_eq!(generate_daemon_plist(), expected);
    }

    #[test]
    fn plist_carries_the_required_fields_and_stays_on_demand() {
        let plist = generate_daemon_plist();
        assert!(plist.contains(&format!("<key>Label</key><string>{SERVICE}</string>")));
        assert!(plist.contains(&format!("<key>{SERVICE}</key>")));
        assert!(plist.contains("<key>BundleProgram</key>"));
        assert!(plist.contains(DAEMON_LOG));
        // On-demand only: neither autostart nor autorestart may appear, or
        // launchd would keep a root daemon alive with nobody talking to it.
        assert!(!plist.contains("KeepAlive"));
        assert!(!plist.contains("RunAtLoad"));
        // A relative program path: no absolute bundle location leaks in.
        assert!(!plist.contains("/Applications"));
        assert!(!plist.contains("Program</key><string>/"));
    }
}

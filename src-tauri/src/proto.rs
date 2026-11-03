//! The wire protocol between GP Bar and its `gp-helper` LaunchDaemon, plus
//! the daemon plist generation — everything both sides must agree on.
//!
//! Fase 2: the daemon answers `ping` and **executes** every privileged
//! operation by running the root-owned [`ADMIN_SCRIPT`] it installs — the
//! same `/Library/PrivilegedHelperTools/gp-bar/admin` the sudo path runs, so
//! there is exactly one implementation of the privileged logic and no
//! divergence between the two transports. What lives here is only the wire
//! shape and the pure validation in front of the exec ([`validate_op`],
//! [`decide`]): the script re-validates everything (defence in depth), so
//! none of this is the security boundary.
//!
//! [`generate_daemon_plist`] stays a pure string; the installation that
//! consumes it is Fase 3 (SMAppService).
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

/// The root-owned admin script the daemon execs — the **same** file
/// `/etc/sudoers.d/gp-bar` lets the sudo path run (installed by
/// `scripts/setup-admin.sh`). It is the security boundary: validated args, a
/// root-only prefix and launchd discipline all live inside it, so neither
/// transport reimplements any of it.
///
/// Defined at the crate root — cross-platform, because the sudo path
/// aliases it from code the Linux CI compiles — and re-exported here so
/// the daemon keeps its one `proto::` namespace.
pub use crate::ADMIN_SCRIPT;

/// How long the daemon waits for one [`ADMIN_SCRIPT`] run before SIGKILLing
/// it and refusing the op.
///
/// Deliberately larger than the client's `ADMIN_OP_TIMEOUT` (30 s) so the
/// daemon never kills an op the app is still waiting for: on a slow machine
/// the app gives up first with a clear `Timeout`, and the script's own
/// internal bounds (~20 s worst case) stay far below this. Seeing a
/// `Failed` about *this* budget therefore means the app's timeout was
/// already saturating — two independent budgets, no shared clock.
pub const DAEMON_OP_BACKSTOP_SECS: u64 = 60;

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
/// The operation's arguments (array of strings). Empty for ops that take
/// none.
pub const KEY_ARGS: &CStr = c"args";
/// The admin script's stdout, trimmed, on a successful op (the contract the
/// sudo path already has: `Ok(String)` is what the script printed).
pub const KEY_OUT: &CStr = c"out";

// -- operations --------------------------------------------------------------------

/// The one operation that does no privileged work.
pub const OP_PING: &str = "ping";

/// The privileged operations the daemon **executes** by running
/// [`ADMIN_SCRIPT`] as root (Fase 2). Their contract is the script's own:
/// `selftest | start <portal> <0|1> [gateway|-] | stop | cancel | repair`.
pub const PRIVILEGED_OPS: [&str; 5] = ["selftest", "start", "stop", "cancel", "repair"];

/// Hard caps on what the daemon will even look at, checked before any
/// filesystem or process work: the script's longest shape is `start`'s four
/// arguments, so 8 leaves room for a future op without ever accepting an
/// unbounded request.
pub const MAX_ARGS: usize = 8;

/// Per-argument cap. Portal and gateway names are DNS-ish labels; 256 is far
/// above anything the script would accept, and bounds the memory one request
/// can make the root daemon copy.
pub const MAX_ARG_LEN: usize = 256;

/// Validate one request's `(op, args)` pair. **Pure** and cross-platform.
///
/// This is fail-fast, not the security boundary: [`ADMIN_SCRIPT`]
/// re-validates every argument itself. Its job is to keep an obviously bad
/// request from ever reaching the filesystem walk and the exec, and to make
/// the accepted shapes testable with no system at all.
///
/// The accepted grammar mirrors the script's (`valid_name` is a byte-for-byte
/// port of the shell function):
///
/// * `selftest | stop | cancel | repair` — zero arguments.
/// * `start <portal> <reuse> [<gateway>]` — `portal` a [`valid_name`],
///   `reuse` exactly `0` or `1`, `gateway` a [`valid_name`] or the literal
///   `-` (the script's "auto-gateway" sentinel).
///
/// Errors are stable `&'static str`s: the client prints them and tests match
/// them.
pub fn validate_op(op: &str, args: &[String]) -> Result<(), &'static str> {
    if args.len() > MAX_ARGS {
        return Err("too many arguments");
    }
    if args.iter().any(|arg| arg.len() > MAX_ARG_LEN) {
        return Err("argument too long");
    }
    match op {
        "start" => {
            if !(3..=4).contains(&args.len()) {
                return Err("start takes <portal> <0|1> [<gateway>]");
            }
            let portal = &args[0];
            let reuse = &args[1];
            if !valid_name(portal) {
                return Err("invalid portal name");
            }
            if reuse != "0" && reuse != "1" {
                return Err("invalid reuse flag");
            }
            // args[2] is the gateway (`$4` in the script; absent means the
            // script's own `-`). The 3..=4 arity allows one trailing
            // argument; it is validated with the same name rule rather than
            // waved through, so no unvalidated byte ever reaches the exec.
            for gateway in &args[2..] {
                if gateway != "-" && !valid_name(gateway) {
                    return Err("invalid gateway name");
                }
            }
            Ok(())
        }
        "selftest" | "stop" | "cancel" | "repair" => {
            if !args.is_empty() {
                return Err("operation takes no arguments");
            }
            Ok(())
        }
        _ => Err("unknown operation"),
    }
}

/// Port of `valid_name` in `scripts/gp-admin`: non-empty, no leading `-`
/// (gpclient's option parser would read it as an option — argument
/// injection), and only `[A-Za-z0-9._-]` (which also keeps the value
/// XML-safe for the job plist the script generates).
pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

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
    /// The admin script's stdout, trimmed, on a successful op. Empty for
    /// `ping` and for refusals — the same `Ok(String)` contract the sudo
    /// transport has always had.
    pub out: String,
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
            out: String::new(),
        }
    }

    /// A successful privileged op: the script's stdout, nothing else. The
    /// daemon's identity is deliberately not echoed here — the op result is
    /// what the caller asked for, and the sudo path returns exactly this.
    pub fn done(out: String) -> Self {
        Self {
            ok: true,
            out,
            ..Self::default()
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
        dict.set_string(KEY_OUT, &self.out);
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
            out: event.get_string(KEY_OUT).unwrap_or_default(),
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
    ///
    /// This is the **only** class that means "the daemon is not there" — the
    /// signature of the Fase-3 anomaly R1 reacts to (a job missing from the
    /// launchd domain).
    Unreachable,
    /// The event was not a reply dictionary at all.
    ///
    /// Deliberately mapped to `AdminError::Failed`, not `Unreachable`
    /// (Review Fase 1, note 1): a malformed reply proves the daemon *was*
    /// reached and spoke — the protocol is broken, not the job. R1 must not
    /// re-register a service that is answering; that repair belongs to
    /// R2/Fase 3. Keeping this out of `Unreachable` also keeps R1's trigger
    /// a single, unambiguous signature.
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
    /// Run [`ADMIN_SCRIPT`] as root with `op` and `args`.
    ///
    /// Only ever produced after [`validate_op`] accepted the pair, so the
    /// args handed to the exec are already bounded and charset-checked. The
    /// script re-validates them anyway.
    Exec { op: String, args: Vec<String> },
    /// Reply `ok: false` with a stable error.
    Refuse { error: &'static str },
}

/// Decide what to do with a request: `op` is the (optional) operation field,
/// `args` its argument list, `peer_allowed` the bundle-owner policy verdict
/// for the connection.
pub fn decide(op: Option<&str>, args: &[String], peer_allowed: bool) -> Decision {
    if !peer_allowed {
        // The peer's euid does not match the .app bundle owner: refuse with a
        // reply (so the client sees why) instead of dropping the connection.
        return Decision::Refuse {
            error: "peer not allowed (bundle-owner uid policy)",
        };
    }
    match op {
        Some(OP_PING) => Decision::Ping,
        Some(op) if PRIVILEGED_OPS.contains(&op) => match validate_op(op, args) {
            Ok(()) => Decision::Exec {
                op: op.to_string(),
                args: args.to_vec(),
            },
            Err(error) => Decision::Refuse { error },
        },
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
    fn ping_is_answered_and_privileged_ops_are_executed() {
        assert_eq!(decide(Some("ping"), &[], true), Decision::Ping);
        // Every privileged op reaches the exec once its args validate.
        for (op, args) in [
            ("selftest", vec![]),
            ("start", vec!["portal".into(), "1".into(), "-".into()]),
            ("stop", vec![]),
            ("cancel", vec![]),
            ("repair", vec![]),
        ] {
            assert_eq!(
                decide(Some(op), &args, true),
                Decision::Exec {
                    op: op.to_string(),
                    args,
                },
                "{op} must be executed"
            );
        }
    }

    #[test]
    fn privileged_ops_with_bad_arguments_are_refused_before_the_exec() {
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        for op in ["selftest", "stop", "cancel", "repair"] {
            assert_eq!(
                decide(Some(op), &args(&["extra"]), true),
                Decision::Refuse {
                    error: "operation takes no arguments"
                },
                "{op} takes no arguments"
            );
        }
        // `start` needs 3 or 4 args.
        for bad in [&[][..], &["p"], &["p", "1"], &["p", "1", "-", "g", "x"]] {
            assert_eq!(
                decide(Some("start"), &args(bad), true),
                Decision::Refuse {
                    error: "start takes <portal> <0|1> [<gateway>]"
                }
            );
        }
        // A rejection never reaches the exec: this is the fail-fast property.
        assert!(matches!(
            decide(Some("start"), &args(&["bad name", "0", "-"]), true),
            Decision::Refuse { .. }
        ));
    }

    #[test]
    fn validate_op_accepts_the_scripts_grammar() {
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert_eq!(validate_op("selftest", &[]), Ok(()));
        assert_eq!(validate_op("stop", &[]), Ok(()));
        assert_eq!(validate_op("cancel", &[]), Ok(()));
        assert_eq!(validate_op("repair", &[]), Ok(()));
        assert_eq!(
            validate_op("start", &args(&["vpn.example.com", "0", "-"])),
            Ok(())
        );
        assert_eq!(
            validate_op("start", &args(&["vpn.example.com", "1", "-"])),
            Ok(())
        );
        // The 3..=4 arity: a trailing argument is accepted, still validated.
        assert_eq!(
            validate_op("start", &args(&["vpn.example.com", "1", "-", "extra"])),
            Ok(())
        );
        assert_eq!(
            validate_op("start", &args(&["a_b-c.d", "0", "gw.example.com"])),
            Ok(())
        );
    }

    #[test]
    fn validate_op_rejects_arity_charset_and_caps() {
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        // Arity.
        assert_eq!(
            validate_op("stop", &args(&["x"])),
            Err("operation takes no arguments")
        );
        // Two args is below the minimum (`MAX_ARGS`/the upper bound are
        // covered under "Caps" below).
        assert_eq!(
            validate_op("start", &args(&["p", "1"])),
            Err("start takes <portal> <0|1> [<gateway>]")
        );
        // Charset / option injection / empty.
        for bad in ["", "-portal", "--gateway", "a b", "a/b", "a$b", "caffè"] {
            assert_eq!(
                validate_op("start", &args(&[bad, "0", "-"])),
                Err("invalid portal name"),
                "portal {bad:?} must be rejected"
            );
        }
        // Reuse is exactly 0 or 1.
        for bad in ["2", "-1", "", "true"] {
            assert_eq!(
                validate_op("start", &args(&["p", bad, "-"])),
                Err("invalid reuse flag"),
                "reuse {bad:?} must be rejected"
            );
        }
        // Gateway: `-` is the sentinel, anything else must be a valid name.
        for bad in ["", "--auto-gateway", "-x", "a b"] {
            assert_eq!(
                validate_op("start", &args(&["p", "0", bad])),
                Err("invalid gateway name"),
                "gateway {bad:?} must be rejected"
            );
        }
        // Caps: more than MAX_ARGS is refused before anything else, and so is
        // one over-long argument.
        assert_eq!(
            validate_op(
                "start",
                &args(&["p", "0", "-", "a", "b", "c", "d", "e", "f"])
            ),
            Err("too many arguments")
        );
        let long = "a".repeat(MAX_ARG_LEN + 1);
        assert_eq!(
            validate_op("start", &args(&[&long, "0", "-"])),
            Err("argument too long")
        );
        // A 4th argument is validated with the same name rule.
        assert_eq!(
            validate_op("start", &args(&["p", "0", "-", "bad name"])),
            Err("invalid gateway name")
        );
        // An unknown op is not validated as if it existed.
        assert_eq!(validate_op("frobnicate", &[]), Err("unknown operation"));
    }

    #[test]
    fn valid_name_matches_the_shell_function() {
        // `case "$1" in ''|-*|*[!A-Za-z0-9._-]*) return 1 ;; esac`
        for good in ["a", "A9", "vpn.example.com", "a_b-c.d", "0"] {
            assert!(valid_name(good), "{good:?} must be valid");
        }
        for bad in ["", "-", "-a", "a b", "a/b", "a:b", "a\\b", "caffè", "a\tb"] {
            assert!(!valid_name(bad), "{bad:?} must be invalid");
        }
    }

    #[test]
    fn anything_else_is_refused_with_a_stable_error() {
        assert_eq!(
            decide(Some("ping"), &[], false),
            Decision::Refuse {
                error: "peer not allowed (bundle-owner uid policy)",
            }
        );
        // A refused peer never learns which operations exist.
        assert_eq!(
            decide(Some("start"), &[], false),
            decide(Some("ping"), &[], false)
        );
        assert_eq!(
            decide(Some("frobnicate"), &[], true),
            Decision::Refuse {
                error: "unknown operation",
            }
        );
        assert_eq!(
            decide(Some(""), &[], true),
            Decision::Refuse {
                error: "unknown operation",
            }
        );
        assert_eq!(
            decide(None, &[], true),
            Decision::Refuse {
                error: "missing op"
            }
        );
    }

    #[test]
    fn a_refusal_roundtrips_through_the_typed_payload() {
        let refused = HelperReply::refused("invalid gateway name");
        assert!(!refused.ok);
        assert_eq!(refused.error.as_deref(), Some("invalid gateway name"));
        assert_eq!(refused.out, "");
        // A ping answer carries the daemon's identity and no error.
        let ping = HelperReply::ping(0, 42, "1.2.0", "ping");
        assert!(ping.ok);
        assert_eq!(ping.error, None);
        assert_eq!(ping.uid, 0);
        assert_eq!(ping.pid, 42);
        assert_eq!(ping.version, "1.2.0");
        assert_eq!(ping.echo, "ping");
        assert_eq!(ping.out, "");
        // A done answer carries only the script's stdout.
        let done = HelperReply::done("ok".to_string());
        assert!(done.ok);
        assert_eq!(done.error, None);
        assert_eq!(done.out, "ok");
        assert_eq!(done.uid, 0);
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

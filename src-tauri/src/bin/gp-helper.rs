//! `gp-helper` — GP Bar's privileged LaunchDaemon executable, on-demand via
//! launchd's Mach service bootstrap (`SMAppService`, Fase 3).
//!
//! **Fase 2: the ops are real.** The daemon answers `{op: "ping"}` with the
//! typed [`HelperReply`] payload and executes every privileged operation
//! (`selftest|start|stop|cancel|repair`) by running the root-owned
//! [`proto::ADMIN_SCRIPT`] — the very file `/etc/sudoers.d/gp-bar` lets the
//! sudo path run. No privileged logic is reimplemented here: one script, two
//! transports, zero divergence.
//!
//! Trust model, in order:
//!
//! 1. The peer's euid is snapshotted at **accept** time and must equal the
//!    bundle owner (`peer.rs`); a refused peer gets an error reply.
//! 2. That euid — never a value from the message — becomes `SUDO_UID`, with
//!    `SUDO_USER` resolved from it. This is the identity the script puts in
//!    the worker's plist so `gpauth` runs as the logged-in user: the daemon
//!    inherits sudo's identity channel, same semantics, audit-token source.
//! 3. The message contributes exactly `op` and `args`, validated here
//!    ([`proto::validate_op`]) and re-validated by the script, which remains
//!    the security boundary. Nothing from the message reaches a path, an
//!    environment key or an exec beyond those two.
//! 4. Before the exec the script's own `require_root_owned` check is repeated:
//!    every component of the resolved path must be root-owned and not
//!    group/world-writable, or a user-writable file would mean root execution.
//!
//! Design choices, all mapped to the review findings:
//!
//! * **No `KeepAlive`, no `RunAtLoad`** — strictly on-demand, matching
//!   [`proto::generate_daemon_plist`]. The idle-exit watchdog (below) makes
//!   the daemon exit after [`proto::DAEMON_IDLE_EXIT_SECS`] with zero open
//!   connections, so every ping forces launchd to start the job fresh.
//! * **Peer check from day one** (B2, minimal-but-real): the peer's euid must
//!   equal the uid owning the `.app` bundle directory, read at startup from
//!   the daemon's own executable path. Refused peers get an error reply (so a
//!   test run as the wrong user sees *why*). If the bundle stat fails,
//!   everyone is refused — fail closed. See `peer.rs` for the residual limits.
//! * **One serial dispatch queue** (B5) as the target queue of the listener
//!   and of every accepted peer: handlers never run concurrently, and the
//!   connection counter cannot race its own bookkeeping.
//! * **`catch_unwind` around every handler body** (B7): a panic in a
//!   C-invoked block aborts the process; instead the daemon answers with an
//!   error reply.
//! * **No leaks** (B9): handler blocks are dropped right after the `xpc_*`
//!   call that retains them; owned objects/connections are RAII (`xpc.rs`).
//! * **`dispatch_main()`** as the main loop (B11).
//!
//! Log discipline (S1/B3): stdout/stderr go to [`proto::DAEMON_LOG`]
//! (root-owned directory, opened by launchd as root) — never into a
//! user-writable path.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CStr;
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::Path;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use gp_bar::peer;
    use gp_bar::proto::{self, Decision, HelperReply};
    use gp_bar::xpc::{self, HandlerBlock, XpcEvent, XpcRaw};

    /// The daemon's whole mutable state: the number of open peer connections.
    /// The watchdog reads it; the handlers keep it exact.
    #[derive(Default)]
    struct State {
        open_connections: AtomicUsize,
    }

    pub fn run() {
        // SAFETY: `getuid` has no preconditions.
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            eprintln!(
                "gp-helper: refusing to run unprivileged (uid={uid}); this binary is a LaunchDaemon executable"
            );
            std::process::exit(1);
        }

        // Peer policy input: the uid owning the app bundle this daemon lives
        // in. `None` (stat failed / not running from a bundle) refuses
        // everyone.
        let owner = peer::current_bundle_root().and_then(|root| peer::bundle_owner_uid(&root));
        match owner {
            Some(owner) => eprintln!("gp-helper: bundle owner uid={owner}"),
            None => eprintln!("gp-helper: cannot resolve the bundle owner; refusing all peers"),
        }

        let Some(queue) = xpc::SerialQueue::new(c"gp-helper.serial") else {
            eprintln!("gp-helper: cannot create the serial dispatch queue");
            std::process::exit(1);
        };
        let listener = match xpc::XpcConnection::listen(proto::SERVICE_C) {
            Some(listener) => listener,
            None => {
                eprintln!(
                    "gp-helper: xpc_connection_create_mach_service failed for {}",
                    proto::SERVICE
                );
                std::process::exit(1);
            }
        };

        let state = Arc::new(State::default());
        spawn_watchdog(Arc::clone(&state));

        let listener_state = Arc::clone(&state);
        let listener_handler: HandlerBlock = new_listener_handler(owner, queue, listener_state);
        listener.set_target_queue(queue);
        listener.set_event_handler(listener_handler);
        listener.resume();
        eprintln!(
            "gp-helper: listening on {} (version {}, uid={uid}, idle exit after {}s)",
            proto::SERVICE,
            env!("CARGO_PKG_VERSION"),
            proto::DAEMON_IDLE_EXIT_SECS
        );

        // B11: the libdispatch main loop. Never returns; the watchdog's exit
        // is the only way out.
        xpc::run_forever();
    }

    /// Pure watchdog tick: idle seconds only grow while zero connections are
    /// open, and reset the moment one is. [`proto::DAEMON_IDLE_EXIT_SECS`]
    /// consecutive idle ticks mean "nobody is talking to me — exit and let
    /// launchd prove it can start me again".
    fn next_idle_secs(open_connections: usize, previous_idle_secs: u64) -> u64 {
        if open_connections == 0 {
            previous_idle_secs.saturating_add(1)
        } else {
            0
        }
    }

    fn spawn_watchdog(state: Arc<State>) {
        thread::spawn(move || {
            let mut idle_secs = 0u64;
            loop {
                thread::sleep(Duration::from_secs(1));
                let open = state.open_connections.load(Ordering::SeqCst);
                idle_secs = next_idle_secs(open, idle_secs);
                if idle_secs >= proto::DAEMON_IDLE_EXIT_SECS {
                    // Never taken while a connection is open: the counter is
                    // only zero here if it was zero at this tick too.
                    eprintln!(
                        "gp-helper: {idle_secs}s idle with no open connections; exiting (launchd restarts on demand)"
                    );
                    std::process::exit(0);
                }
            }
        });
    }

    /// Build the listener's accept handler. Separated from `run` to keep the
    /// block construction (and its captured state) auditable in one place.
    fn new_listener_handler(
        owner: Option<u32>,
        queue: xpc::SerialQueue,
        state: Arc<State>,
    ) -> HandlerBlock {
        block2::RcBlock::new(move |raw| {
            // SAFETY: `raw` is the event libxpc passed to this listener block;
            // valid for the duration of the invocation, owned by the runtime.
            let Some(event) = (unsafe { XpcEvent::from_borrowed(raw) }) else {
                eprintln!("gp-helper: null listener event");
                return;
            };
            if event.is_error() {
                eprintln!("gp-helper: listener error event");
                return;
            }
            let Some(peer_conn) = event.as_peer() else {
                eprintln!("gp-helper: non-connection listener event; ignoring");
                return;
            };

            // euid snapshot at accept time — fixed for the connection's life.
            let euid = peer_conn.euid();
            let allowed = gp_bar::peer::peer_allowed(euid, owner);
            eprintln!("gp-helper: peer euid={euid} allowed={allowed}");

            // Build the peer handler BEFORE bumping the counter: if the
            // allocation fails catastrophically, the counter must not leak a
            // ghost connection (a leaked +1 would keep the daemon alive
            // forever, masking the anomaly the idle exit exists to expose).
            let peer_state = Arc::clone(&state);
            let counted = Arc::new(AtomicBool::new(false));
            let counted_in_handler = Arc::clone(&counted);
            let peer_handler: HandlerBlock = block2::RcBlock::new(move |message| {
                handle_peer_event(&peer_state, &counted_in_handler, allowed, euid, message);
            });

            state.open_connections.fetch_add(1, Ordering::SeqCst);
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                peer_conn.set_target_queue(queue);
                peer_conn.accept(peer_handler);
            }));
            if outcome.is_err() {
                // B7: the accept path panicked before the peer handler took
                // over. Undo the counter bump so the watchdog stays honest,
                // then reject.
                if !counted.swap(true, Ordering::SeqCst) {
                    state.open_connections.fetch_sub(1, Ordering::SeqCst);
                }
                eprintln!("gp-helper: accept path panicked; rejecting the peer");
                let _ = catch_unwind(AssertUnwindSafe(|| peer_conn.cancel()));
            }
        })
    }

    /// One event on an accepted connection: the final error event (connection
    /// went invalid/cancelled — decrement the counter exactly once) or a
    /// message to answer.
    fn handle_peer_event(
        state: &Arc<State>,
        counted: &AtomicBool,
        allowed: bool,
        peer_euid: u32,
        raw: XpcRaw,
    ) {
        // SAFETY: `raw` is the event libxpc passed to this peer block; valid
        // for the duration of the invocation, owned by the runtime.
        let Some(event) = (unsafe { XpcEvent::from_borrowed(raw) }) else {
            return;
        };
        if event.is_error() {
            // Exactly one error event per connection (connection.h): swap
            // guards against a double decrement ever underflowing.
            if !counted.swap(true, Ordering::SeqCst) {
                state.open_connections.fetch_sub(1, Ordering::SeqCst);
                eprintln!(
                    "gp-helper: peer connection closed ({} open)",
                    state.open_connections.load(Ordering::SeqCst)
                );
            }
            return;
        }
        // B7: nothing inside `answer` may unwind through the C frame. On
        // panic, still try to give the client an error reply (create_reply is
        // safe to attempt: if the panicked path consumed it, we log).
        if catch_unwind(AssertUnwindSafe(|| answer(&event, allowed, peer_euid))).is_err() {
            eprintln!("gp-helper: handler panicked; attempting an error reply");
            let _ = catch_unwind(AssertUnwindSafe(|| refuse(&event, "internal panic")));
        }
    }

    /// Answer one request according to [`proto::decide`]. Panic-free by
    /// construction, but wrapped by the caller anyway (B7).
    ///
    /// An `Exec` decision runs the admin script **synchronously on the
    /// serial queue**, inside this handler. That is the deliberate tradeoff:
    /// the app already serializes admin ops (`vpn::ADMIN_OPS`) and the
    /// script's worst case (~20 s) stays inside the client's 30 s timeout, so
    /// no second thread, no new `Send`/unsafe claim, and — the real win — the
    /// connection stays open for the whole op, which means the idle-exit
    /// watchdog can never tear the daemon down mid-operation. A `ping`
    /// arriving during an op waits for it and still answers inside its own
    /// timeout.
    fn answer(event: &XpcEvent, allowed: bool, peer_euid: u32) {
        if !event.is_dictionary() {
            eprintln!("gp-helper: non-dictionary message; ignoring");
            return;
        }
        let op = event.get_string(proto::KEY_OP);
        let args = event.get_string_array(proto::KEY_ARGS);
        eprintln!(
            "gp-helper: op={} args={}",
            op.as_deref().unwrap_or("<missing>"),
            args.len()
        );
        match proto::decide(op.as_deref(), &args, allowed) {
            Decision::Ping => {
                // SAFETY: `getuid` has no preconditions.
                let uid = unsafe { libc::getuid() } as i64;
                let reply = HelperReply::ping(
                    uid,
                    std::process::id() as i64,
                    env!("CARGO_PKG_VERSION"),
                    proto::OP_PING,
                );
                send(event, &reply);
            }
            Decision::Exec { op, args } => match run_admin_script(&op, &args, peer_euid) {
                Ok(out) => {
                    eprintln!("gp-helper: {op} ok ({} bytes of output)", out.len());
                    send(event, &HelperReply::done(out));
                }
                Err(error) => {
                    eprintln!("gp-helper: {op} failed: {error}");
                    refuse(event, &error);
                }
            },
            Decision::Refuse { error } => refuse(event, error),
        }
    }

    /// Send `reply` on the request's channel, logging the failure a client
    /// will otherwise only see as a timeout.
    fn send(event: &XpcEvent, reply: &HelperReply) {
        let Some(dict) = event.create_reply() else {
            eprintln!("gp-helper: message carried no reply context");
            return;
        };
        reply.write_into(&dict);
        if !event.send_reply(&dict) {
            eprintln!("gp-helper: could not send the reply (no reply channel)");
        }
    }

    /// Reply `ok: false` with `error` on the request's channel.
    fn refuse(event: &XpcEvent, error: &str) {
        send(event, &HelperReply::refused(error));
    }

    // -- running the admin script --------------------------------------------------

    /// Run [`proto::ADMIN_SCRIPT`] as root with `op`/`args` and return its
    /// trimmed stdout, or the refusal text to hand back verbatim.
    ///
    /// `peer_euid` is the **accept-time** euid of the validated peer — the
    /// message never contributes identity.
    ///
    /// Blocking by design (see [`answer`]): the caller is the serial queue.
    fn run_admin_script(op: &str, args: &[String], peer_euid: u32) -> Result<String, String> {
        // The script is the security boundary, so the boundary itself must
        // still be the root-only file setup-admin.sh installed.
        require_root_owned(Path::new(proto::ADMIN_SCRIPT))?;

        // sudo's identity channel, reproduced: the uid comes from the audited
        // connection, the name from the uid via libc (fallback to the numeric
        // form — the script warns and carries on).
        let sudo_user = user_name(peer_euid).unwrap_or_else(|| peer_euid.to_string());
        let mut child = Command::new(proto::ADMIN_SCRIPT)
            .arg(op)
            .args(args)
            // A clean environment: nothing inherited from launchd, and the
            // sudo path's `LC_ALL`/`LANG` keep the script's error strings
            // unlocalized (the app's classification matches on them). No
            // `PATH` is set on purpose: every command the script runs
            // (`launchctl`, `stat`, `realpath`, `install`, ...) resolves in
            // sh's built-in default PATH (`/usr/bin:/bin:/usr/sbin:/sbin`),
            // so inheriting one would only add surface.
            .env_clear()
            .env("SUDO_UID", peer_euid.to_string())
            .env("SUDO_USER", &sudo_user)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot run {}: {error}", proto::ADMIN_SCRIPT))?;
        eprintln!("gp-helper: exec {op} (SUDO_UID={peer_euid}, SUDO_USER={sudo_user})");

        // Drain both pipes on their own threads BEFORE waiting: a child that
        // fills a pipe buffer would otherwise block forever while we wait for
        // its exit. `Stdio::piped()` above makes these always `Some`.
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the admin script produced no output pipes".to_string());
        };
        let stdout = drain(stdout);
        let stderr = drain(stderr);

        let status = wait_with_backstop(&mut child)?;
        // Both joins are guaranteed to finish: the child is gone, so the write
        // ends are closed and the readers see EOF.
        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        if status.success() {
            Ok(stdout)
        } else if stderr.is_empty() {
            // Same shape as the sudo path.
            Err(format!("admin op '{op}' failed: exit {status}"))
        } else {
            Err(stderr)
        }
    }

    /// Wait for `child` with the [`proto::DAEMON_OP_BACKSTOP_SECS`] deadline.
    ///
    /// On expiry only the script is SIGKILLed: its own children (the
    /// messenger) die with it, and the worker is launchd's child, never ours —
    /// the same discipline the script follows internally. Polling instead of
    /// a waiter thread keeps the `Child` here, where `kill` can reach it.
    fn wait_with_backstop(child: &mut Child) -> Result<ExitStatus, String> {
        let deadline = Instant::now() + Duration::from_secs(proto::DAEMON_OP_BACKSTOP_SECS);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("cannot wait for the admin script: {error}"));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the admin script did not finish within {}s",
                    proto::DAEMON_OP_BACKSTOP_SECS
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Read one pipe to EOF on its own thread, so the parent can wait without
    /// risking a full-pipe deadlock.
    fn drain<R: Read + Send + 'static>(mut pipe: R) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    }

    /// The name `uid` resolves to, via `getpwuid_r` (no global, no unsafe
    /// static): `None` when the uid has no passwd entry — the caller then
    /// falls back to the numeric form, which the script accepts with a warning.
    fn user_name(uid: u32) -> Option<String> {
        let mut buffer = vec![0u8; 1024];
        loop {
            // SAFETY: `passwd` is plain data (integers and pointers); zeroed
            // is a valid initial value and `getpwuid_r` fills it.
            let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: `entry` and `buffer` are live and distinct for the call;
            // getpwuid_r writes at most `buffer.len()` bytes into `buffer` and
            // sets `result` to `&entry` (or NULL).
            let status = unsafe {
                libc::getpwuid_r(
                    uid,
                    &mut entry,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            };
            if status == libc::ERANGE && buffer.len() < 64 * 1024 {
                let doubled = buffer.len() * 2;
                buffer.resize(doubled, 0);
                continue;
            }
            if status != 0 || result.is_null() || entry.pw_name.is_null() {
                return None;
            }
            // SAFETY: `pw_name` points into `buffer`, NUL-terminated by libc.
            let name = unsafe { CStr::from_ptr(entry.pw_name) };
            return Some(name.to_string_lossy().into_owned());
        }
    }

    /// One walked component's verdict input: `(uid, mode)`.
    type Component = (u32, u32);

    /// The first violation of the ownership policy, as an index into the
    /// walked components (root-most last).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ChainFailure {
        /// Not owned by root.
        NotRootOwned(usize),
        /// Group- or world-writable.
        Writable(usize),
    }

    /// Pure core of `require_root_owned` (a port of the shell function in
    /// `scripts/gp-admin`): every component of the resolved path must be
    /// owned by root and must not be group- or world-writable. Testable with
    /// no filesystem at all.
    fn chain_failure(components: &[Component]) -> Option<ChainFailure> {
        for (index, (uid, mode)) in components.iter().enumerate() {
            if *uid != 0 {
                return Some(ChainFailure::NotRootOwned(index));
            }
            // 0o022 == group-write | other-write, same bits the script's
            // `stat -f %Sp` pattern matches.
            if mode & 0o022 != 0 {
                return Some(ChainFailure::Writable(index));
            }
        }
        None
    }

    /// Stat-level shell around [`chain_failure`]: lstat the path (a symlink
    /// must itself be root-owned), canonicalize it, then stat every ancestor
    /// up to `/`.
    fn require_root_owned(path: &Path) -> Result<(), String> {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() && meta.uid() != 0 => {
                return Err(format!(
                    "{} is not root-owned — re-run setup-admin.sh",
                    path.display()
                ));
            }
            Ok(_) => {}
            Err(error) => return Err(format!("cannot stat {}: {error}", path.display())),
        }
        let resolved = fs::canonicalize(path)
            .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;

        let mut components: Vec<Component> = Vec::new();
        let mut walked: Vec<&Path> = Vec::new();
        let mut current = resolved.as_path();
        loop {
            let meta = fs::metadata(current)
                .map_err(|error| format!("cannot stat {}: {error}", current.display()))?;
            components.push((meta.uid(), meta.mode()));
            walked.push(current);
            match current.parent() {
                // `parent() == Some(self)` only for the root itself.
                Some(parent) if parent != current => current = parent,
                _ => break,
            }
        }

        match chain_failure(&components) {
            Some(ChainFailure::NotRootOwned(index)) => Err(format!(
                "{} is not root-owned — re-run setup-admin.sh",
                walked[index].display()
            )),
            Some(ChainFailure::Writable(index)) => Err(format!(
                "{} is group/world-writable — re-run setup-admin.sh",
                walked[index].display()
            )),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn idle_seconds_only_grow_while_nothing_is_connected() {
            assert_eq!(next_idle_secs(0, 0), 1);
            assert_eq!(next_idle_secs(0, 59), 60);
            assert_eq!(next_idle_secs(0, 60), 61);
        }

        #[test]
        fn an_open_connection_resets_the_idle_counter_to_zero() {
            // The one invariant that matters: never exit while a peer is
            // talking.
            assert_eq!(next_idle_secs(1, 59), 0);
            assert_eq!(next_idle_secs(3, 1_000), 0);
        }

        /// `(root, wheel-readable, 0o755)`: the shape every real component of
        /// the install prefix has.
        const OK: Component = (0, 0o755);
        /// Root-owned but group-writable: only root may write this chain.
        const GROUP_WRITABLE: Component = (0, 0o775);
        /// Root-owned, world-writable: the classic /tmp hole.
        const WORLD_WRITABLE: Component = (0, 0o777);
        /// A user-owned component: the case the check exists for.
        const USER_OWNED: Component = (501, 0o755);

        #[test]
        fn a_fully_root_owned_non_writable_chain_passes() {
            assert_eq!(chain_failure(&[]), None);
            assert_eq!(chain_failure(&[OK]), None);
            assert_eq!(chain_failure(&[OK, OK, OK, OK]), None);
            // 0o700 and 0o750 are root-only writes: still fine.
            assert_eq!(chain_failure(&[(0, 0o700), (0, 0o750)]), None);
        }

        #[test]
        fn the_first_violation_wins_and_carries_its_index() {
            // Deepest component (the script itself) checked first.
            assert_eq!(
                chain_failure(&[USER_OWNED, OK, OK]),
                Some(ChainFailure::NotRootOwned(0))
            );
            assert_eq!(
                chain_failure(&[OK, USER_OWNED, OK]),
                Some(ChainFailure::NotRootOwned(1))
            );
            assert_eq!(
                chain_failure(&[OK, OK, WORLD_WRITABLE]),
                Some(ChainFailure::Writable(2))
            );
            // Not-root-owned is reported before writability of the same entry,
            // so a user-owned 0o777 file reads as the ownership problem.
            assert_eq!(
                chain_failure(&[(501, 0o777)]),
                Some(ChainFailure::NotRootOwned(0))
            );
            // Group-write alone is already a violation.
            assert_eq!(
                chain_failure(&[OK, GROUP_WRITABLE]),
                Some(ChainFailure::Writable(1))
            );
            // World-read (0o755) is not a violation; world-write is.
            assert_ne!(chain_failure(&[OK]), Some(ChainFailure::Writable(0)));
            assert_eq!(
                chain_failure(&[(0, 0o757)]),
                Some(ChainFailure::Writable(0))
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    imp::run();
}

/// Linux CI builds this target too (`--all-targets`): fail loudly instead of
/// pretending the transport exists there.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("gp-helper: the SMAppService helper is macOS-only");
    std::process::exit(1);
}

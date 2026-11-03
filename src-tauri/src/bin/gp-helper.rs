//! `gp-helper` — GP Bar's privileged LaunchDaemon executable, on-demand via
//! launchd's Mach service bootstrap (`SMAppService`, Fase 2).
//!
//! **Transport phase only (Fase 1).** The daemon answers `{op: "ping"}` with
//! the typed [`HelperReply`] payload (`ok`, `uid`, `pid`, `version`, `echo`)
//! and answers every privileged operation (`selftest|start|stop|cancel|repair`)
//! with `ok: false` and [`proto::NOT_IMPLEMENTED`]. It spawns nothing, writes
//! nothing, touches no privileged API: what is being validated is the
//! SMAppService/launchd/XPC chain and nothing else.
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
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

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

        let queue = xpc::serial_queue(c"gp-helper.serial");
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
    fn new_listener_handler(owner: Option<u32>, queue: XpcRaw, state: Arc<State>) -> HandlerBlock {
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
                handle_peer_event(&peer_state, &counted_in_handler, allowed, message);
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
    fn handle_peer_event(state: &Arc<State>, counted: &AtomicBool, allowed: bool, raw: XpcRaw) {
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
        if catch_unwind(AssertUnwindSafe(|| answer(&event, allowed))).is_err() {
            eprintln!("gp-helper: handler panicked; attempting an error reply");
            let _ = catch_unwind(AssertUnwindSafe(|| refuse(&event, "internal panic")));
        }
    }

    /// Answer one request according to [`proto::decide`]. Panic-free by
    /// construction, but wrapped by the caller anyway (B7).
    fn answer(event: &XpcEvent, allowed: bool) {
        if !event.is_dictionary() {
            eprintln!("gp-helper: non-dictionary message; ignoring");
            return;
        }
        let op = event.get_string(proto::KEY_OP);
        eprintln!("gp-helper: op={}", op.as_deref().unwrap_or("<missing>"));
        match proto::decide(op.as_deref(), allowed) {
            Decision::Ping => {
                // SAFETY: `getuid` has no preconditions.
                let uid = unsafe { libc::getuid() } as i64;
                let reply = HelperReply::ping(
                    uid,
                    std::process::id() as i64,
                    env!("CARGO_PKG_VERSION"),
                    proto::OP_PING,
                );
                send(&event, &reply);
            }
            Decision::NotImplemented => {
                eprintln!("gp-helper: {op:?} is not implemented in the transport phase");
                send(&event, &HelperReply::refused(proto::NOT_IMPLEMENTED));
            }
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

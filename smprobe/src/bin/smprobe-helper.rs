//! `smprobe-helper` — the probe's echo LaunchDaemon (root, on-demand via
//! launchd's Mach service bootstrap).
//!
//! It answers exactly one operation, `{op: "ping"}`, with
//! `{ok: true, uid, pid, echo}` and refuses everything else with an error
//! reply. No VPN, no network, no spawning: the point is to exercise the
//! SMAppService/launchd/XPC chain and nothing else.
//!
//! Design choices, all mapped to the review findings:
//!
//! * **No `KeepAlive`, no `RunAtLoad`** — strictly on-demand. The
//!   idle-exit watchdog (below) makes the daemon exit after
//!   [`identity::DAEMON_IDLE_EXIT_SECS`] with zero open connections, so
//!   every client ping forces launchd to start the job fresh: continuous
//!   validation of the exact mechanism that died in anomaly D
//!   ("registered and Enabled, but the job never starts").
//! * **Peer check from day one** (review B2, minimal-but-real): the
//!   peer's euid must equal the uid owning the `.app` bundle directory,
//!   read at startup from the daemon's own executable path. Refused peers
//!   get an error reply (so a test run as the wrong user sees *why*). If
//!   the bundle stat fails, everyone is refused — fail closed. This is
//!   parity with a sudoers rule for that user, not stronger; see
//!   `peer.rs` for the documented residual limits.
//! * **`catch_unwind` around every handler body** (review B7): a panic in
//!   a C-invoked block aborts the process; instead the daemon answers
//!   with an error reply and keeps the counter honest.
//! * **No leaks** (review B9): handler blocks are dropped right after the
//!   `xpc_*` call that retains them; owned objects/connections are RAII
//!   (`xpc.rs`).
//! * **`dispatch_main()`** as the main loop (review B11): the daemon
//!   parks the main thread inside libdispatch, not a `loop { park() }`.
//!
//! Log discipline (review S1/B3): stdout/stderr go to
//! `/Library/Logs/gp-bar/smprobe-helper.log` (root-owned directory, opened
//! by launchd as root) — never into user-writable paths. The "0-byte
//! daemon log" is a first-class diagnostic for anomaly D: if this file has
//! no startup line after a ping, launchd never started the job.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use smprobe::{identity, peer, proto, xpc, xpc::HandlerBlock, xpc::XpcEvent};

/// The daemon's whole mutable state: the number of open peer connections.
/// The watchdog reads it; the handlers keep it exact.
#[derive(Default)]
struct State {
  open_connections: AtomicUsize,
}

fn main() {
  let uid = unsafe { libc::getuid() };
  if uid != 0 {
    eprintln!(
      "smprobe-helper: refusing to run unprivileged (uid={uid}); this binary is a LaunchDaemon executable"
    );
    std::process::exit(1);
  }

  // Peer policy input: the uid owning the app bundle this daemon lives in.
  // `None` (stat failed / not running from a bundle) refuses everyone.
  let owner = peer::current_bundle_root().and_then(|root| peer::bundle_owner_uid(&root));
  match owner {
    Some(owner) => eprintln!("smprobe-helper: bundle owner uid={owner}"),
    None => eprintln!("smprobe-helper: cannot resolve the bundle owner; refusing all peers"),
  }

  let listener = match xpc::XpcConnection::listen(identity::SERVICE) {
    Some(listener) => listener,
    None => {
      eprintln!(
        "smprobe-helper: xpc_connection_create_mach_service failed for {}",
        identity::SERVICE_STR
      );
      std::process::exit(1);
    }
  };

  let state = Arc::new(State::default());
  spawn_watchdog(Arc::clone(&state));

  let listener_state = Arc::clone(&state);
  let listener_handler: HandlerBlock = new_listener_handler(owner, listener_state);
  listener.set_event_handler(listener_handler);
  listener.resume();
  eprintln!(
    "smprobe-helper: listening on {} (uid={uid}, idle exit after {}s)",
    identity::SERVICE_STR,
    identity::DAEMON_IDLE_EXIT_SECS
  );

  // B11: the libdispatch main loop. Never returns; the watchdog's exit(0)
  // is the only way out.
  xpc::run_forever();
}

/// Pure watchdog tick: idle seconds only grow while zero connections are
/// open, and reset the moment one is. [`identity::DAEMON_IDLE_EXIT_SECS`]
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
      if idle_secs >= identity::DAEMON_IDLE_EXIT_SECS {
        // Never taken while a connection is open: the counter is only
        // zero here if it was zero at this tick too.
        eprintln!(
          "smprobe-helper: {idle_secs}s idle with no open connections; exiting (launchd restarts on demand)"
        );
        std::process::exit(0);
      }
    }
  });
}

/// Build the listener's accept handler. Separated from `main` to keep the
/// block construction (and its captured state) auditable in one place.
fn new_listener_handler(owner: Option<u32>, state: Arc<State>) -> HandlerBlock {
  use block2::RcBlock;
  RcBlock::new(move |raw| {
    // SAFETY: `raw` is the event libxpc passed to this listener block;
    // valid for the duration of the invocation, owned by the runtime.
    let Some(event) = (unsafe { XpcEvent::from_borrowed(raw) }) else {
      eprintln!("smprobe-helper: null listener event");
      return;
    };
    if event.is_error() {
      eprintln!("smprobe-helper: listener error event");
      return;
    }
    let Some(peer_conn) = event.as_peer() else {
      eprintln!("smprobe-helper: non-connection listener event; ignoring");
      return;
    };

    // euid snapshot at accept time — fixed for the connection's life.
    let euid = peer_conn.euid();
    let allowed = peer::peer_allowed(euid, owner);
    eprintln!("smprobe-helper: peer euid={euid} allowed={allowed}");

    // Build the peer handler BEFORE bumping the counter: if the allocation
    // fails catastrophically, the counter must not leak a ghost connection
    // (a leaked +1 would keep the daemon alive forever — masking the very
    // anomaly the probe exists to catch).
    let peer_state = Arc::clone(&state);
    let counted = Arc::new(AtomicBool::new(false));
    let counted_in_handler = Arc::clone(&counted);
    let peer_handler: HandlerBlock = RcBlock::new(move |message| {
      handle_peer_event(&peer_state, &counted_in_handler, allowed, message);
    });

    state.open_connections.fetch_add(1, Ordering::SeqCst);
    let outcome = catch_unwind(AssertUnwindSafe(|| peer_conn.accept(peer_handler)));
    if outcome.is_err() {
      // B7: the accept path panicked before the peer handler took over.
      // Undo the counter bump so the watchdog stays honest, then reject.
      if !counted.swap(true, Ordering::SeqCst) {
        state.open_connections.fetch_sub(1, Ordering::SeqCst);
      }
      eprintln!("smprobe-helper: accept path panicked; rejecting the peer");
      let _ = catch_unwind(AssertUnwindSafe(|| peer_conn.cancel()));
    }
  })
}

/// One event on an accepted connection: the final error event (connection
/// went invalid/cancelled — decrement the counter exactly once) or a
/// message to answer.
fn handle_peer_event(state: &Arc<State>, counted: &AtomicBool, allowed: bool, raw: xpc::XpcRaw) {
  // SAFETY: `raw` is the event libxpc passed to this peer block; valid for
  // the duration of the invocation, owned by the runtime.
  let Some(event) = (unsafe { XpcEvent::from_borrowed(raw) }) else {
    return;
  };
  if event.is_error() {
    // Exactly one error event per connection (connection.h): swap guards
    // against a double decrement ever underflowing the counter.
    if !counted.swap(true, Ordering::SeqCst) {
      state.open_connections.fetch_sub(1, Ordering::SeqCst);
      eprintln!(
        "smprobe-helper: peer connection closed ({} open)",
        state.open_connections.load(Ordering::SeqCst)
      );
    }
    return;
  }
  // B7: nothing inside `answer` may unwind through the C frame. On panic,
  // still try to give the client an error reply (create_reply is
  // idempotent-safe: if the panicked path already consumed it, we log).
  if catch_unwind(AssertUnwindSafe(|| answer(&event, allowed))).is_err() {
    eprintln!("smprobe-helper: handler panicked; attempting an error reply");
    let _ = catch_unwind(AssertUnwindSafe(|| refuse(&event, "internal panic")));
  }
}

/// Answer one request according to `proto::decide`. Panic-free by
/// construction, but wrapped by the caller anyway (B7).
fn answer(event: &XpcEvent, allowed: bool) {
  if !event.is_dictionary() {
    eprintln!("smprobe-helper: non-dictionary message; ignoring");
    return;
  }
  let op = event.get_string(proto::KEY_OP);
  eprintln!(
    "smprobe-helper: op={}",
    op.as_deref().unwrap_or("<missing>")
  );
  match proto::decide(op.as_deref(), allowed) {
    proto::PingDecision::Echo { echo } => {
      let Some(reply) = event.create_reply() else {
        eprintln!("smprobe-helper: message carried no reply context");
        return;
      };
      reply.set_bool(proto::KEY_OK, true);
      reply.set_int64(proto::KEY_UID, unsafe { libc::getuid() } as i64);
      reply.set_int64(proto::KEY_PID, std::process::id() as i64);
      reply.set_string(proto::KEY_ECHO, echo);
      if !event.send_reply(&reply) {
        eprintln!("smprobe-helper: could not send the reply (no reply channel)");
      }
    }
    proto::PingDecision::Refuse { error } => refuse(event, error),
  }
}

/// Reply `{ok: false, error}` on the request's channel.
fn refuse(event: &XpcEvent, error: &str) {
  let Some(reply) = event.create_reply() else {
    eprintln!("smprobe-helper: cannot refuse: message carried no reply context");
    return;
  };
  reply.set_bool(proto::KEY_OK, false);
  reply.set_string(proto::KEY_ERROR, error);
  if !event.send_reply(&reply) {
    eprintln!("smprobe-helper: could not send the refusal (no reply channel)");
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
    // The one invariant that matters: never exit while a peer is talking.
    assert_eq!(next_idle_secs(1, 59), 0);
    assert_eq!(next_idle_secs(3, 1_000), 0);
  }
}

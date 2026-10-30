//! The ping client: one `{op: "ping"}` request per call, parsed into Rust
//! types, with a hard timeout so a wedged daemon can never hang the
//! sampler.
//!
//! The request goes through the **asynchronous** reply API
//! (`xpc_connection_send_message_with_reply`) instead of the sync variant:
//! the sync call can block indefinitely (connection.h warns about it), and
//! a hung sampler is a silent probe. With the async API the reply (or the
//! connection error) arrives on a channel with `recv_timeout`, and the
//! late reply — if any — is dropped harmlessly.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;

use crate::xpc::{HandlerBlock, XpcConnection, XpcEvent, XpcObject};

use crate::{identity, proto};

/// A successful ping reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingOk {
  /// Real uid of the answering daemon — `0` proves the whole chain
  /// (LaunchDaemon via launchd, not a stray user process).
  pub uid: i64,
  /// Pid of the answering daemon.
  pub pid: i64,
  /// The echoed operation name (`ping`).
  pub echo: String,
}

/// Parse one reply event into a `Result`. Pure data extraction, panic-free
/// (it runs inside a C-invoked block: B7).
pub fn parse_reply(event: &XpcEvent) -> Result<PingOk, String> {
  // XPC_ERROR_* dictionaries are dictionaries too: recognize them first.
  if event.is_error() {
    return Err("the smprobe helper is unreachable (connection error)".to_string());
  }
  if !event.is_dictionary() {
    return Err("unexpected reply shape from the smprobe helper".to_string());
  }
  if !event.get_bool(proto::KEY_OK) {
    let error = event
      .get_string(proto::KEY_ERROR)
      .unwrap_or_else(|| "the smprobe helper reported a failure".to_string());
    return Err(error);
  }
  Ok(PingOk {
    uid: event.get_int64(proto::KEY_UID),
    pid: event.get_int64(proto::KEY_PID),
    echo: event.get_string(proto::KEY_ECHO).unwrap_or_default(),
  })
}

/// Send one ping and wait for the reply (up to
/// [`identity::PING_TIMEOUT_SECS`]).
pub fn ping() -> Result<PingOk, String> {
  let conn = XpcConnection::connect(identity::SERVICE)
    .ok_or_else(|| format!("cannot create a connection to {}", identity::SERVICE_STR))?;

  let message = XpcObject::dict_new();
  message.set_string(proto::KEY_OP, proto::OP_PING);

  let (tx, rx) = mpsc::channel::<Result<PingOk, String>>();
  let reply_block: HandlerBlock = {
    let tx = tx.clone();
    RcBlock::new(move |raw| {
      // SAFETY: `raw` is the reply object libxpc passed to this block;
      // valid for the duration of the invocation, owned by the runtime.
      let parsed = match unsafe { XpcEvent::from_borrowed(raw) } {
        Some(event) => parse_reply(&event),
        None => Err("empty reply event from the smprobe helper".to_string()),
      };
      // The receiver may already be gone (timeout won the race): drop it.
      let _ = tx.send(parsed);
    })
  };
  conn.send_message_with_reply(&message, reply_block);
  // `reply_block` is dropped here: libxpc holds its own copy until the
  // reply fires (B9). Same for `message` and, at the end of the function,
  // `conn` (cancelled + released).

  match rx.recv_timeout(Duration::from_secs(identity::PING_TIMEOUT_SECS)) {
    Ok(result) => result,
    Err(_) => Err(format!(
      "ping to {} timed out after {}s",
      identity::SERVICE_STR,
      identity::PING_TIMEOUT_SECS
    )),
  }
}

// No unit tests here on purpose: `parse_reply` needs a live XPC event and
// `ping` needs the daemon, and a test that cannot fail documents nothing
// (review B11). The end-to-end round trip is what PROTOCOL.md step 3
// verifies (`ok uid=0 ... echo=ping`); the pure decision logic the reply
// encodes is covered by `proto::decide` tests.

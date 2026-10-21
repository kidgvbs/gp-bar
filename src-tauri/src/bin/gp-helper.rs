//! gp-helper: the GP Bar privileged LaunchDaemon (root, registered via
//! SMAppService, booted on demand by launchd through its Mach service).
//!
//! It executes ONLY fixed operations with scalar arguments:
//!
//! * `start` — resolve `gpclient` (console user's config override, then the
//!   Homebrew candidates: mirrors `config::Config::resolve_gpclient`), spawn
//!   it DIRECTLY (`Command`, never `sh -c`) as a child of this daemon, with
//!   stdout/stderr on the connect log and the `SUDO_UID`/`SUDO_USER` pair of
//!   the console user in its environment (the SAML `--browser` flow drops
//!   privileges through that pair; without it the browser auth dies with
//!   "… cannot be run as root").
//! * `stop` — `gpclient disconnect`, then poll `/var/run/gpclient.lock` for
//!   the worker's own cleanup; fail LOUD if it never goes (never SIGKILL: a
//!   killed worker skips the vpnc-script teardown and leaves the system DNS
//!   pointing at VPN resolvers).
//! * `cancel` — SIGTERM the connecting worker and wait for its exit.
//! * `selftest` — reply with the daemon uid (0 proves the chain).
//!
//! No paths travel over the wire; the portal is re-validated here with the
//! same charset as the app (`proto::validate_portal`).
//!
//! Peer validation: pragmatic. The modern `xpc_peer_requirement` machinery
//! is useless for ad-hoc signed apps (a Team ID requirement cannot be
//! satisfied without a real developer account). We only reject peers with
//! audit session id 0 as a tripwire. RESIDUAL RISK, accepted for a personal
//! tool: any process of any logged-in user can ask this root daemon to
//! (dis)connect the VPN.

#[path = "../proto.rs"]
mod proto;
#[path = "../xpc.rs"]
mod xpc;

use std::{
  fs,
  os::unix::fs::PermissionsExt,
  path::{Path, PathBuf},
  process::{Command, Stdio},
  sync::{Arc, Condvar, Mutex},
  time::{Duration, Instant},
};

use block2::RcBlock;

/// `Mutex<bool>` + `Condvar`: set once the reaper collected the worker.
type ExitPair = (Mutex<bool>, Condvar);

struct Worker {
  pid: u32,
  exited: Arc<ExitPair>,
}

#[derive(Default)]
struct State {
  worker: Mutex<Option<Worker>>,
}

fn main() {
  let uid = unsafe { libc::getuid() };
  if uid != 0 {
    eprintln!("gp-helper: refusing to run unprivileged (uid={uid}); this binary is meant to be a LaunchDaemon");
    std::process::exit(1);
  }

  let listener = xpc::create_listener(proto::SERVICE);
  if listener.is_null() {
    eprintln!("gp-helper: xpc_connection_create_mach_service failed");
    std::process::exit(1);
  }

  let state = Arc::new(State::default());
  let listener_state = state.clone();
  let listener_handler: RcBlock<dyn Fn(xpc::xpc_object_t)> = RcBlock::new(move |event| {
    if xpc::is_connection(event) {
      let client = event as xpc::xpc_connection_t;
      // Tripwire peer check (see the module doc for the residual risk).
      let asid = xpc::asid(client);
      if asid == 0 {
        eprintln!("gp-helper: rejecting a peer with audit session id 0");
        xpc::cancel(client);
        return;
      }
      let peer_state = listener_state.clone();
      let peer: RcBlock<dyn Fn(xpc::xpc_object_t)> = RcBlock::new(move |message| {
        handle_message(&peer_state, message);
      });
      xpc::set_event_handler(client, peer);
      xpc::resume(client);
    } else if xpc::is_error(event) {
      eprintln!("gp-helper: listener error event");
    }
  });

  xpc::set_event_handler(listener, listener_handler);
  xpc::resume(listener);
  eprintln!(
    "gp-helper: listening on {} (uid={uid})",
    proto::SERVICE.to_string_lossy()
  );

  // The listener's blocks run on libxpc's own queue; park the main thread.
  loop {
    std::thread::park();
  }
}

// -- replies ------------------------------------------------------------------------

/// Reply `{ok, error?, pid, uid}` on the request's remote connection.
fn reply(state: &State, message: xpc::xpc_object_t, ok: bool, error: Option<&str>) {
  let worker_pid = state
    .worker
    .lock()
    .unwrap()
    .as_ref()
    .map(|w| w.pid as i64)
    .unwrap_or(0);
  let reply = xpc::make_reply(message);
  if reply.is_null() {
    eprintln!("gp-helper: message carried no reply context (one-way send)");
    return;
  }
  xpc::dict_set_bool(reply, proto::KEY_OK, ok);
  if let Some(error) = error {
    xpc::dict_set_string(reply, proto::KEY_ERROR, error);
  }
  xpc::reply_set_int64(reply, proto::KEY_PID, worker_pid);
  xpc::reply_set_int64(reply, proto::KEY_UID, unsafe { libc::getuid() } as i64);
  let remote = xpc::remote_connection(message);
  xpc::send(remote, reply);
  xpc::release(reply);
}

fn handle_message(state: &Arc<State>, message: xpc::xpc_object_t) {
  if !xpc::is_dictionary(message) {
    eprintln!("gp-helper: peer event is not a dictionary");
    return;
  }
  let op = xpc::dict_get_string(message, proto::KEY_OP).unwrap_or_default();
  eprintln!("gp-helper: op={op}");
  let (ok, error) = match op.as_str() {
    proto::OP_START => {
      let portal = xpc::dict_get_string(message, proto::KEY_PORTAL).unwrap_or_default();
      let reuse = xpc::dict_get_bool(message, proto::KEY_REUSE_COOKIE);
      match start(state, &portal, reuse) {
        Ok(()) => (true, None),
        Err(error) => (false, Some(error)),
      }
    }
    proto::OP_STOP => match stop(state) {
      Ok(()) => (true, None),
      Err(error) => (false, Some(error)),
    },
    proto::OP_CANCEL => match cancel(state) {
      Ok(()) => (true, None),
      Err(error) => (false, Some(error)),
    },
    proto::OP_SELFTEST => (true, None),
    _ => (false, Some("unknown operation".to_string())),
  };
  reply(state, message, ok, error.as_deref());
}

// -- console user / paths ------------------------------------------------------------

/// (uid, name) of the console user, from the ownership of `/dev/console`.
fn console_user() -> Option<(u32, String)> {
  let read = |flag: &str| -> Option<String> {
    let out = Command::new("/usr/bin/stat")
      .args(["-f", flag, "/dev/console"])
      .output()
      .ok()?;
    if !out.status.success() {
      return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() || value.contains(char::is_whitespace) {
      return None;
    }
    Some(value)
  };
  Some((read("%u")?.parse().ok()?, read("%Su")?))
}

/// Home directory of `uid`. Root can read any passwd entry; the pointer is
/// copied immediately (getpwuid returns a static buffer).
fn home_of(uid: u32) -> Option<PathBuf> {
  unsafe {
    let pw = libc::getpwuid(uid);
    if pw.is_null() {
      return None;
    }
    let dir = std::ffi::CStr::from_ptr((*pw).pw_dir)
      .to_string_lossy()
      .into_owned();
    Some(PathBuf::from(dir))
  }
}

/// Where the app's log tailer expects the connect log: the console user's
/// `~/Library/Logs/gp-bar/gpclient-connect.log` (see `config::Config`).
fn connect_log_path(home: &Path) -> PathBuf {
  home
    .join("Library")
    .join("Logs")
    .join("gp-bar")
    .join("gpclient-connect.log")
}

// -- operations ---------------------------------------------------------------------

fn start(state: &Arc<State>, portal: &str, reuse_cookie: bool) -> Result<(), String> {
  proto::validate_portal(portal)?;

  // Defense in depth: the app only calls start while idle, and a worker
  // started elsewhere (terminal) holds the lock file anyway.
  if std::path::Path::new(proto::LOCK_FILE).exists() {
    return Err("a gpclient worker already holds the lock file".to_string());
  }

  let (uid, name) =
    console_user().ok_or_else(|| "cannot determine the console user".to_string())?;
  let home = home_of(uid).ok_or_else(|| "cannot resolve the console user's home".to_string())?;

  let gpclient = proto::resolve_gpclient()
    .ok_or_else(|| "gpclient not found (Homebrew locations only; the root daemon must not execute user-configured paths)".to_string())?;

  let log_path = connect_log_path(&home);
  if let Some(dir) = log_path.parent() {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
  }
  // Truncate like the old `: > log` in the admin script: the app's log
  // tailer rewinds on shrink, so this marks a fresh connection attempt.
  let log = fs::OpenOptions::new()
    .create(true)
    .write(true)
    .truncate(true)
    .open(&log_path)
    .map_err(|e| format!("cannot truncate {}: {e}", log_path.display()))?;
  // Keep it readable by the user app that tails it (the daemon's umask is
  // not guaranteed to be 022).
  let _ = fs::set_permissions(&log_path, fs::Permissions::from_mode(0o644));

  let argv = proto::connect_argv(&gpclient, portal, reuse_cookie);
  let log_err = log
    .try_clone()
    .map_err(|e| format!("cannot clone the log handle: {e}"))?;
  let mut child = Command::new(&argv[0])
    .args(&argv[1..])
    // HOME keeps the root worker's cookie cache where it always lived
    // (/var/root/.config/gpclient) so --cookie-cache stays useful.
    .env("HOME", "/var/root")
    .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin")
    // The SAML `--browser` auth helper drops privileges through this pair
    // (gpapi resolves the real user from SUDO_UID/PKEXEC_UID/DOAS_USER):
    // without it every browser-auth connect dies with
    // "… cannot be run as root" as soon as the SSO cookie expires.
    .env("SUDO_UID", uid.to_string())
    .env("SUDO_USER", &name)
    .stdout(Stdio::from(log))
    .stderr(Stdio::from(log_err))
    .spawn()
    .map_err(|e| format!("cannot spawn the gpclient worker: {e}"))?;
  let pid = child.id();

  let exited: Arc<ExitPair> = Arc::new((Mutex::new(false), Condvar::new()));
  let mut slot = state.worker.lock().unwrap();
  if let Some(previous) = slot.as_ref() {
    if !is_exited(&previous.exited) {
      return Err(format!(
        "a gpclient worker is already running (pid {})",
        previous.pid
      ));
    }
  }
  *slot = Some(Worker {
    pid,
    exited: exited.clone(),
  });
  drop(slot);

  // Reap the child (root would otherwise accumulate a zombie per connect)
  // and forget the worker bookkeeping once it exits.
  let reaper_state = state.clone();
  std::thread::spawn(move || {
    let _ = child.wait();
    let (lock, cvar) = &*exited;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
    let mut slot = reaper_state.worker.lock().unwrap();
    if slot.as_ref().is_some_and(|w| w.pid == pid) {
      *slot = None;
    }
  });

  eprintln!("gp-helper: started gpclient worker pid={pid} portal={portal}");
  Ok(())
}

fn is_exited(exited: &Arc<ExitPair>) -> bool {
  *exited.0.lock().unwrap()
}

/// `gpclient disconnect` + poll the lock file, failing loud (never SIGKILL).
fn stop(_state: &Arc<State>) -> Result<(), String> {
  let gpclient = proto::resolve_gpclient().ok_or_else(|| "gpclient not found".to_string())?;

  let output = Command::new(&gpclient)
    .arg("disconnect")
    .output()
    .map_err(|e| format!("cannot run gpclient disconnect: {e}"))?;
  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    return Err(if stderr.is_empty() {
      format!("gpclient disconnect exited {}", output.status)
    } else {
      stderr
    });
  }

  // gpclient exits 0 whether or not the signal was acted on: verify the
  // worker's own cleanup against the lock file instead.
  let deadline = Instant::now() + Duration::from_secs(proto::GRACEFUL_STOP_TIMEOUT_SECS);
  while std::path::Path::new(proto::LOCK_FILE).exists() {
    if Instant::now() >= deadline {
      return Err("the VPN worker did not stop (lock file still present)".to_string());
    }
    std::thread::sleep(Duration::from_millis(500));
  }
  Ok(())
}

/// SIGTERM the connecting worker and wait for its exit; no SIGKILL.
fn cancel(state: &Arc<State>) -> Result<(), String> {
  let worker = state.worker.lock().unwrap().take();
  let Some(worker) = worker else {
    // Idempotent: nothing we spawned is connecting (the app restarted, or
    // the worker already reaped itself).
    return Ok(());
  };
  if is_exited(&worker.exited) {
    return Ok(());
  }
  // SIGTERM: the worker tears the tunnel down on this signal. Never
  // SIGKILL — a killed worker skips the vpnc-script teardown.
  let rc = unsafe { libc::kill(worker.pid as i32, libc::SIGTERM) };
  if rc != 0 {
    return Err(format!(
      "cannot signal the worker (pid {}): errno {}",
      worker.pid, rc
    ));
  }
  let deadline = Instant::now() + Duration::from_secs(proto::GRACEFUL_STOP_TIMEOUT_SECS);
  let (lock, cvar) = &*worker.exited;
  let mut exited = lock.lock().unwrap();
  while !*exited {
    let now = Instant::now();
    if now >= deadline {
      return Err(format!(
        "the worker (pid {}) did not exit after SIGTERM",
        worker.pid
      ));
    }
    let (guard, _) = cvar
      .wait_timeout(exited, deadline.saturating_duration_since(now))
      .unwrap();
    exited = guard;
  }
  drop(exited);
  eprintln!("gp-helper: canceled worker pid={}", worker.pid);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn console_user_looks_like_a_pair() {
    // Info-dependent, but on any sane macOS login: uid >= 500, short name.
    if let Some((uid, name)) = console_user() {
      assert!(uid >= 500, "unexpected console uid {uid}");
      assert!(!name.is_empty() && !name.contains(char::is_whitespace));
    }
  }

  #[test]
  fn connect_log_path_matches_the_app_config_layout() {
    let path = connect_log_path(Path::new("/Users/test"));
    assert_eq!(
      path,
      PathBuf::from("/Users/test/Library/Logs/gp-bar/gpclient-connect.log")
    );
  }
}

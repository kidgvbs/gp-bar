//! gp-helper: the GP Bar privileged LaunchDaemon (root, registered via
//! SMAppService, booted on demand by launchd through its Mach service).
//!
//! It executes ONLY fixed operations with scalar arguments:
//!
//! * `start` — resolve `gpclient` (Homebrew candidates only: a root daemon
//!   must not execute user-configured paths), validate the portal, then
//!   generate and LOAD a one-shot launchd job running the connect. The
//!   worker is born a DIRECT child of launchd: on macOS 27.0.1 (26A434)
//!   root workers born any other way (authtrampoline, or a child of this
//!   daemon) are signal-deaf and can never be disconnected gracefully
//!   (RECAP.md). stdout/stderr land on the user's connect log (the job's
//!   StandardOutPath), with the `SUDO_UID`/`SUDO_USER` pair of the console
//!   user in the environment (the SAML `--browser` flow drops privileges
//!   through that pair).
//! * `stop` — `gpclient disconnect`, then poll `/var/run/gpclient.lock`
//!   for the worker's own cleanup; fail LOUD if it never goes (never
//!   SIGKILL: a killed worker skips the vpnc-script teardown and leaves
//!   the system DNS pointing at VPN resolvers), then unload the job.
//! * `cancel` — unload the one-shot job: launchd terminates the worker,
//!   which runs its own teardown instead of a blind kill.
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
  process::Command,
  sync::Arc,
  time::{Duration, Instant},
};

use block2::RcBlock;

/// No daemon-side bookkeeping: the worker lives in a launchd job (pid from
/// the lock file), so there is no child to reap or remember.
#[derive(Default)]
struct State {}

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
fn reply(_state: &State, message: xpc::xpc_object_t, ok: bool, error: Option<&str>) {
  let reply = xpc::make_reply(message);
  if reply.is_null() {
    eprintln!("gp-helper: message carried no reply context (one-way send)");
    return;
  }
  xpc::dict_set_bool(reply, proto::KEY_OK, ok);
  if let Some(error) = error {
    xpc::dict_set_string(reply, proto::KEY_ERROR, error);
  }
  xpc::reply_set_int64(reply, proto::KEY_PID, 0);
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

fn start(_state: &Arc<State>, portal: &str, reuse_cookie: bool) -> Result<(), String> {
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
  // The job's StandardOutPath then appends the worker's output to it.
  fs::write(&log_path, b"").map_err(|e| format!("cannot truncate {}: {e}", log_path.display()))?;
  let _ = fs::set_permissions(&log_path, fs::Permissions::from_mode(0o644));

  // Generate the one-shot launchd job and let LAUNCHD run the connect: a
  // worker born as this daemon's child would be signal-deaf on this macOS
  // beta (see the module doc), and one born directly of launchd stays
  // signalable — `gpclient disconnect` can then actually stop it.
  let job_dir = PathBuf::from("/Library/Application Support/GP Bar S1");
  let worker_plist = job_dir.join("gp-worker.plist");
  fs::create_dir_all(&job_dir).map_err(|e| format!("cannot create {}: {e}", job_dir.display()))?;
  fs::write(
    &worker_plist,
    proto::build_worker_plist(
      &gpclient,
      portal,
      reuse_cookie,
      &log_path,
      &uid.to_string(),
      &name,
    ),
  )
  .map_err(|e| format!("cannot write {}: {e}", worker_plist.display()))?;
  let _ = fs::set_permissions(&worker_plist, fs::Permissions::from_mode(0o644));

  // Idempotent load (leading unload recovers from a worker that exited
  // outside GP Bar); `launchctl` failing here means the job never started.
  let _ = Command::new("/bin/launchctl")
    .args(["unload", "-w"])
    .arg(&worker_plist)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status();
  let loaded = Command::new("/bin/launchctl")
    .args(["load", "-w"])
    .arg(&worker_plist)
    .output()
    .map_err(|e| format!("cannot run launchctl: {e}"))?;
  if !loaded.status.success() {
    return Err(format!(
      "launchctl load failed: {}",
      String::from_utf8_lossy(&loaded.stderr).trim()
    ));
  }

  eprintln!(
    "gp-helper: loaded worker job {} portal={portal}",
    proto::WORKER_LABEL
  );
  Ok(())
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
  unload_worker_job();
  Ok(())
}

/// Unload the one-shot worker job. Used by `stop` (cleanup) and `cancel`
/// (launchd then terminates the connecting worker, which runs its own
/// teardown instead of a blind kill — never SIGKILL).
fn unload_worker_job() {
  let _ = Command::new("/bin/launchctl")
    .args(["unload", "-w"])
    .arg(proto::WORKER_PLIST)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status();
}

/// Unload the connecting worker's job: launchd terminates it, which runs its
/// own teardown instead of a blind kill.
fn cancel(_state: &Arc<State>) -> Result<(), String> {
  unload_worker_job();
  eprintln!("gp-helper: unloaded worker job {}", proto::WORKER_LABEL);
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

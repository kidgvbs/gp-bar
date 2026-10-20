//! VPN orchestration: drives the open-source `gpclient` CLI with
//! administrator privileges and monitors the connection from user space.
//!
//! The CLI is the component with no connection time limits; this module only
//! starts it, watches its PID lock file and tails its JSON log lines:
//!
//! * connect: `/usr/bin/osascript` shows the admin password dialog, then runs
//!   `gpclient --log-format json connect <portal> --browser --auto-gateway
//!   [--cookie-cache] > <log> 2>&1 & echo $!` as root. The shell returns the
//!   worker PID immediately; gpclient keeps running as root under launchd.
//! * status: `/var/run/gpclient.lock` is written once the tunnel is up and
//!   removed on disconnect (it is world-readable), so its existence plus the
//!   liveness of the PID it contains is the source of truth.
//! * disconnect: `gpclient disconnect` signals the worker for a graceful
//!   teardown of the tunnel.

use std::{
  fs,
  path::Path,
  process::Stdio,
  sync::{Arc, Mutex},
  time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tauri::{AppHandle, Emitter};

use crate::{config::Config, osascript};

pub const LOCK_FILE: &str = "/var/run/gpclient.lock";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TAIL_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
  Disconnected,
  Connecting,
  Connected,
  Disconnecting,
  Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VpnState {
  pub phase: Phase,
  pub portal: Option<String>,
  pub gateway: Option<String>,
  pub connected_since_epoch_secs: Option<i64>,
  pub pid: Option<u32>,
  pub last_error: Option<String>,
}

impl Default for VpnState {
  fn default() -> Self {
    Self {
      phase: Phase::Disconnected,
      portal: None,
      gateway: None,
      connected_since_epoch_secs: None,
      pid: None,
      last_error: None,
    }
  }
}

/// Shared, lock-protected inner state.
#[derive(Default)]
struct Inner {
  state: VpnState,
  /// PID of the root gpclient worker we started, when connecting.
  worker_pid: Option<u32>,
}

pub struct VpnController {
  inner: Mutex<Inner>,
}

impl VpnController {
  pub fn new() -> Arc<Self> {
    Arc::new(Self {
      inner: Mutex::new(Inner::default()),
    })
  }

  pub fn snapshot(&self) -> VpnState {
    self.inner.lock().unwrap().state.clone()
  }

  pub fn connected_worker_pid(&self) -> Option<u32> {
    self.inner.lock().unwrap().worker_pid
  }

  fn update(&self, app: &AppHandle, mutate: impl FnOnce(&mut Inner)) -> VpnState {
    let state = {
      let mut inner = self.inner.lock().unwrap();
      let before = inner.state.clone();
      mutate(&mut inner);
      if inner.state == before {
        // Nothing observable changed (e.g. a poll tick with the same
        // phase): skip the event instead of waking every listener.
        return before;
      }
      inner.state.clone()
      // The guard is dropped here, BEFORE emitting. `Emitter::emit` runs
      // Rust listeners synchronously on this thread; emitting while still
      // holding the guard self-deadlocked the poller the moment any
      // listener read the state back through `snapshot()`, freezing the
      // main thread on its next `get_state` and stalling the whole
      // NSApplication run loop (dead tray, dead window).
    };
    let _ = app.emit("vpn-state", &state);
    state
  }

  // -- connect ---------------------------------------------------------------

  pub fn begin_connect(&self, app: &AppHandle, portal: String) {
    self.update(app, |inner| {
      inner.state.phase = Phase::Connecting;
      inner.state.portal = Some(portal.clone());
      inner.state.gateway = None;
      inner.state.last_error = None;
      inner.state.connected_since_epoch_secs = None;
    });
  }

  /// Called when the privileged shell has started (or failed to start).
  pub fn connect_started(&self, app: &AppHandle, worker_pid: Option<u32>) {
    self.update(app, |inner| {
      inner.worker_pid = worker_pid;
    });
  }

  pub fn connect_failed(&self, app: &AppHandle, error: String) {
    self.update(app, |inner| {
      inner.state.phase = Phase::Error;
      inner.state.last_error = Some(error);
      inner.worker_pid = None;
    });
  }

  // -- disconnect ------------------------------------------------------------
  pub fn begin_disconnect(&self, app: &AppHandle) {
    self.update(app, |inner| {
      if inner.state.phase == Phase::Connected {
        inner.state.phase = Phase::Disconnecting;
      }
    });
  }

  // -- observations from the poller -------------------------------------------

  /// Apply filesystem evidence (lock file present/absent, PID alive/dead).
  pub fn observe_lock(&self, app: &AppHandle, lock: Option<LockObservation>) {
    self.update(app, |inner| match (lock, &inner.state.phase) {
      (Some(obs), Phase::Disconnected | Phase::Error) => {
        // A gpclient worker is running although we did not start it this
        // session (e.g. started from a terminal, or the app restarted).
        inner.state.phase = Phase::Connected;
        inner.state.connected_since_epoch_secs = Some(obs.connected_since_epoch_secs);
        inner.state.pid = Some(obs.pid);
        if inner.worker_pid.is_none() {
          inner.worker_pid = Some(obs.pid);
        }
      }
      (Some(obs), Phase::Connecting) => {
        inner.state.phase = Phase::Connected;
        inner.state.connected_since_epoch_secs = Some(obs.connected_since_epoch_secs);
        inner.state.pid = Some(obs.pid);
      }
      (Some(_), Phase::Connected | Phase::Disconnecting) => {}
      (None, Phase::Connected) => {
        // The lock disappeared: gpclient exited and cleaned up.
        inner.state.phase = Phase::Disconnected;
        inner.state.connected_since_epoch_secs = None;
        inner.state.pid = None;
        inner.state.gateway = None;
        inner.worker_pid = None;
      }
      (None, Phase::Disconnecting) => {
        inner.state.phase = Phase::Disconnected;
        inner.state.connected_since_epoch_secs = None;
        inner.state.pid = None;
        inner.state.gateway = None;
        inner.worker_pid = None;
      }
      (None, _) => {}
    });
  }

  /// A parsed gateway name from the JSON log stream.
  pub fn observe_gateway(&self, app: &AppHandle, gateway: Option<String>) {
    if gateway.is_none() {
      return;
    }
    self.update(app, |inner| {
      inner.state.gateway = gateway;
    });
  }
}

/// What the PID lock file tells us right now.
pub struct LockObservation {
  pub pid: u32,
  /// Lock file mtime; when gpclient wrote it, i.e. when the tunnel went up.
  pub connected_since_epoch_secs: i64,
}

pub fn read_lock_observation() -> Option<LockObservation> {
  let raw = fs::read_to_string(LOCK_FILE).ok()?;
  let pid: u32 = raw.trim().parse().ok()?;
  if !process_alive(pid) {
    return None;
  }
  let connected_since = fs::metadata(LOCK_FILE)
    .and_then(|meta| meta.modified())
    .map(|time| {
      time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
    })
    .unwrap_or(0);
  Some(LockObservation {
    pid,
    connected_since_epoch_secs: connected_since,
  })
}

/// `ps -p <pid>` avoids linking libc just for kill(pid, 0).
fn process_alive(pid: u32) -> bool {
  std::process::Command::new("/bin/ps")
    .args(["-p", &pid.to_string(), "-o", "pid="])
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .map(|status| status.success())
    .unwrap_or(false)
}

// -- background tasks ---------------------------------------------------------

/// Poll the PID lock file and push state transitions to the frontend.
pub fn spawn_status_poller(app: AppHandle, controller: Arc<VpnController>) {
  tauri::async_runtime::spawn(async move {
    loop {
      let observation = read_lock_observation();
      controller.observe_lock(&app, observation);
      tokio::time::sleep(POLL_INTERVAL).await;
    }
  });
}

/// Tail the connect log file and stream parsed JSON records to the frontend.
pub fn spawn_log_tailer(app: AppHandle, controller: Arc<VpnController>) {
  tauri::async_runtime::spawn(async move {
    let path = Config::connect_log_path();
    let mut offset: u64 = 0;
    let mut buffer = String::new();
    loop {
      // The connect flow truncates the file with `>` before gpclient starts,
      // so a shrink means a new connection attempt: rewind to the start.
      if let Ok(meta) = tokio::fs::metadata(&path).await {
        if meta.len() < offset {
          offset = 0;
          buffer.clear();
        }
        if meta.len() > offset {
          offset = tail_chunk(&path, offset, &mut buffer, &app, &controller).await;
        }
      }
      tokio::time::sleep(TAIL_INTERVAL).await;
    }
  });
}

/// Read everything past `offset` that forms complete lines, emit them, and
/// return the new offset.
async fn tail_chunk(
  path: &Path,
  offset: u64,
  buffer: &mut String,
  app: &AppHandle,
  controller: &VpnController,
) -> u64 {
  use tokio::io::{AsyncReadExt, AsyncSeekExt};

  let mut file = match tokio::fs::File::open(path).await {
    Ok(file) => file,
    Err(_) => return offset,
  };
  if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
    return offset;
  }
  let mut chunk = Vec::new();
  if file.read_to_end(&mut chunk).await.is_err() {
    return offset;
  }
  let mut new_offset = offset + chunk.len() as u64;
  buffer.push_str(&String::from_utf8_lossy(&chunk));

  // Emit every complete line; keep any trailing partial line in the buffer.
  while let Some(pos) = buffer.find('\n') {
    let line: String = buffer.drain(..=pos).collect();
    let line = line.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
      continue;
    }
    emit_log_line(app, controller, line);
  }
  // Guard: never grow the buffer unboundedly if gpclient never prints a newline.
  if buffer.len() > 1 << 20 {
    buffer.clear();
    new_offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(new_offset);
  }
  new_offset
}

#[derive(Debug, Clone, Serialize)]
struct LogEvent {
  level: String,
  target: String,
  message: String,
}

fn emit_log_line(app: &AppHandle, controller: &VpnController, line: &str) {
  let event = match serde_json::from_str::<serde_json::Value>(line) {
    Ok(value) => LogEvent {
      level: value
        .get("level")
        .and_then(|v| v.as_str())
        .unwrap_or("INFO")
        .to_string(),
      target: value
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string(),
      message: value
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or(line)
        .to_string(),
    },
    Err(_) => LogEvent {
      level: "RAW".to_string(),
      target: String::new(),
      message: line.to_string(),
    },
  };

  // Extract the gateway name from the connection log messages.
  if let Some(gateway) = parse_gateway(&event.message) {
    controller.observe_gateway(app, Some(gateway));
  }

  let _ = app.emit("vpn-log", &event);
}

fn parse_gateway(message: &str) -> Option<String> {
  for prefix in [
    "Connecting to the selected gateway: ",
    "Connecting to the only available gateway: ",
    "Auto-gateway: attempting gateway ",
  ] {
    if let Some(rest) = message.strip_prefix(prefix) {
      return Some(rest.split_whitespace().next().unwrap_or(rest).to_string());
    }
  }
  None
}

// -- command builders ---------------------------------------------------------

/// Build the privileged script that launches gpclient detached.
pub fn build_connect_script(
  gpclient: &Path,
  portal: &str,
  reuse_cookie: bool,
  log_path: &Path,
) -> String {
  let mut script = String::new();
  // gpclient 3.1.0 runs the SAML auth helper (GP Connect's `gpauth`) as the
  // logged-in user; when gpclient itself is root it resolves that user from
  // SUDO_UID / PKEXEC_UID / DOAS_USER (gpapi::process::users). osascript's
  // "with administrator privileges" sets none of them, so publish the
  // console user's UID ourselves — without it every browser-auth connect
  // dies with "… cannot be run as root" as soon as the SSO cookie expires.
  script.push_str("SUDO_UID=$(stat -f %u /dev/console) ");
  script.push_str(&osascript::sh_quote(&gpclient.to_string_lossy()));
  script.push_str(" --log-format json connect ");
  script.push_str(&osascript::sh_quote(portal));
  script.push_str(" --browser --auto-gateway");
  if reuse_cookie {
    script.push_str(" --cookie-cache");
  }
  script.push_str(" > ");
  script.push_str(&osascript::sh_quote(&log_path.to_string_lossy()));
  script.push_str(" 2>&1 & echo $!");
  script
}

/// Build the privileged script that tears the tunnel down.
pub fn build_disconnect_script(gpclient: &Path) -> String {
  format!(
    "{} disconnect",
    osascript::sh_quote(&gpclient.to_string_lossy())
  )
}

/// Build the privileged script that interrupts a connecting worker.
pub fn build_kill_script(pid: u32) -> String {
  format!("kill -INT {pid}")
}

/// Run the admin script off-thread and return (stdout, stderr, exit_ok).
pub async fn run_admin_script(script: String) -> Result<(String, String), String> {
  tauri::async_runtime::spawn_blocking(move || osascript::run_as_admin(&script))
    .await
    .map_err(|e| format!("join error: {e}"))?
    .map_err(|e| format!("osascript failed to run: {e}"))
    .and_then(|output| {
      let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
      let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
      if output.status.success() {
        Ok((stdout, stderr))
      } else {
        let reason = if stderr.contains("User canceled") {
          "Authorization was canceled".to_string()
        } else {
          stderr
        };
        Err(reason)
      }
    })
}

/// gpclient version string, from the user-accessible CLI (no root needed).
pub async fn gpclient_version(gpclient: &Path) -> String {
  Command::new(gpclient)
    .arg("--version")
    .output()
    .await
    .ok()
    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    .unwrap_or_else(|| "not found".to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn connect_script_quotes_paths_and_arguments() {
    let script = build_connect_script(
      Path::new("/opt/homebrew/bin/gpclient"),
      "gp.imm.re",
      true,
      Path::new("/Users/test/Library/Logs/gp-bar/gpclient-connect.log"),
    );
    assert!(script.contains("'/opt/homebrew/bin/gpclient'"));
    assert!(script.starts_with("SUDO_UID=$(stat -f %u /dev/console) "));
    assert!(script.contains("'gp.imm.re' --browser --auto-gateway --cookie-cache"));
    assert!(script.ends_with("& echo $!"));
  }

  #[test]
  fn connect_script_without_cookie_cache() {
    let script = build_connect_script(Path::new("/gpclient"), "p.example.com", false, Path::new("/log"));
    assert!(!script.contains("--cookie-cache"));
  }

  #[test]
  fn parses_gateway_names() {
    assert_eq!(
      parse_gateway("Connecting to the only available gateway: vpn-1.corp.example"),
      Some("vpn-1.corp.example".to_string())
    );
    assert_eq!(
      parse_gateway("Auto-gateway: attempting gateway gtw-2"),
      Some("gtw-2".to_string())
    );
    assert_eq!(parse_gateway("Unrelated message"), None);
  }
}

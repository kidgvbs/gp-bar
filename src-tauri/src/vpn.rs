//! VPN orchestration: drives the open-source `gpclient` CLI with
//! administrator privileges and monitors the connection from user space.
//!
//! The CLI is the component with no connection time limits; this module only
//! starts it, watches its PID lock file and tails its JSON log lines:
//!
//! * connect: `/usr/bin/osascript` shows the admin password dialog, then
//!   installs and loads a one-shot launchd job (`daemon_plist_path`) that
//!   runs `gpclient --log-format json connect <portal> --browser
//!   --auto-gateway [--cookie-cache]` as a signalable child of launchd.
//!   It must NOT be spawned straight from the admin shell: see
//!   `daemon_plist_path` for the signal-deafness of `authtrampoline`
//!   children on macOS 27.
//! * status: `/var/run/gpclient.lock` is written once the tunnel is up and
//!   removed on disconnect (it is world-readable), so its existence plus the
//!   liveness of the PID it contains is the source of truth.
//! * disconnect: `gpclient disconnect` signals the worker for a graceful
//!   teardown of the tunnel, verified against the lock file.

use std::{
  fs,
  path::{Path, PathBuf},
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

// No serde rename: the frontend (src/main.ts render()), the CSS
// `[data-phase="Connected"]` selector and the tray sync listener all expect
// the variant names verbatim ("Connected"…). A `rename_all = "snake_case"`
// here silently broke every panel state render (the switch fell through to
// the Disconnected default).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

  /// The privileged teardown failed: surface the failure instead of a
  /// stuck "Disconnecting". The poller then reports the truth from the lock
  /// file (still connected → Connected again).
  pub fn disconnect_failed(&self, app: &AppHandle, error: String) {
    self.update(app, |inner| {
      inner.state.phase = Phase::Error;
      inner.state.last_error = Some(error);
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

  /// Apply a portal selection from the config to the idle state.
  ///
  /// `state.portal` is the only hint the panel has that a Connect target
  /// exists: the Connect button stays disabled while it is null. The old
  /// native tray menu seeded it implicitly by connecting straight from
  /// `config.selected`; with the menu gone, a fresh session (or an app
  /// restart while a terminal-started worker holds the lock) left the
  /// button dead after the first disconnect. Only applies while idle:
  /// during Connecting/Connected/Disconnecting `portal` describes the
  /// in-flight operation and must not be rewritten by a selection change.
  pub fn note_selected_portal(&self, app: &AppHandle, portal: Option<String>) {
    self.update(app, |inner| apply_selected_portal(inner, portal));
  }
}

/// Pure half of `VpnController::note_selected_portal`, so the state machine
/// stays unit-testable without an AppHandle.
fn apply_selected_portal(inner: &mut Inner, portal: Option<String>) {
  if matches!(inner.state.phase, Phase::Disconnected | Phase::Error) {
    inner.state.portal = portal;
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

// -- privileged launchd plumbing ----------------------------------------------

/// Where GP Bar (unprivileged) stages the launchd job before installing it.
pub fn staged_plist_path() -> PathBuf {
  Config::log_dir().join("gp-worker.plist")
}

/// The one-shot launchd job loaded per connect.
///
/// The worker must be a child of launchd, not of `authtrampoline`. On
/// macOS 27.0.1 (26A434) the root processes spawned by osascript's
/// `do shell script … with administrator privileges`
/// (`AuthorizationExecuteWithPrivileges`) never act on any catchable
/// signal — proven with a plain C probe that installs handlers and
/// `pause()`s: as root-from-authtrampoline SIGINT and SIGTERM are silently
/// swallowed (even SIGHUP with no handler at all), while the same probe as
/// uid 501, and a root probe spawned by launchd, terminate cleanly.
/// gpclient tears the tunnel down on SIGINT to the lock-file PID, so a
/// worker born under authtrampoline can never be disconnected:
/// `gpclient disconnect` exits 0 having done nothing and the phase sticks
/// at "Disconnecting" forever (SIGKILL works but skips the vpnc-script
/// teardown that restores DNS settings and routes). Hence this
/// RunAtLoad/no-KeepAlive job outside the auto-loaded
/// `/Library/LaunchDaemons` directory: no boot-time autostart, root-owned
/// once installed, and only loaded when GP Bar asks launchd to run it.
pub fn daemon_plist_path() -> PathBuf {
  PathBuf::from("/Library/Application Support/GP Bar/gp-worker.plist")
}

/// (uid, name) of the console user, from the ownership of /dev/console.
///
/// gpclient 3.1.0 spawns the SAML auth helper (GP Connect's `gpauth`) via
/// `CommandExt::into_non_root`, which resolves the real user from
/// SUDO_UID / PKEXEC_UID / DOAS_USER (gpapi::process::users); none exist in
/// a launchd daemon, so the pair is provided through the job's
/// EnvironmentVariables. Without it every browser-auth connect dies with
/// "… cannot be run as root" as soon as the SSO cookie expires.
pub fn console_user() -> Option<(String, String)> {
  let stat = |flag: &str| -> Option<String> {
    std::process::Command::new("/usr/bin/stat")
      .args(["-f", flag, "/dev/console"])
      .output()
      .ok()
      .filter(|out| out.status.success())
      .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
      .filter(|value| !value.is_empty() && !value.contains(char::is_whitespace))
  };
  Some((stat("%u")?, stat("%Su")?))
}

/// XML-escape a value for the job plist. Portals are user input: a raw `&`
/// or `<` would corrupt the description launchd has to parse.
fn xml_escape(value: &str) -> String {
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

/// The launchd job: the same gpclient connect command line GP Bar used to
/// run through osascript, unchanged except for its parent process.
pub fn build_connect_plist(
  gpclient: &Path,
  portal: &str,
  reuse_cookie: bool,
  log_path: &Path,
  console_user: Option<&(String, String)>,
) -> String {
  let mut argv = vec![
    gpclient.to_string_lossy().into_owned(),
    "--log-format".to_string(),
    "json".to_string(),
    "connect".to_string(),
    portal.to_string(),
    "--browser".to_string(),
    "--auto-gateway".to_string(),
  ];
  if reuse_cookie {
    argv.push("--cookie-cache".to_string());
  }
  let arguments = argv
    .iter()
    .map(|arg| format!("<string>{}</string>", xml_escape(arg)))
    .collect::<Vec<_>>()
    .join("");

  // HOME keeps the root worker's cookie cache where the AEWP worker kept
  // it (/var/root/.config/gpclient) so --cookie-cache stays useful; the
  // SUDO_* pair feeds gpclient's auth-helper privilege drop.
  let mut environment = String::from(
    "<key>HOME</key><string>/var/root</string>\
     <key>PATH</key><string>/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>",
  );
  if let Some((uid, name)) = console_user {
    environment.push_str(&format!(
      "<key>SUDO_UID</key><string>{}</string>\
       <key>SUDO_USER</key><string>{}</string>",
      xml_escape(uid),
      xml_escape(name)
    ));
  }

  let log_file = xml_escape(&log_path.to_string_lossy());
  format!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
     <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\
     <plist version=\"1.0\"><dict>\
     <key>Label</key><string>dev.gpbar.worker</string>\
     <key>ProgramArguments</key><array>{arguments}</array>\
     <key>EnvironmentVariables</key><dict>{environment}</dict>\
     <key>StandardOutPath</key><string>{log_file}</string>\
     <key>StandardErrorPath</key><string>{log_file}</string>\
     <key>RunAtLoad</key><true/>\
     </dict></plist>"
  )
}

/// Install the staged job and ask launchd to run it. One admin prompt.
///
/// `install` makes the job root-owned so launchd accepts it; the leading
/// `unload` makes the load idempotent and recovers from a worker that
/// exited outside GP Bar. NOTE: the staged file is written by the
/// unprivileged app and installed by root, so there is a local TOCTOU
/// window — only relevant on multi-user machines with hostile locals.
pub fn build_daemon_connect_script(staged: &Path, plist_path: &Path, log_path: &Path) -> String {
  let mut script = String::new();
  // `install` does not create parent directories; the support dir is ours.
  script.push_str("mkdir -p ");
  script.push_str(&osascript::sh_quote(
    &plist_path
      .parent()
      .unwrap_or_else(|| Path::new("/"))
      .to_string_lossy(),
  ));
  script.push_str("; install -m 0644 -o root -g wheel ");
  script.push_str(&osascript::sh_quote(&staged.to_string_lossy()));
  script.push(' ');
  script.push_str(&osascript::sh_quote(&plist_path.to_string_lossy()));
  // Truncate the connect log: the tailer rewinds on shrink, this keeps a
  // single source of truth for "new attempt".
  script.push_str("; : > ");
  script.push_str(&osascript::sh_quote(&log_path.to_string_lossy()));
  script.push_str("; launchctl unload -w ");
  script.push_str(&osascript::sh_quote(&plist_path.to_string_lossy()));
  script.push_str(" 2>/dev/null; launchctl load -w ");
  script.push_str(&osascript::sh_quote(&plist_path.to_string_lossy()));
  script
}

/// Tear the tunnel down, and fail loudly if the worker does not stop.
///
/// `gpclient disconnect` signals the lock-file PID with SIGINT and exits 0
/// whether or not the signal was acted on (gpapi swallows the delivery
/// error), so the script polls the lock file for the worker's own cleanup
/// instead of trusting the exit code. A non-zero exit surfaces the failure
/// rather than leaving the panel stuck at "Disconnecting". The SIGKILL path
/// is deliberately absent: a killed worker skips the vpnc-script teardown
/// and leaves the macOS DNS settings pointing at VPN resolvers.
pub fn build_disconnect_script(gpclient: &Path, plist_path: &Path) -> String {
  let mut script = String::new();
  script.push_str(&osascript::sh_quote(&gpclient.to_string_lossy()));
  script.push_str(" disconnect; i=0; while [ $i -lt 20 ] && [ -e ");
  script.push_str(LOCK_FILE);
  script.push_str(" ]; do sleep 0.5; i=$((i+1)); done; ");
  script.push_str("if [ -e ");
  script.push_str(LOCK_FILE);
  script.push_str(" ]; then echo 'the VPN worker did not stop' >&2; exit 1; fi; ");
  script.push_str("launchctl unload -w ");
  script.push_str(&osascript::sh_quote(&plist_path.to_string_lossy()));
  script.push_str(" 2>/dev/null");
  script
}

/// Interrupt a connecting worker: unloading the one-shot launchd job makes
/// launchd terminate it, which runs the worker's own teardown instead of a
/// blind kill.
pub fn build_cancel_script(plist_path: &Path) -> String {
  format!(
    "launchctl unload -w {} 2>/dev/null",
    osascript::sh_quote(&plist_path.to_string_lossy())
  )
}

/// Run the admin script off-thread and return (stdout, stderr, exit_ok).
pub async fn run_admin_script(script: String) -> Result<(String, String), String> {
  tauri::async_runtime::spawn_blocking(move || osascript::run_as_admin(&script))
    .await
    .map_err(|e| format!("join error: {e}"))?
    .map_err(|e| format!("sudo failed to run: {e}"))
    .and_then(|output| {
      let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
      let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
      if output.status.success() {
        Ok((stdout, stderr))
      } else {
        let reason = if stderr.contains("User canceled")
          || stderr.contains("no password was provided")
        {
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
  fn connect_plist_escapes_xml_and_keeps_arguments() {
    let console = Some(("501".to_string(), "gab&riele".to_string()));
    let plist = build_connect_plist(
      Path::new("/opt/homebrew/bin/gpclient"),
      "gp.imm.re?a=1&b=2",
      true,
      Path::new("/Users/test/Library/Logs/gp-bar/gpclient-connect.log"),
      console.as_ref(),
    );
    assert!(plist.contains("<string>/opt/homebrew/bin/gpclient</string>"));
    assert!(plist.contains("<string>gp.imm.re?a=1&amp;b=2</string>"));
    assert!(plist
      .contains("<string>--auto-gateway</string><string>--cookie-cache</string>"));
    assert!(plist.contains("<key>SUDO_UID</key><string>501</string>"));
    assert!(plist.contains("<key>SUDO_USER</key><string>gab&amp;riele</string>"));
    assert!(plist.contains("<key>RunAtLoad</key><true/>"));
    // Standard{Out,Error}Path carry the log the tailer follows.
    assert!(plist.contains("<key>StandardOutPath</key>"));
  }

  #[test]
  fn connect_plist_without_cookie_cache_or_console_user() {
    let plist =
      build_connect_plist(Path::new("/gpclient"), "p.example.com", false, Path::new("/log"), None);
    assert!(!plist.contains("--cookie-cache"));
    assert!(!plist.contains("SUDO_UID"));
  }

  #[test]
  fn daemon_connect_script_installs_and_loads() {
    let script = build_daemon_connect_script(
      Path::new("/Users/test/Library/Logs/gp-bar/gp-worker.plist"),
      &daemon_plist_path(),
      Path::new("/Users/test/Library/Logs/gp-bar/gpclient-connect.log"),
    );
    assert!(script.starts_with("mkdir -p '/Library/Application Support/GP Bar'; "));
    assert!(script.contains("install -m 0644 -o root -g wheel "));
    assert!(script.ends_with("launchctl load -w '/Library/Application Support/GP Bar/gp-worker.plist'"));
  }

  #[test]
  fn disconnect_script_waits_for_the_lock_and_fails_loud() {
    let script = build_disconnect_script(Path::new("/opt/homebrew/bin/gpclient"), &daemon_plist_path());
    assert!(script.starts_with("'/opt/homebrew/bin/gpclient' disconnect; "));
    assert!(script.contains("exit 1"));
    assert!(script
      .ends_with("launchctl unload -w '/Library/Application Support/GP Bar/gp-worker.plist' 2>/dev/null"));
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

  #[test]
  fn selected_portal_updates_idle_state_only() {
    // Idle: the selection becomes the Connect target the panel enables.
    let mut idle = Inner::default();
    apply_selected_portal(&mut idle, Some("gp.imm.re".to_string()));
    assert_eq!(idle.state.portal.as_deref(), Some("gp.imm.re"));
    // And clears again when the last portal is removed.
    apply_selected_portal(&mut idle, None);
    assert_eq!(idle.state.portal, None);

    // After an error the user may retarget the next attempt.
    let mut errored = Inner::default();
    errored.state.phase = Phase::Error;
    apply_selected_portal(&mut errored, Some("next.example".to_string()));
    assert_eq!(errored.state.portal.as_deref(), Some("next.example"));

    // In flight: `portal` describes the operation, not the selection, and
    // must survive selection changes and cleanups.
    let mut connecting = Inner::default();
    connecting.state.phase = Phase::Connecting;
    connecting.state.portal = Some("old.example".to_string());
    apply_selected_portal(&mut connecting, Some("new.example".to_string()));
    assert_eq!(connecting.state.portal.as_deref(), Some("old.example"));

    let mut disconnecting = Inner::default();
    disconnecting.state.phase = Phase::Disconnecting;
    disconnecting.state.portal = Some("keep.example".to_string());
    apply_selected_portal(&mut disconnecting, None);
    assert_eq!(disconnecting.state.portal.as_deref(), Some("keep.example"));
  }
}

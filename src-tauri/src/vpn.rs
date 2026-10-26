//! VPN orchestration: drives the open-source `gpclient` CLI with
//! administrator privileges and monitors the connection from user space.
//!
//! The CLI is the component with no connection time limits; this module only
//! starts it, watches its PID lock file and tails its JSON log lines:
//!
//! * connect: `sudo -n` runs the root-owned helper (`scripts/gp-admin`,
//!   installed once by `scripts/setup-admin.sh`, which grants a NOPASSWD
//!   sudoers rule for that exact path). The helper generates and loads a
//!   one-shot launchd job running `gpclient --log-format json connect
//!   <portal> --browser --auto-gateway [--cookie-cache]` as a signalable
//!   child of launchd. No credentials ever pass through the app: the
//!   wrapper's fixed operations are the security boundary.
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
use tauri::{AppHandle, Emitter};
use tokio::process::Command;

use crate::config::Config;

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
            time.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })
        .unwrap_or(0);
    Some(LockObservation {
        pid,
        connected_since_epoch_secs: connected_since,
    })
}

/// `kill(pid, 0)` sends no signal: it only probes existence, and needs no
/// fork/exec (the old `ps -p` spawned a subprocess every poll tick). When the
/// probe fails with EPERM the process exists but belongs to root — our worker.
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 only performs the permission/existence
    // check; it never delivers a signal.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// -- background tasks ---------------------------------------------------------

/// Poll the PID lock file and push state transitions to the frontend.
pub fn spawn_status_poller(app: AppHandle, controller: Arc<VpnController>) {
    tauri::async_runtime::spawn(async move {
        loop {
            let observation = read_lock_observation();
            // Connecting without a lock file means the tunnel never came up:
            // gpclient only writes the lock once it is established, so a dead
            // worker here is a failed attempt (wrong SSO, closed browser,
            // gateway error, crash) — surface it instead of spinning forever.
            // The one-shot job stays loaded (no KeepAlive), so probing it
            // needs the `pid =` line, not the command's exit status.
            if observation.is_none() && controller.snapshot().phase == Phase::Connecting {
                if let Some(false) = worker_has_active_pid(WORKER_LABEL).await {
                    // The job is loaded but has no live process: gpclient ran
                    // and exited before the tunnel came up (wrong SSO, closed
                    // browser, gateway error, crash).
                    controller.connect_failed(
                        &app,
                        "The VPN worker exited before the tunnel came up".to_string(),
                    );
                }
                // Some(true) = still authenticating; None = the job is not
                // loaded yet (the poller can tick between begin_connect and
                // the wrapper's `launchctl load`) or launchctl failed:
                // nothing to report here. `run_admin("start")` reports its
                // own failure, and cancel/stop set their own phases.
            }
            controller.observe_lock(&app, observation);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// `dev.gpbar.worker` — the label of the one-shot launchd job the admin
/// wrapper generates for the root gpclient worker.
const WORKER_LABEL: &str = "dev.gpbar.worker";

/// Whether the worker's launchd job currently has a live process assigned.
///
/// `launchctl print` is readable by an unprivileged user, but its exit status
/// is useless as a liveness probe: a `RunAtLoad` job without KeepAlive stays
/// loaded (exit 0, `state = not running`, `last exit code = <n>`) long after
/// the process died. The `pid = <n>` line is the truth: it is only present
/// while launchd has a process for the job.
///
/// * `Some(true)` — the `pid =` line is there: the worker process is alive.
/// * `Some(false)` — the job is loaded but has no `pid =`: it ran and exited.
/// * `None` — the job is not in the domain (exit 113, e.g. before the
///   wrapper's first `launchctl load`) or launchctl failed: unknown.
async fn worker_has_active_pid(label: &str) -> Option<bool> {
    let target = format!("system/{label}");
    let output = tokio::process::Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.lines()
            .any(|line| line.trim_start().starts_with("pid = ")),
    )
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
        new_offset = tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .unwrap_or(new_offset);
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
        controller.observe_gateway(app, Some(gateway.clone()));
        harvest_gateway(app, &gateway);
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

/// A gateway name parsed from the live connect log: keep it in the config so
/// the picker can offer it next time (`gpclient` lists gateways only while
/// connecting; there is no query command).
fn harvest_gateway(app: &AppHandle, gateway: &str) {
    use tauri::Manager;
    let Some(ctx) = app.try_state::<crate::AppContext>() else {
        return;
    };
    let (cfg, changed) = {
        let mut cfg = ctx.config.lock().unwrap();
        let changed = !cfg.gateways.iter().any(|g| g == gateway);
        if changed {
            cfg.gateways.push(gateway.to_string());
            if let Err(error) = cfg.save() {
                eprintln!("gp-bar: cannot save the harvested gateway: {error}");
            }
        }
        (cfg.clone(), changed)
        // Guard dropped BEFORE emitting (see `update`).
    };
    if changed {
        let _ = app.emit("config-updated", &cfg);
    }
}

// -- network health (user-space, read-only) -------------------------------------
//
// After a worker dies without its graceful teardown (overnight standby,
// crash, power loss), the DNS servers vpnc-script set with
// `networksetup -setdnsservers` stay in the system settings and break name
// resolution. Detecting that needs no privileges: reading a service's DNS
// is a user operation, and stale DNS only matters while the VPN is down.

/// What the panel needs to know about residual VPN DNS settings.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NetworkHealth {
    pub stale: bool,
    pub service: Option<String>,
    pub servers: Vec<String>,
}

/// Manual DNS servers of the active network service, for the stale-DNS
/// banner. Read-only and unprivileged.
pub async fn active_service_dns() -> (Option<String>, Vec<String>) {
    let Some(interface) = scutil_primary_interface().await else {
        return (None, Vec::new());
    };
    let order = command_stdout(&["/usr/sbin/networksetup", "-listnetworkserviceorder"]).await;
    let Some(service) = parse_service_for_device(&order, &interface) else {
        return (None, Vec::new());
    };
    let dns = command_stdout(&["/usr/sbin/networksetup", "-getdnsservers", &service]).await;
    (Some(service), parse_dns_servers(&dns))
}

async fn scutil_primary_interface() -> Option<String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("/usr/sbin/scutil")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(b"show State:/Network/Global/IPv4\n")
        .await
        .ok()?;
    let output = child.wait_with_output().await.ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.split_once("PrimaryInterface :"))
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn command_stdout(command: &[&str]) -> String {
    match tokio::process::Command::new(command[0])
        .args(&command[1..])
        .output()
        .await
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
        Err(_) => String::new(),
    }
}

/// IPv4 literals from `networksetup -getdnsservers` output. The "no manual
/// DNS" prose ("There aren't any DNS Servers set on …") contains no literal,
/// so it filters out naturally.
fn parse_dns_servers(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| {
            let parts: Vec<_> = line.trim().split('.').collect();
            parts.len() == 4
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(|line| line.trim().to_string())
        .collect()
}

/// The network service that owns `device`, from
/// `networksetup -listnetworkserviceorder` output. Mirrors the wrapper's
/// `active_service` so the banner and `gp-admin repair` agree on the service.
fn parse_service_for_device(output: &str, device: &str) -> Option<String> {
    let mut service: Option<String> = None;
    for line in output.lines() {
        let line = line.trim();
        // The device line starts with '(' too: it must be checked first, or
        // it would be mistaken for a service name line.
        if let Some(rest) = line.strip_prefix("(Hardware Port:") {
            // "(Hardware Port: Wi-Fi, Device: en0)"
            if let Some((_, dev)) = rest.split_once("Device: ") {
                if dev.trim_end_matches(')').trim() == device {
                    return service;
                }
            }
        } else if let Some(rest) = line.strip_prefix('(') {
            // "(4) Wi-Fi" / "(*) monitor ufficio" — a service name line.
            if let Some((_, name)) = rest.split_once(") ") {
                service = Some(name.to_string());
            }
        }
    }
    None
}

// -- privileged helper ---------------------------------------------------------

/// The root-owned wrapper that `/etc/sudoers.d/gp-bar` allows GP Bar (and
/// nothing else) to run passwordless. Fixed operations with validated
/// arguments only — the wrapper file is the security boundary, see
/// `scripts/gp-admin` (bundled under `Contents/Resources/scripts/`).
pub const ADMIN_WRAPPER: &str = "/usr/local/lib/gp-bar/admin";

/// Run one admin operation (`selftest`, `start`, `stop`, `cancel`).
///
/// `sudo -n` never prompts: without the sudoers rule it fails fast, and
/// `admin_error_hint` turns that failure into the one-time setup
/// instructions. No credentials are ever seen or handled by GP Bar — the
/// whole point of the wrapper design (Touch ID in a sudo-askpass context
/// turned out to be impossible: `pam_tid` falls back to the password for
/// any process that is not an interactive terminal, verified empirically
/// — see RECAP.md).
pub async fn run_admin(op: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("/usr/bin/sudo")
        .arg("-n")
        .arg(ADMIN_WRAPPER)
        .arg(op)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("sudo failed to run: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else if stderr.is_empty() {
        Err(format!("admin op '{op}' failed: exit {}", output.status))
    } else {
        Err(stderr)
    }
}

/// The bundled setup script path inside the running app bundle.
pub fn setup_script_path(app: &AppHandle) -> PathBuf {
    use tauri::Manager;
    app.path()
        .resource_dir()
        .unwrap_or_else(|_| PathBuf::from("/Applications/GP Bar.app/Contents/Resources"))
        .join("scripts/setup-admin.sh")
}

/// Map sudo/wrapper failures to the one-time setup hint when the helper is
/// not installed; operational failures pass through unchanged.
pub fn admin_error_hint(setup_script: &Path, error: &str) -> String {
    if error.contains("a password is required") || error.contains("command not found") {
        format!(
            "The privileged helper is not installed. One-time setup (Terminal):\n  sudo bash '{}'",
            setup_script.display()
        )
    } else {
        error.to_string()
    }
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
    fn admin_errors_map_to_the_setup_hint() {
        let setup = Path::new("/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh");
        // Missing sudoers rule and missing wrapper both mean "run the setup".
        assert!(admin_error_hint(setup, "sudo: a password is required").contains("setup-admin.sh"));
        assert!(admin_error_hint(
            setup,
            "sudo: /usr/local/lib/gp-bar/admin: command not found"
        )
        .contains("setup-admin.sh"));
        // Operational failures pass through untouched.
        assert_eq!(
            admin_error_hint(setup, "gp-admin: the VPN worker did not stop"),
            "gp-admin: the VPN worker did not stop"
        );
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
        apply_selected_portal(&mut idle, Some("vpn.example.com".to_string()));
        assert_eq!(idle.state.portal.as_deref(), Some("vpn.example.com"));
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

    #[test]
    fn parses_manual_dns_servers_only() {
        assert_eq!(
            parse_dns_servers("172.16.3.101\n172.16.3.102\n"),
            vec!["172.16.3.101", "172.16.3.102"]
        );
        // The "no manual DNS" prose filters out: no IPv4 literal on the line.
        assert_eq!(
            parse_dns_servers("There aren't any DNS Servers set on Wi-Fi.\n"),
            Vec::<String>::new()
        );
        assert_eq!(parse_dns_servers(""), Vec::<String>::new());
    }

    #[test]
    fn parses_the_service_owning_the_active_device() {
        let order = "\
An asterisk (*) denotes that a network service is disabled.
(*) monitor ufficio
(Hardware Port: USB 10/100/1000 LAN, Device: en4)

(1) Hub-immo
(Hardware Port: AX88179A, Device: en6)

(4) Wi-Fi
(Hardware Port: Wi-Fi, Device: en0)
";
        assert_eq!(
            parse_service_for_device(order, "en0"),
            Some("Wi-Fi".to_string())
        );
        assert_eq!(
            parse_service_for_device(order, "en6"),
            Some("Hub-immo".to_string())
        );
        // Disabled services are resolvable too (they carry stale VPN DNS).
        assert_eq!(
            parse_service_for_device(order, "en4"),
            Some("monitor ufficio".to_string())
        );
        assert_eq!(parse_service_for_device(order, "en9"), None);
    }
}

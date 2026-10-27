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
    /// Whether the in-flight connect attempt's `admin start` has been
    /// confirmed (the wrapper returned Ok and loaded a fresh launchd job).
    /// The status poller may blame a job without a pid on a dead worker ONLY
    /// while this is true: before it, the job on the system is still the
    /// previous attempt's (stale, no pid) and would be reported as a false
    /// "worker exited" through the whole SSO login.
    worker_started: bool,
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

    /// Gate + transition for a connect attempt in ONE critical section.
    ///
    /// `do_connect` used to check the phase with `snapshot()` (lock taken and
    /// released), do other work, and only then transition with
    /// `begin_connect` (lock re-taken). On the multi-threaded tokio runtime
    /// two invocations (double click, panel + tray menu) both passed the
    /// gate, and the second `admin start` unloaded — killing — the first
    /// worker mid-SSO. The check and the transition must be atomic; the
    /// refusal error is the caller's to surface.
    pub fn try_begin_connect(&self, app: &AppHandle, portal: &str) -> Result<(), String> {
        let outcome = {
            let mut inner = self.inner.lock().unwrap();
            match enter_connecting(&mut inner, portal) {
                Ok(()) => Ok(inner.state.clone()),
                Err(phase) => Err(format!("cannot connect while {phase:?}")),
            }
            // Guard dropped BEFORE emitting (the `update` discipline).
        };
        match outcome {
            Ok(state) => {
                let _ = app.emit("vpn-state", &state);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Gate + transition for a disconnect attempt, atomic like
    /// `try_begin_connect`. A second disconnect while already Disconnecting
    /// is idempotent (Ok), matching the old `do_disconnect` behavior.
    pub fn try_begin_disconnect(&self, app: &AppHandle) -> Result<(), String> {
        let outcome = {
            let mut inner = self.inner.lock().unwrap();
            match enter_disconnecting(&mut inner) {
                Ok(()) => Ok(inner.state.clone()),
                Err(phase) => Err(format!("cannot disconnect while {phase:?}")),
            }
            // Guard dropped BEFORE emitting (the `update` discipline).
        };
        match outcome {
            Ok(state) => {
                let _ = app.emit("vpn-state", &state);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub fn connect_failed(&self, app: &AppHandle, error: String) {
        self.update(app, |inner| {
            inner.state.phase = Phase::Error;
            inner.state.last_error = Some(error);
            inner.worker_started = false;
        });
    }

    /// The wrapper's `start` returned Ok: the launchd job now belongs to this
    /// attempt, and the poller may trust its `pid =` line from here on.
    ///
    /// If the phase already left Connecting (the attempt failed, was
    /// canceled, or a stale lock file graduated it straight to Connected),
    /// setting the flag is harmless: it is reset by `enter_connecting` on
    /// every new attempt and cleared on every teardown/failed transition, so
    /// a stale `true` can never gate a future attempt.
    pub fn mark_worker_started(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.worker_started = true;
    }

    // -- disconnect ------------------------------------------------------------
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
        self.update(app, |inner| apply_lock(inner, lock));
    }

    /// Whether the poller may attribute a missing lock file to a dead worker:
    /// only while a connect attempt is in flight AND its start has been
    /// confirmed (`mark_worker_started`, set once `run_admin("start")`
    /// returned Ok). Before that the launchd job is still the previous one
    /// (or not loaded), and a job without a `pid =` line would be reported as
    /// a false "worker exited" through the whole SSO login.
    pub fn connecting_worker_started(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.state.phase == Phase::Connecting && inner.worker_started
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

/// Pure half of `VpnController::try_begin_connect`: the connect gate and
/// transition, unit-testable without an AppHandle. Only the idle phases may
/// start a connect; the refusal carries the blocking phase.
fn enter_connecting(inner: &mut Inner, portal: &str) -> Result<(), Phase> {
    if !matches!(inner.state.phase, Phase::Disconnected | Phase::Error) {
        return Err(inner.state.phase.clone());
    }
    inner.state.phase = Phase::Connecting;
    inner.state.portal = Some(portal.to_string());
    inner.state.gateway = None;
    inner.state.last_error = None;
    inner.state.connected_since_epoch_secs = None;
    // New attempt: its start is not confirmed yet (see `worker_started`).
    inner.worker_started = false;
    Ok(())
}

/// Pure half of `VpnController::try_begin_disconnect`. Connected proceeds;
/// Disconnecting is already in flight (idempotent Ok); anything else refuses.
fn enter_disconnecting(inner: &mut Inner) -> Result<(), Phase> {
    match inner.state.phase.clone() {
        Phase::Connected => {
            inner.state.phase = Phase::Disconnecting;
            Ok(())
        }
        Phase::Disconnecting => Ok(()),
        phase => Err(phase),
    }
}

/// Pure half of `VpnController::observe_lock`: the phase × observation
/// transition table, unit-testable without an AppHandle. Documented by
/// `tests::apply_lock_transitions`.
fn apply_lock(inner: &mut Inner, lock: Option<LockObservation>) {
    match (lock, &inner.state.phase) {
        (Some(obs), Phase::Disconnected | Phase::Error) => {
            // A gpclient worker is running although we did not start it this
            // session (e.g. started from a terminal, or the app restarted).
            inner.state.phase = Phase::Connected;
            inner.state.connected_since_epoch_secs = Some(obs.connected_since_epoch_secs);
            inner.state.pid = Some(obs.pid);
        }
        (Some(obs), Phase::Connecting) => {
            inner.state.phase = Phase::Connected;
            inner.state.connected_since_epoch_secs = Some(obs.connected_since_epoch_secs);
            inner.state.pid = Some(obs.pid);
        }
        (Some(_), Phase::Connected | Phase::Disconnecting) => {}
        (None, Phase::Connected | Phase::Disconnecting) => {
            // The lock disappeared: gpclient exited and cleaned up.
            inner.state.phase = Phase::Disconnected;
            inner.state.connected_since_epoch_secs = None;
            inner.state.pid = None;
            inner.state.gateway = None;
            inner.worker_started = false;
        }
        (None, _) => {}
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
    let pid = parse_lock_pid(&raw)?;
    if !process_alive(pid) {
        return None;
    }
    // kill(pid, 0) == EPERM only says "a process with this PID exists"; after
    // a crash without cleanup the PID may be ANY process's (C6). Require the
    // process to actually be gpclient, else treat the lock as stale.
    if !process_is_gpclient(pid) {
        log::warn!("lock file PID {pid} is not a gpclient process — treating the lock as stale");
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

/// PID from the lock file (`gpclient` writes its own `std::process::id()`).
/// Parse into a u32 FIRST and bound it to the positive i32 range: a `u32`
/// cast straight to `libc::pid_t` would wrap negative above `i32::MAX`, and
/// `kill(-1, 0)` means "every process" — a phantom-alive lock (C6).
/// Empty or garbage content is treated like a missing PID.
fn parse_lock_pid(raw: &str) -> Option<u32> {
    let pid = raw.trim().parse::<u32>().ok()?;
    (pid > 0 && pid <= i32::MAX as u32).then_some(pid)
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

/// Whether the process behind `pid` is actually a `gpclient` binary — the
/// best identity check available without privileges (C6). `kill(pid, 0)`
/// returning EPERM only proves that SOME process owns the PID; after a
/// crash without cleanup the PID may have been reused by anything, and the
/// app would show Connected forever. `proc_pidpath` (libproc, unprivileged)
/// reports the executable path of the process: compare its basename against
/// the worker's binary name (setup-admin.sh copies gpclient to
/// `$PREFIX/bin/gpclient`, so the name is stable regardless of install).
#[cfg(target_os = "macos")]
fn process_is_gpclient(pid: u32) -> bool {
    // SAFETY: proc_pidpath writes at most `buffersize` bytes into `buffer`
    // and only reads the integer `pid`; both come from here.
    extern "C" {
        fn proc_pidpath(pid: libc::pid_t, buffer: *mut u8, buffersize: u32) -> i32;
    }
    let mut buffer = [0u8; 4096];
    let len = unsafe { proc_pidpath(pid as libc::pid_t, buffer.as_mut_ptr(), buffer.len() as u32) };
    if len <= 0 {
        // Dead PID, or not permitted: cannot confirm, do not trust.
        return false;
    }
    let path = std::str::from_utf8(&buffer[..len as usize])
        .ok()
        .map(Path::new);
    path.is_some_and(|path| path.file_name().is_some_and(|name| name == "gpclient"))
}

#[cfg(not(target_os = "macos"))]
fn process_is_gpclient(_pid: u32) -> bool {
    // No proc_pidpath off macOS: keep the kill(pid, 0) semantics only.
    true
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
            // But only once the start is CONFIRMED (`worker_started`, set
            // after `run_admin("start")` returns Ok): before that the job on
            // the system is still the previous attempt's, and its missing
            // `pid =` line would be a false "worker exited" sticking on screen
            // through the whole SSO login.
            // The one-shot job stays loaded (no KeepAlive), so probing it
            // needs the `pid =` line, not the command's exit status.
            if observation.is_none() && controller.connecting_worker_started() {
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
                // loaded yet or launchctl failed: nothing to report here.
                // `run_admin("start")` reports its own failure, and
                // cancel/stop set their own phases.
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
        // BYTE buffer, never a String: a multibyte UTF-8 character split
        // across two reads must not become two U+FFFD (R3). Only complete
        // lines are decoded, inside `drain_complete_lines`.
        let mut buffer: Vec<u8> = Vec::new();
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

/// Split complete lines off the byte buffer, decoding each one
/// individually. Splitting on `b'\n'` is UTF-8-safe: no multibyte sequence
/// contains a 0x0A byte, so a line boundary can never cut a character.
/// Incomplete trailing bytes stay buffered for the next chunk; a line with
/// invalid bytes (corrupt log, a partial write made visible by the
/// truncate-rewind race) is decoded lossily — at most one U+FFFD per line.
fn drain_complete_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buffer.drain(..=pos).collect();
        let line = match String::from_utf8(raw) {
            Ok(line) => line,
            Err(error) => {
                log::warn!("invalid UTF-8 in a connect log line: {error}");
                String::from_utf8_lossy(error.as_bytes()).into_owned()
            }
        };
        let line = line.trim_end_matches(['\n', '\r']);
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Read everything past `offset` that forms complete lines, emit them, and
/// return the new offset.
async fn tail_chunk(
    path: &Path,
    offset: u64,
    buffer: &mut Vec<u8>,
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
    buffer.extend_from_slice(&chunk);

    for line in drain_complete_lines(buffer) {
        emit_log_line(app, controller, &line);
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

    // Extract the gateway from the connection log messages.
    match parse_gateway(&event.message) {
        // A selection line: learn the pair if present, then observe and
        // harvest the NAME (never the address — the picker stores names).
        Some(GatewayMention::Selected { name, address }) => {
            if let Some(address) = address {
                learn_gateway_host(app, &address, &name);
            }
            controller.observe_gateway(app, Some(name.clone()));
            harvest_gateway(app, &name);
        }
        // A getconfig POST: the cached-cookie path emits only this. Translate
        // to the picker name via the config map; display the raw FQDN when
        // unknown, but harvest only a known NAME (never the FQDN).
        Some(GatewayMention::Host(host)) => {
            let name = gateway_display_name(app, &host);
            controller.observe_gateway(app, Some(name.clone().unwrap_or(host)));
            if let Some(name) = name {
                harvest_gateway(app, &name);
            }
        }
        None => {}
    }

    let _ = app.emit("vpn-log", &event);
}

/// A gateway mentioned in a connect-log line, and in which form.
#[derive(Debug, PartialEq, Eq)]
enum GatewayMention {
    /// A selection line: `name` or `name (address)` — `Gateway`'s Display.
    Selected {
        name: String,
        address: Option<String>,
    },
    /// A getconfig POST to a gateway host: universal signal, the only one
    /// the cached-cookie re-auth path emits (no selection lines there).
    Host(String),
}

fn parse_gateway(message: &str) -> Option<GatewayMention> {
    for prefix in [
        "Connecting to the selected gateway: ",
        "Connecting to the only available gateway: ",
        "Auto-gateway: attempting gateway ",
    ] {
        if let Some(rest) = message.strip_prefix(prefix) {
            let mut tokens = rest.split_whitespace();
            let name = tokens.next().unwrap_or(rest).to_string();
            // `name (address)`: strip the parens to remember the pair.
            let address = tokens
                .next()
                .filter(|token| token.starts_with('(') && token.ends_with(')'))
                .map(|token| token[1..token.len() - 1].to_string());
            return Some(GatewayMention::Selected { name, address });
        }
    }
    // `POST https://<gw>/ssl-vpn/getconfig.esp` — but NOT the portal's
    // `/global-protect/getconfig.esp` or the HIP `/ssl-vpn/hipreportcheck.esp`.
    if let Some(rest) = message.strip_prefix("POST https://") {
        if let Some(host) = rest.strip_suffix("/ssl-vpn/getconfig.esp") {
            if !host.contains('/') {
                return Some(GatewayMention::Host(host.to_string()));
            }
        }
    }
    None
}

/// Remember a gateway FQDN → name pair in the config, so the cached-cookie
/// re-auth path (which logs only the FQDN) can still display the name.
fn learn_gateway_host(app: &AppHandle, address: &str, name: &str) {
    use tauri::Manager;
    let Some(ctx) = app.try_state::<crate::AppContext>() else {
        return;
    };
    let (cfg, changed) = {
        let mut cfg = ctx.config.lock().unwrap();
        if cfg.gateway_hosts.get(address).map(String::as_str) == Some(name) {
            (cfg.clone(), false)
        } else {
            cfg.gateway_hosts
                .insert(address.to_string(), name.to_string());
            if let Err(error) = cfg.save() {
                log::warn!("cannot save the gateway host map: {error}");
            }
            (cfg.clone(), true)
        }
        // Guard dropped BEFORE emitting (see `update`).
    };
    if changed {
        let _ = app.emit("config-updated", &cfg);
    }
}

/// Translate a gateway FQDN into its picker name, if known.
fn gateway_display_name(app: &AppHandle, address: &str) -> Option<String> {
    use tauri::Manager;
    let ctx = app.try_state::<crate::AppContext>()?;
    let cfg = ctx.config.lock().unwrap();
    cfg.gateway_hosts.get(address).cloned()
}

/// A gateway name parsed from the live connect log: keep it in the config so
/// the picker can offer it next time (`gpclient` lists gateways only while
/// connecting; there is no query command).
fn harvest_gateway(app: &AppHandle, gateway: &str) {
    use tauri::Manager;
    let Some(ctx) = app.try_state::<crate::AppContext>() else {
        return;
    };
    // Same charset the wrapper and `select_gateway` enforce (R4): a name
    // harvested straight from the log must not reach the picker if the
    // wrapper would refuse it (the picker would offer a dead entry).
    if !crate::config::valid_name(gateway) {
        log::warn!("skipping harvested gateway {gateway:?}: invalid name");
        return;
    }
    let (cfg, changed) = {
        let mut cfg = ctx.config.lock().unwrap();
        let changed = !cfg.gateways.iter().any(|g| g == gateway);
        if changed {
            cfg.gateways.push(gateway.to_string());
            if let Err(error) = cfg.save() {
                log::warn!("cannot save the harvested gateway: {error}");
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

/// The pre-VPN DNS snapshot the privileged wrapper records at connect time
/// (see scripts/gp-admin `start`): line 1 = the service it was taken on, then
/// the manual IPv4 servers, an `ipv6:` marker line, then the IPv6 servers.
/// World-readable, written atomically by root.
const DNS_SNAPSHOT: &str = "/Library/Application Support/GP Bar/dns-snapshot.txt";

/// Whether the active service's current manual IPv4 DNS is exactly the
/// recorded pre-VPN state: then it is the user's OWN configuration, not a
/// VPN leftover, and the banner must not call it stale. Anything else
/// (missing/empty snapshot, another service, a different set) is unknown —
/// the banner is advisory and `gp-admin repair` is the one that decides.
pub fn dns_matches_pre_vpn_snapshot(service: &str, servers: &[String]) -> bool {
    let Ok(raw) = std::fs::read_to_string(DNS_SNAPSHOT) else {
        return false;
    };
    snapshot_v4_matches(&raw, service, servers)
}

fn snapshot_v4_matches(raw: &str, service: &str, servers: &[String]) -> bool {
    let mut lines = raw.lines().map(str::trim);
    let Some(snap_service) = lines.next() else {
        return false;
    };
    if snap_service != service {
        return false;
    }
    let mut snapshot: Vec<String> = lines
        .take_while(|line| *line != "ipv6:")
        .map(str::to_string)
        .collect();
    let mut current: Vec<String> = servers.to_vec();
    // Set equality, order-insensitive (networksetup may list in any order).
    snapshot.sort_unstable();
    current.sort_unstable();
    !snapshot.is_empty() && snapshot == current
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
/// The whole install prefix (wrapper + the gpclient binaries it executes)
/// is root-owned under /Library/PrivilegedHelperTools — every parent of
/// that path is system-owned, unlike the old /usr/local/lib location
/// (which Homebrew chowns to the user on Intel Macs).
pub const ADMIN_WRAPPER: &str = "/Library/PrivilegedHelperTools/gp-bar/admin";

/// The gpclient the admin wrapper actually executes (installed by the setup
/// script under the same root-owned prefix as `ADMIN_WRAPPER`). The version
/// reported in the About box is resolved from THIS binary when it exists, so
/// the displayed version matches what the worker really runs.
pub const WORKER_GPCLIENT: &str = "/Library/PrivilegedHelperTools/gp-bar/bin/gpclient";

/// Run one admin operation (`selftest`, `start`, `stop`, `cancel`).
///
/// `sudo -n` never prompts: without the sudoers rule it fails fast, and
/// `admin_error_hint` turns that failure into the one-time setup
/// instructions. No credentials are ever seen or handled by GP Bar — the
/// whole point of the wrapper design (Touch ID in a sudo-askpass context
/// turned out to be impossible: `pam_tid` falls back to the password for
/// any process that is not an interactive terminal, verified empirically
/// — see RECAP.md).
/// How long to wait for one privileged helper operation before giving up
/// and reporting a timeout (R2). 30 s is far above every healthy op (start
/// ≈1 s, stop waits on the worker's 60 s SIGTERM grace — a stop that long
/// means a stuck `gpclient disconnect`, exactly what must not hang the UI
/// forever); short enough that the "Disconnecting…" state resolves within
/// half a minute instead of never.
const ADMIN_OP_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run_admin(op: &str, args: &[&str]) -> Result<String, String> {
    // LC_ALL/LANG=C: sudo and the wrapper run with stable, non-localized
    // error strings, which `admin_error_hint` matches on (R5).
    let fut = Command::new("/usr/bin/sudo")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .arg("-n")
        .arg(ADMIN_WRAPPER)
        .arg(op)
        .args(args)
        // Reap the child if this future is dropped by the timeout below:
        // a hung `gpclient disconnect` must not survive as a stray root
        // process (sudo forwards nothing here; the worker is launchd's).
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(ADMIN_OP_TIMEOUT, fut)
        .await
        .map_err(|_| format!("admin op '{op}' timed out"))?
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
    // Locale-independent fast path (R5): no wrapper file means the setup is
    // missing, whatever the exact sudo/OS error string says. sudo's stderr
    // is forced to LC_ALL=C by `run_admin`, so the string matches below are
    // stable, but the existence check works even without them.
    let hint = || {
        format!(
            "The privileged helper is not installed. One-time setup (Terminal):\n  sudo bash '{}'",
            setup_script.display()
        )
    };
    if !Path::new(ADMIN_WRAPPER).exists()
        || error.contains("a password is required")
        || error.contains("command not found")
    {
        hint()
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
            "sudo: /Library/PrivilegedHelperTools/gp-bar/admin: command not found"
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
        use super::GatewayMention;
        assert_eq!(
            parse_gateway("Connecting to the only available gateway: vpn-1 (vpn-1.corp.example)"),
            Some(GatewayMention::Selected {
                name: "vpn-1".to_string(),
                address: Some("vpn-1.corp.example".to_string()),
            })
        );
        assert_eq!(
            parse_gateway("Auto-gateway: attempting gateway gtw-2"),
            Some(GatewayMention::Selected {
                name: "gtw-2".to_string(),
                address: None,
            })
        );
        assert_eq!(
            parse_gateway("POST https://vpn-1.corp.example/ssl-vpn/getconfig.esp"),
            Some(GatewayMention::Host("vpn-1.corp.example".to_string()))
        );
        // The portal getconfig and the HIP report must NOT match.
        assert_eq!(
            parse_gateway("POST https://portal.corp.example/global-protect/getconfig.esp"),
            None
        );
        assert_eq!(
            parse_gateway("POST https://vpn-1.corp.example/ssl-vpn/hipreportcheck.esp"),
            None
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
    fn apply_lock_transitions() {
        let obs = || LockObservation {
            pid: 4242,
            connected_since_epoch_secs: 1_700_000_000,
        };
        let inner_at = |phase: Phase| {
            let mut inner = Inner::default();
            inner.state.phase = phase;
            inner
        };

        // -- observation Some (lock file present, PID alive) ----------------
        //
        // Idle phases are adopted: an outside worker is connected.
        for phase in [Phase::Disconnected, Phase::Error] {
            let mut inner = inner_at(phase);
            apply_lock(&mut inner, Some(obs()));
            assert_eq!(inner.state.phase, Phase::Connected);
            assert_eq!(inner.state.pid, Some(4242));
            assert_eq!(inner.state.connected_since_epoch_secs, Some(1_700_000_000));
        }
        // Our own attempt completed: Connecting graduates to Connected.
        {
            let mut inner = inner_at(Phase::Connecting);
            inner.state.portal = Some("vpn.example.com".to_string());
            apply_lock(&mut inner, Some(obs()));
            assert_eq!(inner.state.phase, Phase::Connected);
            // The in-flight target survives the transition.
            assert_eq!(inner.state.portal.as_deref(), Some("vpn.example.com"));
        }
        // Already-connected phases absorb the observation unchanged.
        for phase in [Phase::Connected, Phase::Disconnecting] {
            let mut inner = inner_at(phase.clone());
            inner.state.gateway = Some("gw-keep".to_string());
            apply_lock(&mut inner, Some(obs()));
            assert_eq!(inner.state.phase, phase);
            assert_eq!(inner.state.gateway.as_deref(), Some("gw-keep"));
            // No adoption: the reported pid was not rewritten either.
            assert_eq!(inner.state.pid, None);
        }

        // -- observation None (no lock file / dead PID) ---------------------
        //
        // Connected phases are torn down: the worker exited and cleaned up.
        for phase in [Phase::Connected, Phase::Disconnecting] {
            let mut inner = inner_at(phase);
            inner.state.pid = Some(4242);
            inner.state.connected_since_epoch_secs = Some(1_700_000_000);
            inner.state.gateway = Some("gw-old".to_string());
            apply_lock(&mut inner, None);
            assert_eq!(inner.state.phase, Phase::Disconnected);
            assert_eq!(inner.state.connected_since_epoch_secs, None);
            assert_eq!(inner.state.pid, None);
            assert_eq!(inner.state.gateway, None);
        }
        // Idle and in-flight phases absorb a missing lock: Connecting keeps
        // waiting for its worker (the poller decides exit-vs-authenticating
        // separately, not here).
        for phase in [Phase::Disconnected, Phase::Connecting, Phase::Error] {
            let mut inner = inner_at(phase.clone());
            apply_lock(&mut inner, None);
            assert_eq!(inner.state.phase, phase);
        }
    }

    #[test]
    fn connect_gate_only_allows_idle_phases() {
        // Idle phases start a connect and reset the attempt bookkeeping.
        for phase in [Phase::Disconnected, Phase::Error] {
            let mut inner = Inner::default();
            inner.state.phase = phase;
            inner.state.gateway = Some("gw-stale".to_string());
            inner.state.last_error = Some("old failure".to_string());
            inner.worker_started = true;
            enter_connecting(&mut inner, "vpn.example.com").unwrap();
            assert_eq!(inner.state.phase, Phase::Connecting);
            assert_eq!(inner.state.portal.as_deref(), Some("vpn.example.com"));
            assert_eq!(inner.state.gateway, None);
            assert_eq!(inner.state.last_error, None);
            assert_eq!(inner.state.connected_since_epoch_secs, None);
            assert!(!inner.worker_started);
        }
        // In-flight phases refuse and change nothing.
        for phase in [Phase::Connecting, Phase::Connected, Phase::Disconnecting] {
            let mut inner = Inner::default();
            inner.state.phase = phase.clone();
            inner.state.portal = Some("in-flight.example".to_string());
            assert_eq!(
                enter_connecting(&mut inner, "other.example"),
                Err(phase.clone())
            );
            assert_eq!(inner.state.phase, phase);
            assert_eq!(inner.state.portal.as_deref(), Some("in-flight.example"));
        }
    }

    #[test]
    fn disconnect_gate_allows_connected_and_idempotent_disconnecting() {
        let mut inner = Inner::default();
        inner.state.phase = Phase::Connected;
        enter_disconnecting(&mut inner).unwrap();
        assert_eq!(inner.state.phase, Phase::Disconnecting);
        // A second disconnect while already tearing down is idempotent.
        enter_disconnecting(&mut inner).unwrap();
        assert_eq!(inner.state.phase, Phase::Disconnecting);

        for phase in [Phase::Disconnected, Phase::Connecting, Phase::Error] {
            let mut inner = Inner::default();
            inner.state.phase = phase.clone();
            assert_eq!(enter_disconnecting(&mut inner), Err(phase));
        }
    }

    #[test]
    fn poller_exit_gate_requires_a_confirmed_start() {
        // The C2 scenario: Connecting but `run_admin("start")` not back yet.
        // The poller must NOT blame the stale launchd job for the exit.
        let mut inner = Inner::default();
        inner.state.phase = Phase::Connecting;
        apply_lock(&mut inner, None);
        assert_eq!(inner.state.phase, Phase::Connecting);

        let controller = VpnController::new();
        {
            let mut i = controller.inner.lock().unwrap();
            i.state.phase = Phase::Connecting;
            i.worker_started = false;
        }
        assert!(!controller.connecting_worker_started());

        // Once the start is confirmed, the gate opens...
        controller.mark_worker_started();
        assert!(controller.connecting_worker_started());

        // ...and if the phase already moved on (stale lock graduated it to
        // Connected, or a cancel failed the attempt), marking is harmless:
        // the flag alone never re-opens the gate.
        {
            let mut i = controller.inner.lock().unwrap();
            i.state.phase = Phase::Error;
        }
        controller.mark_worker_started();
        assert!(!controller.connecting_worker_started());

        // Teardown and failure clear it for the next attempt too.
        {
            let mut i = controller.inner.lock().unwrap();
            i.state.phase = Phase::Connected;
            i.worker_started = true;
        }
        apply_lock(&mut controller.inner.lock().unwrap(), None);
        assert!(!controller.connecting_worker_started());
    }

    #[test]
    fn splits_lines_on_bytes_without_breaking_utf8_across_reads() {
        // R3: the two halves of a multibyte character arrive in separate
        // reads. The byte buffer must hold the first half and reassemble
        // the character, not emit two U+FFFD.
        let mut buffer = Vec::new();
        // "café" with é split across the chunk boundary: no line complete yet.
        buffer.extend_from_slice(b"caf");
        assert!(drain_complete_lines(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\xc3\xa9\n");
        assert_eq!(
            drain_complete_lines(&mut buffer),
            vec!["caf\u{e9}".to_string()]
        );
        assert_eq!(buffer, Vec::<u8>::new());

        // Multiple complete lines at once, plus a trailing partial one that
        // stays buffered for the next read; CRLF endings are cleaned and
        // empty lines skipped like before.
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"one\r\nplain text\n\npartial");
        assert_eq!(
            drain_complete_lines(&mut buffer),
            vec!["one".to_string(), "plain text".to_string()]
        );
        assert_eq!(buffer, b"partial".to_vec());

        // A second multibyte split, completing in the NEXT chunk: the full
        // character's bytes (東, U+6771 = e6 9d b1) split from its newline.
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"gateway \xe6\x9d");
        assert!(drain_complete_lines(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\xb1\n");
        assert_eq!(
            drain_complete_lines(&mut buffer),
            vec!["gateway \u{6771}".to_string()]
        );

        // An invalid line (a truncated write made visible) decodes lossily —
        // one U+FFFD per bad byte at most, never a panic.
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"bad \xff\xfe\n");
        assert_eq!(
            drain_complete_lines(&mut buffer),
            vec!["bad \u{fffd}\u{fffd}".to_string()]
        );
    }

    #[test]
    fn parses_lock_pids_within_the_positive_i32_range() {
        // C6: values above i32::MAX must not wrap negative through `as`
        // (kill(-1, 0) = "every process" = phantom alive).
        assert_eq!(parse_lock_pid("4242\n"), Some(4242));
        assert_eq!(parse_lock_pid("  4242  "), Some(4242));
        assert_eq!(parse_lock_pid("0"), None);
        assert_eq!(parse_lock_pid("2147483648"), None);
        assert_eq!(parse_lock_pid("4294967295"), None);
        assert_eq!(parse_lock_pid("99999999999999"), None);
        // Empty or garbage lock content is stale-removable, never alive.
        assert_eq!(parse_lock_pid(""), None);
        assert_eq!(parse_lock_pid("not a pid"), None);
        assert_eq!(parse_lock_pid("-1"), None);
    }

    #[test]
    fn admin_timeout_errors_reach_the_setup_hint_unchanged() {
        // R2/R5: the timeout string is an operational failure, not a setup
        // one — it must pass through and be surfaced by the caller.
        let setup = Path::new("/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh");
        let error = "admin op 'stop' timed out";
        assert_eq!(admin_error_hint(setup, error), error);
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
    fn snapshot_v4_matches_only_the_own_service_and_set() {
        let raw = "Wi-Fi\n1.1.1.1\n8.8.8.8\nipv6:\n2001:db8::53\n";
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Same service, same set (any order): the user's own pre-VPN state.
        assert!(snapshot_v4_matches(
            raw,
            "Wi-Fi",
            &set(&["8.8.8.8", "1.1.1.1"])
        ));
        // The v6 section after the marker must not leak into the v4 set.
        assert!(!snapshot_v4_matches(
            raw,
            "Wi-Fi",
            &set(&["8.8.8.8", "1.1.1.1", "2001:db8::53"])
        ));
        // Different service: never a match.
        assert!(!snapshot_v4_matches(raw, "Ethernet", &set(&["1.1.1.1"])));
        // Different set: not the pre-VPN state.
        assert!(!snapshot_v4_matches(raw, "Wi-Fi", &set(&["9.9.9.9"])));
        // Empty snapshot / no service line: unknown, never a match.
        assert!(!snapshot_v4_matches("", "Wi-Fi", &set(&["1.1.1.1"])));
        assert!(!snapshot_v4_matches(
            "Wi-Fi\nipv6:\n",
            "Wi-Fi",
            &set(&["1.1.1.1"])
        ));
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

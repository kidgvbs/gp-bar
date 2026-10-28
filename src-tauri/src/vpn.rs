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
use tokio::{process::Command, sync::watch};

use crate::config::Config;

pub const LOCK_FILE: &str = "/var/run/gpclient.lock";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TAIL_INTERVAL: Duration = Duration::from_millis(400);

// No serde rename: the variant names ARE the wire format. The frontend
// (src/main.ts render()), the CSS `[data-phase="Connected"]` selector and
// the tray sync listener all match "Connected"… verbatim; a
// `rename_all = "snake_case"` here would silently break every panel state
// render (the switch falls through to the Disconnected default).
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
    /// While Connected: a fresh connect would go through a named gateway
    /// other than the live one (a gateway switch waiting to happen). The
    /// panel renders the amber Reconnect off it, the tray menu arms its
    /// Reconnect item off it — backend-owned, so the two can never drift.
    pub reconnect_armed: bool,
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
            reconnect_armed: false,
            last_error: None,
        }
    }
}

/// Identity of one connect attempt. Every async completion — the start
/// confirmation, the poller's dead-worker blame, a cancel — carries the id
/// of the attempt it belongs to, and the transition applies only if that
/// is still the current one (checked under the controller's lock): a
/// completion that arrives "later" belongs to a world that may have moved
/// on twice over (the attempt superseded, or resolved in place by a stale
/// lock adoption). Same principle as a fencing token in distributed locks.
///
/// Bumped ONLY by `enter_connecting` (every gated connect BEGIN): a
/// disconnect or a cancel ENDS an attempt, it never starts a new one —
/// their completions are phase-fenced instead (`disconnect_failed` only
/// ever fires while in the Disconnecting its own gate created; nothing
/// else may transition out from under it). The counter starts at 0, so
/// the `Default` id is never a live attempt's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AttemptId(u64);

impl AttemptId {
    /// The next attempt's id.
    fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Shared, lock-protected inner state.
#[derive(Default)]
struct Inner {
    state: VpnState,
    /// The connect attempt the current phase belongs to. Superseding it
    /// (a cancel, a failure, then a new connect) is what makes late
    /// completions stale: they carry the old id and no longer match.
    attempt: AttemptId,
    /// Whether the in-flight connect attempt's `admin start` has been
    /// confirmed (the wrapper returned Ok and loaded a fresh launchd job).
    /// The status poller may blame a job without a pid on a dead worker ONLY
    /// while this is true AND the attempt matches: before the confirmation,
    /// the job on the system is still the previous attempt's (stale, no
    /// pid) and would be reported as a false "worker exited" through the
    /// whole SSO login.
    worker_started: bool,
    /// What a fresh connect would target right now (the resolved gateway:
    /// explicit pick, else preferred — noted by `note_reconnect_target`
    /// after every config change that touches the resolution, seeded once
    /// at startup). `state.reconnect_armed` is derived from it on every
    /// update (`apply_reconnect_armed`); the controller never reads the
    /// config itself.
    reconnect_target: Option<String>,
}

/// The VPN state machine, with its publishing decoupled: `inner` is the
/// single source of truth, and every mutation sends the new [`VpnState`]
/// on the `tx` watch channel instead of emitting a Tauri event itself.
/// Publishing is structural — `main` owns the ONE subscriber
/// (`spawn_state_publisher`) that forwards each change to the tray and
/// then the frontend — so the controller carries no `AppHandle` and has
/// no opinion about UI. `watch` also coalesces: a subscriber that reads
/// only the latest value may miss transient intermediates, which the
/// consumers tolerate by rendering absolutely (from the state, never
/// from a diff).
pub struct VpnController {
    inner: Mutex<Inner>,
    tx: watch::Sender<VpnState>,
}

impl VpnController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            // The seed is the default state; `subscribe` marks it seen, so
            // the setup-time notes (portal, reconnect target) are the first
            // *changes* the publisher forwards — nothing is re-sent at spawn.
            tx: watch::channel(VpnState::default()).0,
        })
    }

    /// The live state feed. A late subscriber's `borrow` starts at the
    /// CURRENT value, then sees every change from there.
    pub fn subscribe(&self) -> watch::Receiver<VpnState> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> VpnState {
        self.inner.lock().unwrap().state.clone()
    }

    /// One state → every subscriber. `send` fails only when no receiver
    /// is alive (the publisher task holds one for the app's lifetime):
    /// ignored like the old `let _ = app.emit`, same "no one listens"
    /// semantics.
    fn publish(&self, state: VpnState) {
        let _ = self.tx.send(state);
    }

    /// Apply `mutate` under the lock and publish the result if anything
    /// observable changed. `mutate` returns a value so the fenced callers
    /// (see `connect_failed`) learn whether they were applied or ignored;
    /// every other caller returns `()`.
    fn update<T>(&self, mutate: impl FnOnce(&mut Inner) -> T) -> (T, VpnState) {
        let (value, state) = {
            let mut inner = self.inner.lock().unwrap();
            let before = inner.state.clone();
            let value = mutate(&mut inner);
            // The armed flag is derived state, never a second source of
            // truth: recompute it after every mutation so every published
            // state satisfies the formula (see `apply_reconnect_armed`).
            apply_reconnect_armed(&mut inner);
            if inner.state == before {
                // Nothing observable changed (e.g. a poll tick with the same
                // phase, or a fenced-off stale completion): skip the
                // publish instead of waking every listener.
                return (value, before);
            }
            (value, inner.state.clone())
            // The guard is dropped here, before publishing. `watch::send`
            // runs no listener code on this thread (unlike the old direct
            // emit, whose synchronous listeners re-reading `snapshot()`
            // self-deadlocked the poller), but dropping every guard before
            // any kind of notification stays the crate's uniform rule —
            // see the lock-order doc on `AppContext` in main.rs.
        };
        self.publish(state.clone());
        (value, state)
    }

    // -- connect ---------------------------------------------------------------

    /// Gate + transition in ONE critical section. Checking the phase with a
    /// separate `snapshot()` (lock taken and released) before transitioning
    /// (lock re-taken) is a TOCTOU hole: on the multi-threaded tokio runtime
    /// two invocations (double click, panel + tray menu) both pass the gate,
    /// and the second `admin start` unloads — killing — the first worker
    /// mid-SSO. The check and the transition must be atomic; the refusal
    /// error is the caller's to surface.
    ///
    /// On success the caller receives the NEW attempt's [`AttemptId`]: every
    /// async completion of this attempt (the start confirmation, a failure,
    /// a cancel) must carry it back so the transition can fence staleness.
    pub fn try_begin_connect(&self, portal: &str) -> Result<AttemptId, String> {
        let outcome = {
            let mut inner = self.inner.lock().unwrap();
            match enter_connecting(&mut inner, portal) {
                Ok(attempt) => Ok((attempt, inner.state.clone())),
                Err(phase) => Err(format!("cannot connect while {phase:?}")),
            }
            // Guard dropped BEFORE publishing (the `update` discipline).
        };
        match outcome {
            Ok((attempt, state)) => {
                self.publish(state);
                Ok(attempt)
            }
            Err(error) => Err(error),
        }
    }

    /// Gate + transition for a disconnect attempt, atomic like
    /// `try_begin_connect`. A second disconnect while already Disconnecting
    /// is idempotent (Ok): the stop request is already in flight.
    ///
    /// No `AttemptId`: a disconnect ENDS an attempt (it never begins a new
    /// one — see the type's doc), and its completions are phase-fenced: the
    /// only transition out of Disconnecting is the poller's filesystem truth
    /// (`apply_lock`), so there is nothing to fence against.
    pub fn try_begin_disconnect(&self) -> Result<(), String> {
        let outcome = {
            let mut inner = self.inner.lock().unwrap();
            match enter_disconnecting(&mut inner) {
                Ok(()) => Ok(inner.state.clone()),
                Err(phase) => Err(format!("cannot disconnect while {phase:?}")),
            }
            // Guard dropped BEFORE publishing (the `update` discipline).
        };
        match outcome {
            Ok(state) => {
                self.publish(state);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Gate for a cancel, atomic like the other begin-gates but
    /// transitionless: a cancel is addressed to the in-flight connect
    /// attempt, and its own completion (`connect_failed`) or the poller
    /// resolves the phase — an intermediate "Canceling" phase would race
    /// both. On success the caller receives the addressed attempt's id:
    /// the post-guard re-check and the completion fence on it.
    pub fn try_begin_cancel(&self) -> Result<AttemptId, String> {
        let inner = self.inner.lock().unwrap();
        match gate_cancel(&inner) {
            Ok(attempt) => Ok(attempt),
            Err(phase) => Err(format!("cannot cancel while {phase:?}")),
        }
        // No publish: a cancel transitions nothing (see `gate_cancel`).
    }

    /// A completion of connect attempt `id` that failed: applies ONLY if
    /// `id` is still the current attempt AND the phase is still Connecting —
    /// anything else is a stale completion (the attempt was superseded, or
    /// resolved in place: canceled, failed once, or graduated to Connected
    /// by a stale lock file) and is a logged no-op.
    pub fn connect_failed(&self, id: AttemptId, error: String) {
        let (applied, _) = self.update(|inner| apply_connect_failed(inner, id, &error));
        if !applied {
            log::debug!("connect_failed: ignoring a stale completion for attempt {id:?}");
        }
    }

    /// The wrapper's `start` returned Ok for attempt `id`: the launchd job
    /// now belongs to it, and the poller may trust its `pid =` line from
    /// here on. A marker from a superseded attempt is ignored: it must not
    /// arm the CURRENT attempt's blame gate. (A marker whose phase already
    /// left Connecting within the SAME attempt is still harmless, as
    /// before: the flag is only consulted via `connecting_attempt_started`,
    /// which re-checks the phase.)
    pub fn mark_worker_started(&self, id: AttemptId) {
        let (armed, _) = self.update(|inner| apply_worker_started(inner, id));
        if !armed {
            log::debug!("mark_worker_started: ignoring a stale marker for attempt {id:?}");
        }
    }

    // -- disconnect ------------------------------------------------------------
    /// The privileged teardown failed: surface the failure instead of a
    /// stuck "Disconnecting". The poller then reports the truth from the lock
    /// file (still connected → Connected again).
    pub fn disconnect_failed(&self, error: String) {
        self.update(|inner| {
            inner.state.phase = Phase::Error;
            inner.state.last_error = Some(error);
        });
    }

    // -- observations from the poller -------------------------------------------

    /// Apply filesystem evidence (lock file present/absent, PID alive/dead).
    pub fn observe_lock(&self, lock: Option<LockObservation>) {
        self.update(|inner| apply_lock(inner, lock));
    }

    /// The in-flight connect attempt's id, iff the phase is Connecting.
    /// The id-only half of the poller gate (no `worker_started`
    /// requirement): `cancel` uses it as the completion's id — the fence
    /// inside `connect_failed` then decides whether it still applies.
    pub fn connecting_attempt(&self) -> Option<AttemptId> {
        let inner = self.inner.lock().unwrap();
        (inner.state.phase == Phase::Connecting).then_some(inner.attempt)
    }

    /// Whether `id` is still the in-flight connect attempt: the fence a
    /// gated async op re-checks after an `.await` that could have
    /// outlived its own gate verdict (waiting for the admin-op lock, the
    /// privileged call itself).
    pub fn attempt_is_connecting(&self, id: AttemptId) -> bool {
        self.connecting_attempt() == Some(id)
    }

    /// The in-flight connect attempt's id, iff the poller may attribute a
    /// missing lock file to a dead worker: the phase must be Connecting AND
    /// the attempt's start must be confirmed (`mark_worker_started`, set
    /// once `run_admin("start")` returned Ok — for THIS attempt). Before
    /// that the launchd job is still the previous one (or not loaded), and
    /// a job without a `pid =` line would be reported as a false "worker
    /// exited" through the whole SSO login.
    ///
    /// Capturing the id here (under the same lock as the gate check) and
    /// passing it to the completion is the fencing pattern: the blame is
    /// issued for a KNOWN attempt, and `connect_failed` re-validates that
    /// it is still current after the `launchctl print` await that follows.
    pub fn connecting_attempt_started(&self) -> Option<AttemptId> {
        let inner = self.inner.lock().unwrap();
        (inner.state.phase == Phase::Connecting && inner.worker_started).then_some(inner.attempt)
    }

    /// A parsed gateway name from the JSON log stream.
    pub fn observe_gateway(&self, gateway: Option<String>) {
        if gateway.is_none() {
            return;
        }
        self.update(|inner| {
            inner.state.gateway = gateway;
        });
    }

    /// Apply a portal selection from the config to the idle state.
    ///
    /// `state.portal` is the only hint the panel has that a Connect target
    /// exists: the Connect button stays disabled while it is null, so the
    /// selection must be seeded into the idle state or a fresh session (or
    /// an app restart while a terminal-started worker holds the lock)
    /// leaves the button dead after the first disconnect. Only applies
    /// while idle: during Connecting/Connected/Disconnecting `portal`
    /// describes the in-flight operation and must not be rewritten by a
    /// selection change.
    pub fn note_selected_portal(&self, portal: Option<String>) {
        self.update(|inner| apply_selected_portal(inner, portal));
    }

    /// Record what a fresh connect would currently target (the resolved
    /// gateway), so `reconnect_armed` stays truthful without the controller
    /// ever reading the config: called after every config save that touches
    /// the resolution (see `sync_reconnect_target` in main.rs) and seeded
    /// once at startup. While Connected, a target differing from the live
    /// gateway arms the flag in the very same update — the published state
    /// carries it to the panel and the tray together.
    pub fn note_reconnect_target(&self, target: Option<String>) {
        self.update(|inner| {
            inner.reconnect_target = target;
        });
    }
}

/// Pure half of the armed flag: `reconnect_armed` is true exactly while
/// Connected AND the fresh-connect target names a gateway other than the
/// live one. `None` target = gpclient auto-gateway: nothing to switch to.
/// A `None` live gateway (the tunnel came up before the log tailer parsed
/// its name, or an outside-started worker never wrote our log) counts as a
/// switch for any explicit target — same semantics the frontend condition
/// this replaces had. Recomputed by `update` after every mutation.
fn apply_reconnect_armed(inner: &mut Inner) {
    let armed = inner.state.phase == Phase::Connected
        && inner.reconnect_target.is_some()
        && inner.reconnect_target != inner.state.gateway;
    inner.state.reconnect_armed = armed;
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
/// start a connect; the refusal carries the blocking phase. On success the
/// attempt counter is bumped and the NEW id is returned: every completion
/// of the attempt must carry it back (see `AttemptId`).
fn enter_connecting(inner: &mut Inner, portal: &str) -> Result<AttemptId, Phase> {
    if !matches!(inner.state.phase, Phase::Disconnected | Phase::Error) {
        return Err(inner.state.phase.clone());
    }
    inner.attempt = inner.attempt.next();
    inner.state.phase = Phase::Connecting;
    inner.state.portal = Some(portal.to_string());
    inner.state.gateway = None;
    inner.state.last_error = None;
    inner.state.connected_since_epoch_secs = None;
    // New attempt: its start is not confirmed yet (see `worker_started`).
    inner.worker_started = false;
    // Leaving Connected clears the armed flag in the state this transition
    // emits (it is Connected-only, see `apply_reconnect_armed`).
    apply_reconnect_armed(inner);
    Ok(inner.attempt)
}

/// Pure half of `VpnController::mark_worker_started`: arms the poller blame
/// gate only for the attempt the confirmation belongs to. A marker from a
/// superseded attempt (its `admin start` raced a cancel + retry) is refused:
/// the dead attempt must not arm the live one's gate. Within the SAME
/// attempt there is no phase check — a late-but-current marker stays
/// harmless exactly as before, because the gate (`connecting_attempt_started`)
/// re-checks the phase under the lock.
fn apply_worker_started(inner: &mut Inner, id: AttemptId) -> bool {
    if inner.attempt != id {
        return false;
    }
    inner.worker_started = true;
    true
}

/// Pure half of `VpnController::connect_failed`: the fenced failure
/// transition. Applies ONLY if the completion's attempt is still the
/// current one AND the phase is still Connecting — one fence for each way a
/// completion can be stale (the attempt was superseded; the phase resolved
/// in place, e.g. a stale lock graduated it to Connected). Returns whether
/// it applied, so the caller can log the ignored case.
fn apply_connect_failed(inner: &mut Inner, id: AttemptId, error: &str) -> bool {
    if inner.attempt != id || inner.state.phase != Phase::Connecting {
        return false;
    }
    inner.state.phase = Phase::Error;
    inner.state.last_error = Some(error.to_string());
    inner.worker_started = false;
    true
}

/// Pure half of `VpnController::try_begin_disconnect`. Connected proceeds;
/// Disconnecting is already in flight (idempotent Ok); anything else refuses.
fn enter_disconnecting(inner: &mut Inner) -> Result<(), Phase> {
    match inner.state.phase.clone() {
        Phase::Connected => {
            inner.state.phase = Phase::Disconnecting;
            // The armed flag must not go stale through the Disconnecting
            // window: clear it in the state this transition emits.
            apply_reconnect_armed(inner);
            Ok(())
        }
        Phase::Disconnecting => Ok(()),
        phase => Err(phase),
    }
}

/// Pure half of `VpnController::try_begin_cancel`: only a Connecting
/// phase has an attempt to cancel — every other phase refuses with
/// itself. No transition: the cancel's own completion
/// (`apply_connect_failed`) or the poller's lock observation resolves the
/// phase, so the gate only hands out the addressee's id for the caller to
/// fence with.
fn gate_cancel(inner: &Inner) -> Result<AttemptId, Phase> {
    match &inner.state.phase {
        Phase::Connecting => Ok(inner.attempt),
        phase => Err(phase.clone()),
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
            inner.state.pid = Some(obs.pid.get());
        }
        (Some(obs), Phase::Connecting) => {
            inner.state.phase = Phase::Connected;
            inner.state.connected_since_epoch_secs = Some(obs.connected_since_epoch_secs);
            inner.state.pid = Some(obs.pid.get());
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

/// A PID as kill(2)/proc_pidpath may legally receive it: positive and
/// within the i32 range, so never 0 (a process group) and never the
/// wrap-around `kill(-1, 0)` ("every process") a plain `u32 as
/// libc::pid_t` produces above `i32::MAX`. The invariant is the type:
/// parsing is the single validation point, and `raw()` is the only
/// `libc::pid_t` conversion in the crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pid(std::num::NonZeroI32);

impl std::str::FromStr for Pid {
    type Err = ();
    /// Rejects 0, negatives, anything above `i32::MAX` and garbage: such
    /// lock content counts as having no PID at all.
    fn from_str(raw: &str) -> Result<Self, ()> {
        let n: i32 = raw.trim().parse().map_err(|_| ())?;
        std::num::NonZeroI32::new(n)
            .filter(|n| n.get() > 0)
            .map(Pid)
            .ok_or(())
    }
}

impl std::fmt::Display for Pid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.get().fmt(f)
    }
}

impl Pid {
    /// The `libc::pid_t` every probe (kill, proc_pidpath) receives.
    fn raw(self) -> libc::pid_t {
        self.0.get()
    }

    /// The wire payload: `VpnState.pid` is a plain JSON number.
    fn get(self) -> u32 {
        self.0.get().unsigned_abs()
    }
}

/// What the PID lock file tells us right now.
pub struct LockObservation {
    pub pid: Pid,
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
    // a crash without cleanup the PID may be ANY process's. Require the
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

/// PID from the lock file (`gpclient` writes its own
/// `std::process::id()`). Parsing IS the validation: `Pid` exists only
/// for positive in-range values, so the probes can neither target a
/// process group nor wrap into `kill(-1, 0)`, and no `as libc::pid_t`
/// cast survives anywhere. Empty or garbage content is treated like a
/// missing PID.
fn parse_lock_pid(raw: &str) -> Option<Pid> {
    raw.trim().parse::<Pid>().ok()
}

/// `kill(pid, 0)` sends no signal: it only probes existence, and needs no
/// fork/exec (a `ps -p` probe would spawn a subprocess every poll tick).
/// When the probe fails with EPERM the process exists but belongs to root —
/// our worker. `Pid` is positive by construction, so this can never
/// address a process group.
fn process_alive(pid: Pid) -> bool {
    // SAFETY: kill(2) with signal 0 only performs the permission/existence
    // check; it never delivers a signal.
    let ret = unsafe { libc::kill(pid.raw(), 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether the process behind `pid` is actually a `gpclient` binary — the
/// best identity check available without privileges. `kill(pid, 0)`
/// returning EPERM only proves that SOME process owns the PID; after a
/// crash without cleanup the PID may have been reused by anything, and the
/// app would show Connected forever. `proc_pidpath` (libproc, unprivileged)
/// reports the executable path of the process: compare its basename against
/// the worker's binary name (setup-admin.sh copies gpclient to
/// `$PREFIX/bin/gpclient`, so the name is stable regardless of install).
#[cfg(target_os = "macos")]
fn process_is_gpclient(pid: Pid) -> bool {
    let mut buffer = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: proc_pidpath writes at most `buffersize` bytes into `buffer`
    // and only reads the integer `pid`; both come from here.
    let len =
        unsafe { libc::proc_pidpath(pid.raw(), buffer.as_mut_ptr().cast(), buffer.len() as u32) };
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
fn process_is_gpclient(_pid: Pid) -> bool {
    // No proc_pidpath off macOS: keep the kill(pid, 0) semantics only.
    true
}

// -- background tasks ---------------------------------------------------------

/// Poll the PID lock file and push state transitions to the publisher
/// (the controller's watch channel — no `AppHandle` needed here).
pub fn spawn_status_poller(controller: Arc<VpnController>) {
    tauri::async_runtime::spawn(async move {
        loop {
            let observation = read_lock_observation();
            // Connecting without a lock file means the tunnel never came up:
            // gpclient only writes the lock once it is established, so a dead
            // worker here is a failed attempt (wrong SSO, closed browser,
            // gateway error, crash) — surface it instead of spinning forever.
            // But only once the start is CONFIRMED for THIS attempt (the id
            // `connecting_attempt_started` captures under the same lock):
            // before that the job on the system is still the previous
            // attempt's, and its missing `pid =` line would be a false
            // "worker exited" sticking on screen through the whole SSO
            // login.
            // The one-shot job stays loaded (no KeepAlive), so probing it
            // needs the `pid =` line, not the command's exit status.
            if observation.is_none() {
                if let Some(attempt) = controller.connecting_attempt_started() {
                    if let Some(false) = worker_has_active_pid(WORKER_LABEL).await {
                        // The job is loaded but has no live process: gpclient
                        // ran and exited before the tunnel came up (wrong
                        // SSO, closed browser, gateway error, crash). The
                        // completion carries the id the gate captured: if
                        // the await above outlived the attempt (canceled and
                        // superseded, or graduated to Connected by the lock
                        // file appearing), `connect_failed` is a no-op.
                        controller.connect_failed(
                            attempt,
                            "The VPN worker exited before the tunnel came up".to_string(),
                        );
                    }
                }
                // Some(true) = still authenticating; None = the job is not
                // loaded yet or launchctl failed: nothing to report here.
                // `run_admin("start")` reports its own failure, and
                // cancel/stop set their own phases.
            }
            // `observe_lock` is filesystem truth and deliberately NOT
            // attempt-fenced: an outside-started worker (a terminal, or the
            // app restarting on a live tunnel) must be adopted as Connected
            // even though no attempt of ours exists to match.
            controller.observe_lock(observation);
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

/// Pure half of the tailer's startup decision: where to begin reading the
/// connect log. 0 (re-read from the start) only when the bytes on disk
/// belong to a live session — the phase says a connect is in flight or the
/// tunnel is up, or the lock file names a running gpclient (the app
/// restarted while the worker held the tunnel: the poller's adoption lags
/// this probe by a tick). Otherwise EOF: the root-owned log survives
/// reboots, and replaying a PREVIOUS session's log at startup would
/// observe its gateway and save its data as if it were live (N8).
/// (`log_len` 0 degenerates to 0 either way — an empty log has no lines
/// to mis-teach, and the next writer truncates before writing.)
fn initial_tail_offset(phase: Phase, live_worker: bool, log_len: u64) -> u64 {
    if matches!(phase, Phase::Connecting | Phase::Connected) || live_worker {
        0
    } else {
        log_len
    }
}

/// Pure half of the tailer's teaching gate: a gateway mention may teach —
/// observe into the state, learn the host pair, harvest the name — only
/// while a connect attempt is in flight or the tunnel is up. `emit_log_line`
/// ORs it with the tailer's sticky live-session flag: the phase alone
/// would drop the first lines of an adopted tunnel or a shrink-rewind
/// (the state lags the log by a tick), the flag alone is the startup
/// verdict frozen for the session.
fn gateway_teaching_allowed(phase: Phase) -> bool {
    matches!(phase, Phase::Connecting | Phase::Connected)
}

/// Tail the connect log file and stream parsed JSON records to the frontend.
pub fn spawn_log_tailer(app: AppHandle, controller: Arc<VpnController>) {
    tauri::async_runtime::spawn(async move {
        let path = Config::connect_log_path();
        // N8: begin at EOF unless the log on disk belongs to a live
        // session. The root-owned log survives reboots, and replaying the
        // previous session's log at startup taught its stale gateway into
        // the state and the config. The lock probe — not the poller's
        // adoption — decides the live-worker case: this task may run
        // before the poller's first observation.
        let log_len = tokio::fs::metadata(&path)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        let live_worker = read_lock_observation().is_some();
        let mut offset = initial_tail_offset(controller.snapshot().phase, live_worker, log_len);
        // Sticky, never reset: the bytes from offset 0 are a live session's
        // (see `initial_tail_offset`), and a shrink re-arms it — the wrapper
        // truncates before every start, so bytes that appear after a shrink
        // are a fresh attempt's. Between attempts the log only grows while
        // a worker writes it; the only lines taught without a live attempt
        // are a dying attempt's own trailing ones, which carry that
        // attempt's (correct) gateway.
        let mut attempt_live = offset == 0;
        // BYTE buffer, never a String: a multibyte UTF-8 character split
        // across two reads must not become two U+FFFD. Only complete
        // lines are decoded, inside `drain_complete_lines`.
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            // The connect flow truncates the file with `>` before gpclient starts,
            // so a shrink means a new connection attempt: rewind to the start.
            if let Ok(meta) = tokio::fs::metadata(&path).await {
                if meta.len() < offset {
                    offset = 0;
                    buffer.clear();
                    attempt_live = true;
                }
                if meta.len() > offset {
                    offset =
                        tail_chunk(&path, offset, &mut buffer, &app, &controller, attempt_live)
                            .await;
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
/// return the new offset. `attempt_live` is the tailer's sticky live-session
/// verdict, passed through to the teaching gate in `emit_log_line`.
async fn tail_chunk(
    path: &Path,
    offset: u64,
    buffer: &mut Vec<u8>,
    app: &AppHandle,
    controller: &VpnController,
    attempt_live: bool,
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
        emit_log_line(app, controller, &line, attempt_live).await;
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

async fn emit_log_line(
    app: &AppHandle,
    controller: &VpnController,
    line: &str,
    attempt_live: bool,
) {
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

    // N8: teach only from a live attempt's log. `attempt_live` is the
    // tailer's sticky verdict that these bytes belong to a session of
    // THIS app (startup with a live worker, or a shrink-rewind); the
    // phase check covers the rest. A replayed previous-session line
    // still reaches the panel below — only the teaching is gated, never
    // the display stream.
    let teach = attempt_live || gateway_teaching_allowed(controller.snapshot().phase);
    // Extract the gateway from the connection log messages.
    match parse_gateway(&event.message) {
        // A selection line: learn the pair if present, then observe and
        // harvest the NAME (never the address — the picker stores names).
        Some(GatewayMention::Selected { name, address }) if teach => {
            if let Some(address) = address {
                learn_gateway_host(app, &address, &name).await;
            }
            controller.observe_gateway(Some(name.clone()));
            harvest_gateway(app, &name).await;
        }
        // A getconfig POST: the cached-cookie path emits only this. Translate
        // to the picker name via the config map; display the raw FQDN when
        // unknown, but harvest only a known NAME (never the FQDN).
        Some(GatewayMention::Host(host)) if teach => {
            let name = gateway_display_name(app, &host);
            controller.observe_gateway(Some(name.clone().unwrap_or(host)));
            if let Some(name) = name {
                harvest_gateway(app, &name).await;
            }
        }
        _ => {}
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
///
/// Runs on the log tailer's async task, but the config save is blocking I/O
/// behind a mutex: the whole `update_config` goes to the blocking pool
/// (N13), and the emit happens back on the async side with every guard
/// already dropped.
async fn learn_gateway_host(app: &AppHandle, address: &str, name: &str) {
    let app_for_save = app.clone();
    let address = address.to_string();
    let name = name.to_string();
    // The Option layer is "no AppContext managed" (None); the inner Result
    // is the save outcome.
    let outcome = tokio::task::spawn_blocking(move || {
        use tauri::Manager;
        // No AppContext managed (?): None — the event was not ours to save.
        let ctx = app_for_save.try_state::<crate::AppContext>()?;
        // update_config: mutate a clone, save, commit only on success — this
        // used to mutate the live config before saving (see AppContext).
        Some(ctx.update_config(move |cfg| {
            if cfg.gateway_hosts.get(&address).map(String::as_str) == Some(name.as_str()) {
                return Ok(false);
            }
            cfg.gateway_hosts.insert(address, name);
            Ok(true)
        }))
    })
    .await;
    match outcome {
        Ok(Some(Ok((true, cfg)))) => {
            // Guard dropped BEFORE emitting (lock-order doc on
            // `AppContext` in main.rs).
            let _ = app.emit("config-updated", &cfg);
        }
        Ok(Some(Ok((false, _)))) | Ok(None) => {}
        Ok(Some(Err(error))) => log::warn!("cannot save the gateway host map: {error}"),
        Err(error) => log::warn!("gateway host map save task failed: {error}"),
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
///
/// Runs on the log tailer's async task, but the config save is blocking I/O
/// behind a mutex: the whole `update_config` goes to the blocking pool
/// (N13), and the emit happens back on the async side with every guard
/// already dropped.
async fn harvest_gateway(app: &AppHandle, gateway: &str) {
    // Same charset the wrapper and `select_gateway` enforce: a name
    // harvested straight from the log must not reach the picker if the
    // wrapper would refuse it (the picker would offer a dead entry).
    if !crate::config::valid_name(gateway) {
        log::warn!("skipping harvested gateway {gateway:?}: invalid name");
        return;
    }
    let app_for_save = app.clone();
    let gateway = gateway.to_string();
    // The Option layer is "no AppContext managed" (None); the inner Result
    // is the save outcome.
    let outcome = tokio::task::spawn_blocking(move || {
        use tauri::Manager;
        // No AppContext managed (?): None — the event was not ours to save.
        let ctx = app_for_save.try_state::<crate::AppContext>()?;
        // update_config: mutate a clone, save, commit only on success (see
        // AppContext) — the unchanged case skips the disk write too.
        Some(ctx.update_config(move |cfg| {
            if cfg.gateways.iter().any(|g| g == &gateway) {
                return Ok(false);
            }
            cfg.gateways.push(gateway);
            Ok(true)
        }))
    })
    .await;
    match outcome {
        Ok(Some(Ok((true, cfg)))) => {
            // Guard dropped BEFORE emitting (lock-order doc on
            // `AppContext` in main.rs).
            let _ = app.emit("config-updated", &cfg);
        }
        Ok(Some(Ok((false, _)))) | Ok(None) => {}
        Ok(Some(Err(error))) => log::warn!("cannot save the harvested gateway: {error}"),
        Err(error) => log::warn!("harvested gateway save task failed: {error}"),
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
    let order = command_stdout("/usr/sbin/networksetup", &["-listnetworkserviceorder"]).await;
    let Some(service) = parse_service_for_device(&order, &interface) else {
        return (None, Vec::new());
    };
    let dns = command_stdout("/usr/sbin/networksetup", &["-getdnsservers", &service]).await;
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

async fn command_stdout(program: &str, args: &[&str]) -> String {
    match tokio::process::Command::new(program)
        .args(args)
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
/// is root-owned under /Library/PrivilegedHelperTools: every parent of
/// that path is system-owned, unlike a /usr/local prefix that Homebrew
/// chowns to the user on Intel Macs.
pub const ADMIN_WRAPPER: &str = "/Library/PrivilegedHelperTools/gp-bar/admin";

/// The gpclient the admin wrapper actually executes (installed by the setup
/// script under the same root-owned prefix as `ADMIN_WRAPPER`). The version
/// reported in the About box is resolved from THIS binary when it exists, so
/// the displayed version matches what the worker really runs.
pub const WORKER_GPCLIENT: &str = "/Library/PrivilegedHelperTools/gp-bar/bin/gpclient";

/// One privileged helper operation at a time, process-wide: the guard is
/// held ACROSS each `run_admin` await in connect/disconnect/cancel/repair.
/// A `tokio::sync::Mutex` because the guard legitimately crosses an
/// `.await` — everything else in the crate keeps the std `Mutex`
/// discipline documented on `AppContext` (never across an await).
///
/// What it buys: the wrapper's operations all write the same launchd job,
/// and two of them racing produce a lost update the state machine cannot
/// see. The user-visible one is the lost cancel — Cancel clicked within
/// the first second of Connect used to `unload` the job BEFORE the
/// start's `load` ran, unload nothing, show "Connection canceled", and
/// then let the start load the job anyway: the SSO browser opens and the
/// VPN connects. Serialized, the cancel waits for the start's sudo to
/// finish and THEN unloads a job that is actually loaded. The same
/// ordering protects stop vs. a concurrent start (no half-fresh job for
/// the next teardown to miss). The wrapper itself stays stateless; this
/// lock sequences the APP's operations, nothing more.
pub static ADMIN_OPS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long to wait for one privileged helper operation before giving up
/// and reporting a timeout. The wrapper bounds every internal wait BELOW
/// this (see scripts/gp-admin): a `stop` is at worst 5 s
/// (`gpclient disconnect`) + 10 s (lock-file wait) + 5 s (final job
/// unload) = 20 s, a `cancel` at worst 10 s, the rest ≈1 s — so hitting
/// this timeout means the wrapper itself is stuck (a hung sudo, a wedged
/// launchctl), never a merely slow teardown. 30 s keeps ~10 s of margin
/// over that worst case while still resolving a stuck "Disconnecting…"
/// within half a minute instead of never. launchd's ExitTimeOut (60 s)
/// stays the independent backstop underneath both budgets: it SIGKILLs a
/// worker that ignores SIGTERM even after everyone above gave up waiting
/// for it.
const ADMIN_OP_TIMEOUT: Duration = Duration::from_secs(30);

/// A privileged-helper failure, classified so the consumer can tell the
/// classes apart: a missing one-time setup needs the user's action, a
/// timeout can be retried, a `Failed` is whatever the wrapper said.
///
/// `Display` is the user-facing text AND a stable grep contract — the
/// setup-hint especially is byte-identical to what the old string-based
/// errors carried (panel history and muscle memory match it). The serde
/// shape `{kind, detail}` is the wire format: Tauri rejects the command's
/// promise with the serialized value, and the frontend maps it back to
/// those same strings (see `commandErrorText` in src/main.ts).
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize)]
#[serde(tag = "kind", content = "detail")]
pub enum AdminError {
    /// The wrapper/sudoers setup is missing on this machine.
    #[error(
        "The privileged helper is not installed. One-time setup (Terminal):\n  sudo bash '{setup_script}'"
    )]
    NotInstalled { setup_script: PathBuf },
    /// The helper op did not finish within `ADMIN_OP_TIMEOUT`.
    #[error("admin op '{op}' timed out")]
    Timeout { op: &'static str },
    /// The op ran and failed. `stderr` is the wrapper's stderr verbatim —
    /// or, for the rarer shapes (sudo itself could not spawn, a non-zero
    /// exit with no stderr), the same text the old formatting produced.
    #[error("{stderr}")]
    Failed { op: &'static str, stderr: String },
}

/// Run one admin operation (`selftest`, `start`, `stop`, `cancel`).
///
/// `sudo -n` never prompts: without the sudoers rule it fails fast, and
/// `admin_error_hint` classifies that failure into the one-time setup
/// instructions. No credentials are ever seen or handled by GP Bar — the
/// whole point of the wrapper design (`pam_tid` falls back to the password
/// for any process that is not an interactive terminal, so Touch ID
/// cannot gate the helper).
///
/// Errors are typed, not formatted: the timeout, a spawn failure and a
/// non-zero exit are distinct [`AdminError`]s (the last two keep their
/// old text inside `Failed::stderr`); the caller classifies the raw value
/// through `admin_error_hint`.
pub async fn run_admin(op: &'static str, args: &[&str]) -> Result<String, AdminError> {
    // LC_ALL/LANG=C: sudo and the wrapper run with stable, non-localized
    // error strings, which the classification matches on.
    let fut = Command::new("/usr/bin/sudo")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .arg("-n")
        .arg(ADMIN_WRAPPER)
        .arg(op)
        .args(args)
        // Kill the child if this future is dropped by the timeout below.
        // Honest about what that buys: it kills the SUDO parent only —
        // the root shell it spawned, and anything that shell left behind,
        // can survive it orphaned (sudo forwards no signal here, and the
        // worker is launchd's child, not ours). A last-resort resource
        // guard against a stray root process, NOT a reliable teardown —
        // the wrapper's own bounded waits, with launchd's ExitTimeOut
        // below them, are the real bounds.
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(ADMIN_OP_TIMEOUT, fut)
        .await
        .map_err(|_| AdminError::Timeout { op })?
        .map_err(|e| AdminError::Failed {
            op,
            stderr: format!("sudo failed to run: {e}"),
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else if stderr.is_empty() {
        Err(AdminError::Failed {
            op,
            stderr: format!("admin op '{op}' failed: exit {}", output.status),
        })
    } else {
        Err(AdminError::Failed { op, stderr })
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

/// Classify a raw admin failure from the live system: the only
/// environment-dependent input is the wrapper's existence check.
pub fn admin_error_hint(setup_script: &Path, error: AdminError) -> AdminError {
    admin_error_hint_for(Path::new(ADMIN_WRAPPER).exists(), setup_script, error)
}

/// Pure core of [`admin_error_hint`] and the whole classification:
/// `wrapper_installed` is the caller's locale-independent existence check
/// so tests are environment-free.
///
/// A missing wrapper file, or sudo's password-prompt / command-not-found
/// stderr, means the one-time setup is missing — whatever the raw failure
/// said. Every other failure passes through unchanged, with the same
/// `Display` the raw value already had.
pub fn admin_error_hint_for(
    wrapper_installed: bool,
    setup_script: &Path,
    error: AdminError,
) -> AdminError {
    // No wrapper file means the setup is missing, whatever the exact
    // sudo/OS error string says. sudo's stderr is forced to LC_ALL=C by
    // `run_admin`, so the string matches below are stable, but the
    // existence check works even without them.
    if !wrapper_installed {
        return AdminError::NotInstalled {
            setup_script: setup_script.to_path_buf(),
        };
    }
    match error {
        AdminError::Failed { stderr, .. }
            if stderr.contains("a password is required")
                || stderr.contains("command not found") =>
        {
            AdminError::NotInstalled {
                setup_script: setup_script.to_path_buf(),
            }
        }
        other => other,
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
        let not_installed = |error: AdminError| match error {
            AdminError::NotInstalled { setup_script } => {
                assert_eq!(setup_script, setup);
                true
            }
            _ => false,
        };
        let failed = |stderr: &str| AdminError::Failed {
            op: "start",
            stderr: stderr.to_string(),
        };

        // A missing wrapper beats everything, whatever the raw failure was:
        // the machine has never run the one-time setup.
        assert!(not_installed(admin_error_hint_for(
            false,
            setup,
            failed("sudo: some other failure")
        )));
        assert!(not_installed(admin_error_hint_for(
            false,
            setup,
            AdminError::Timeout { op: "stop" }
        )));
        // Installed, but sudo wants a password (no NOPASSWD rule) or cannot
        // find the wrapper path: the setup is missing in those shapes too.
        assert!(not_installed(admin_error_hint_for(
            true,
            setup,
            failed("sudo: a password is required")
        )));
        assert!(not_installed(admin_error_hint_for(
            true,
            setup,
            failed("sudo: /Library/PrivilegedHelperTools/gp-bar/admin: command not found")
        )));
        // Operational failures pass through untouched.
        let passthrough = failed("gp-admin: the VPN worker did not stop");
        assert_eq!(
            admin_error_hint_for(true, setup, passthrough.clone()),
            passthrough
        );
    }

    /// The user-facing texts are a stable grep contract (RECAP quotes,
    /// panel history, muscle memory): the typed errors must format to
    /// byte-identical strings — including the two rare `Failed` shapes
    /// `run_admin` synthesizes with the old wording.
    #[test]
    fn admin_error_texts_are_byte_identical() {
        let setup =
            PathBuf::from("/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh");
        assert_eq!(
            AdminError::NotInstalled {
                setup_script: setup.clone()
            }
            .to_string(),
            "The privileged helper is not installed. One-time setup (Terminal):\n  \
             sudo bash '/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh'"
        );
        assert_eq!(
            AdminError::Timeout { op: "stop" }.to_string(),
            "admin op 'stop' timed out"
        );
        assert_eq!(
            AdminError::Failed {
                op: "start",
                stderr: "sudo failed to run: No such file or directory (os error 2)".to_string(),
            }
            .to_string(),
            "sudo failed to run: No such file or directory (os error 2)"
        );
        assert_eq!(
            AdminError::Failed {
                op: "stop",
                stderr: "admin op 'stop' failed: exit exit status: 1".to_string(),
            }
            .to_string(),
            "admin op 'stop' failed: exit exit status: 1"
        );
        // A wrapper stderr surfaces verbatim, not wrapped.
        assert_eq!(
            AdminError::Failed {
                op: "stop",
                stderr: "gp-admin: the VPN worker did not stop".to_string(),
            }
            .to_string(),
            "gp-admin: the VPN worker did not stop"
        );
    }

    /// The wire shape of a rejected command promise: Tauri serializes the
    /// error value, so the frontend receives `{kind, detail}` and maps it
    /// back to the same strings (`commandErrorText` in src/main.ts).
    #[test]
    fn admin_errors_serialize_to_kind_and_detail() {
        let setup =
            PathBuf::from("/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh");
        let value = serde_json::to_value(AdminError::NotInstalled {
            setup_script: setup,
        })
        .unwrap();
        assert_eq!(value["kind"], "NotInstalled");
        assert_eq!(
            value["detail"]["setup_script"],
            "/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh"
        );

        let value = serde_json::to_value(AdminError::Timeout { op: "stop" }).unwrap();
        assert_eq!(value["kind"], "Timeout");
        assert_eq!(value["detail"]["op"], "stop");

        let value = serde_json::to_value(AdminError::Failed {
            op: "start",
            stderr: "boom".to_string(),
        })
        .unwrap();
        assert_eq!(value["kind"], "Failed");
        assert_eq!(value["detail"]["op"], "start");
        assert_eq!(value["detail"]["stderr"], "boom");
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
            pid: Pid(std::num::NonZeroI32::new(4242).unwrap()),
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

    /// A cancel is addressed to the in-flight connect attempt: the gate
    /// hands out its id and mutates nothing — the completion or the
    /// poller resolves the phase, never the gate itself.
    #[test]
    fn cancel_gate_only_allows_connecting_and_transitions_nothing() {
        let mut inner = Inner::default();
        let attempt = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        inner.worker_started = true;
        assert_eq!(gate_cancel(&inner), Ok(attempt));
        assert_eq!(inner.state.phase, Phase::Connecting);
        assert_eq!(inner.attempt, attempt);
        assert!(inner.worker_started);

        // Every other phase refuses with itself — a cancel must never
        // touch a live tunnel (Connected) or a teardown in flight.
        for phase in [
            Phase::Disconnected,
            Phase::Connected,
            Phase::Disconnecting,
            Phase::Error,
        ] {
            let mut inner = Inner::default();
            inner.state.phase = phase.clone();
            assert_eq!(gate_cancel(&inner), Err(phase));
        }
    }

    /// The armed condition, moved here from main.rs's `tray_reconnect_armed`
    /// with the flag itself: armed exactly when a fresh connect would go
    /// through a named gateway other than the live tunnel's.
    #[test]
    fn reconnect_arms_only_on_a_real_gateway_switch() {
        let g = |s: &str| Some(s.to_string());
        let armed_at = |target: Option<String>, live: Option<String>| {
            let mut inner = Inner::default();
            inner.state.phase = Phase::Connected;
            inner.state.gateway = live;
            inner.reconnect_target = target;
            apply_reconnect_armed(&mut inner);
            inner.state.reconnect_armed
        };
        // Target equal to the live gateway: no switch.
        assert!(!armed_at(g("gw-1"), g("gw-1")));
        // Target differing from the live gateway: switch.
        assert!(armed_at(g("gw-1"), g("gw-2")));
        // No target (gpclient auto-gateway): nothing to switch to.
        assert!(!armed_at(None, g("gw-1")));
        assert!(!armed_at(None, None));
        // Live gateway unknown (tunnel up before the tailer parsed its
        // name): any explicit target counts as a switch.
        assert!(armed_at(g("gw-1"), None));
    }

    /// The armed flag is a Connected-only affordance: every other phase
    /// keeps it false, and the transitions leaving Connected clear it in
    /// the state they emit instead of letting it go stale through the
    /// Disconnecting window.
    #[test]
    fn reconnect_arms_only_while_connected() {
        let g = |s: &str| Some(s.to_string());
        for phase in [
            Phase::Disconnected,
            Phase::Connecting,
            Phase::Disconnecting,
            Phase::Error,
        ] {
            let mut inner = Inner::default();
            inner.state.phase = phase;
            inner.state.gateway = g("gw-2");
            inner.reconnect_target = g("gw-1");
            apply_reconnect_armed(&mut inner);
            assert!(!inner.state.reconnect_armed);
        }

        // Armed while Connected, cleared by the teardown transition.
        let mut inner = Inner::default();
        inner.state.phase = Phase::Connected;
        inner.state.gateway = g("gw-2");
        inner.reconnect_target = g("gw-1");
        apply_reconnect_armed(&mut inner);
        assert!(inner.state.reconnect_armed);
        enter_disconnecting(&mut inner).unwrap();
        assert!(!inner.state.reconnect_armed);
    }

    #[test]
    fn poller_exit_gate_requires_a_confirmed_start() {
        // Connecting but `run_admin("start")` not back yet: the poller
        // must NOT blame the stale launchd job for the exit.
        let mut inner = Inner::default();
        inner.state.phase = Phase::Connecting;
        apply_lock(&mut inner, None);
        assert_eq!(inner.state.phase, Phase::Connecting);

        let controller = VpnController::new();
        let attempt = controller.try_begin_connect("vpn.example.com").unwrap();
        assert_eq!(controller.connecting_attempt_started(), None);

        // Once the start is confirmed, the gate opens — and hands the
        // poller the attempt id to fence its completion with.
        controller.mark_worker_started(attempt);
        assert_eq!(controller.connecting_attempt_started(), Some(attempt));

        // ...and if the phase already moved on (stale lock graduated it to
        // Connected, or a cancel failed the attempt), marking is harmless:
        // the flag alone never re-opens the gate.
        {
            let mut i = controller.inner.lock().unwrap();
            i.state.phase = Phase::Error;
        }
        controller.mark_worker_started(attempt);
        assert_eq!(controller.connecting_attempt_started(), None);

        // Teardown and failure clear it for the next attempt too.
        {
            let mut i = controller.inner.lock().unwrap();
            i.state.phase = Phase::Connected;
            i.worker_started = true;
        }
        apply_lock(&mut controller.inner.lock().unwrap(), None);
        assert_eq!(controller.connecting_attempt_started(), None);
    }

    /// Publishing goes through the watch channel: every real transition
    /// reaches the subscribers, no-op mutations and refused gates publish
    /// nothing, and a late subscriber still sees the latest state first.
    /// Uses only the sync receiver APIs (`has_changed`,
    /// `borrow_and_update`) — no async runtime in unit tests.
    /// Race (a), through the controller: a completion arriving for an
    /// attempt that already RESOLVED (a stale lock graduated it to
    /// Connected) is a no-op — it neither overwrites the phase nor
    /// publishes anything.
    #[test]
    fn a_late_connect_completion_must_not_overwrite_a_resolved_attempt() {
        let controller = VpnController::new();
        let mut rx = controller.subscribe();
        let attempt = controller.try_begin_connect("vpn.example.com").unwrap();
        // The tunnel came up (stale-lock adoption): the attempt RESOLVED as
        // Connected...
        controller.observe_lock(Some(LockObservation {
            pid: Pid(std::num::NonZeroI32::new(4242).unwrap()),
            connected_since_epoch_secs: 1_700_000_000,
        }));
        assert_eq!(controller.snapshot().phase, Phase::Connected);
        assert!(rx.has_changed().unwrap()); // the graduation was published
        assert_eq!(rx.borrow_and_update().phase, Phase::Connected);
        // ...and THEN the poller's dead-worker blame for that same attempt
        // arrives late (its `launchctl print` await resolved after the
        // adoption tick). The blame belongs to an attempt that already
        // succeeded: applying it must be impossible, and a no-op
        // completion must not even wake the subscribers.
        controller.connect_failed(
            attempt,
            "The VPN worker exited before the tunnel came up".to_string(),
        );
        assert_eq!(controller.snapshot().phase, Phase::Connected);
        assert!(!rx.has_changed().unwrap());
    }

    /// Race (a) again, superseded rather than resolved: the attempt was
    /// canceled and a NEW one began; the old attempt's late blame must not
    /// fail the new one.
    #[test]
    fn a_late_connect_completion_must_not_fail_a_superseding_attempt() {
        let controller = VpnController::new();
        let a = controller.try_begin_connect("vpn.example.com").unwrap();
        // Canceled mid-flight (the fence below is what a real cancel goes
        // through), then a new attempt begins.
        controller.connect_failed(a, "Connection canceled".to_string());
        let b = controller.try_begin_connect("vpn.example.com").unwrap();
        assert_ne!(a, b);
        // A's blame arrives late, carrying A's id.
        controller.connect_failed(
            a,
            "The VPN worker exited before the tunnel came up".to_string(),
        );
        // B is untouched: still Connecting, no error recorded for it.
        assert_eq!(controller.snapshot().phase, Phase::Connecting);
        assert_eq!(controller.snapshot().last_error, None);
        // B's own completion applies normally.
        controller.connect_failed(b, "B's own failure".to_string());
        assert_eq!(controller.snapshot().phase, Phase::Error);
        assert_eq!(
            controller.snapshot().last_error.as_deref(),
            Some("B's own failure")
        );
    }

    /// Race (b): a `worker_started` marker from a superseded attempt must
    /// not arm the CURRENT attempt's poller gate. The marker is
    /// attempt-scoped: A's late start confirmation is ignored, B's own
    /// arms B.
    #[test]
    fn a_stale_worker_started_marker_must_not_arm_the_current_attempt() {
        let controller = VpnController::new();
        let a = controller.try_begin_connect("vpn.example.com").unwrap();
        // The attempt is canceled mid-flight...
        controller.connect_failed(a, "Connection canceled".to_string());
        // ...a new attempt begins, and the FIRST attempt's `admin start`
        // confirmation arrives late (the sudo command raced the cancel).
        let b = controller.try_begin_connect("vpn.example.com").unwrap();
        controller.mark_worker_started(a);
        // The late marker belongs to the dead attempt: it must not let the
        // poller blame a job without a pid on the live one.
        assert_eq!(controller.connecting_attempt_started(), None);
        // B's own confirmation arms B.
        controller.mark_worker_started(b);
        assert_eq!(controller.connecting_attempt_started(), Some(b));
    }

    /// The pure halves of both fences, at the table level. A completion is
    /// stale in two dimensions and each has its own fence: the attempt id
    /// (superseded) and the phase (resolved in place). Only the CURRENT
    /// attempt, still Connecting, may fail.
    #[test]
    fn stale_connect_completions_are_no_ops_in_both_dimensions() {
        // Superseded: A canceled, B in flight, A's blame carries A's id.
        let mut inner = Inner::default();
        let a = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        assert!(apply_connect_failed(&mut inner, a, "Connection canceled"));
        let b = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        assert_ne!(a, b);
        assert!(!apply_connect_failed(&mut inner, a, "A's late blame"));
        assert_eq!(inner.state.phase, Phase::Connecting);
        assert_eq!(inner.state.last_error, None);
        assert!(apply_connect_failed(&mut inner, b, "B's own failure"));
        assert_eq!(inner.state.phase, Phase::Error);

        // Resolved in place: A graduated to Connected (stale-lock adoption)
        // before its blame arrived. Same id, wrong phase: still a no-op.
        let mut inner = Inner::default();
        let a = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        apply_lock(
            &mut inner,
            Some(LockObservation {
                pid: Pid(std::num::NonZeroI32::new(4242).unwrap()),
                connected_since_epoch_secs: 1_700_000_000,
            }),
        );
        assert_eq!(inner.state.phase, Phase::Connected);
        assert!(!apply_connect_failed(&mut inner, a, "A's late blame"));
        assert_eq!(inner.state.phase, Phase::Connected);

        // The stale marker fence, pure: A's late confirmation must not arm
        // B's gate — and within one attempt, marking still works even after
        // the phase moved on (the gate re-checks the phase under the lock).
        let mut inner = Inner::default();
        let a = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        apply_connect_failed(&mut inner, a, "Connection canceled");
        let b = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        assert!(!inner.worker_started);
        assert!(!apply_worker_started(&mut inner, a));
        assert!(!inner.worker_started);
        assert!(apply_worker_started(&mut inner, b));
        assert!(inner.worker_started);
    }

    /// The attempt ids themselves: monotonic from the first connect, never
    /// repeating, and never the `Default` id (0) — a completion built from
    /// the default cannot match anything live.
    #[test]
    fn attempt_ids_are_monotonic_and_never_the_default() {
        let mut inner = Inner::default();
        assert_eq!(inner.attempt, AttemptId::default());
        let first = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        assert_ne!(first, AttemptId::default());
        // Each attempt must resolve before the next begins (the gate
        // refuses a connect while one is in flight — pinned by
        // `connect_gate_only_allows_idle_phases`).
        assert!(apply_connect_failed(&mut inner, first, "done"));
        let second = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        assert!(apply_connect_failed(&mut inner, second, "done"));
        let third = enter_connecting(&mut inner, "vpn.example.com").unwrap();
        assert_ne!(first, second);
        assert_ne!(second, third);

        // A disconnect does NOT begin an attempt: the id stays put through
        // the teardown (its completions are phase-fenced, not id-fenced —
        // see `AttemptId`'s doc).
        let mut inner = Inner::default();
        inner.state.phase = Phase::Connected;
        let before = inner.attempt;
        enter_disconnecting(&mut inner).unwrap();
        assert_eq!(inner.attempt, before);
    }

    #[test]
    fn state_changes_publish_over_the_watch_channel() {
        let controller = VpnController::new();
        let mut rx = controller.subscribe();
        // The seed value is the current one: nothing has changed yet.
        assert!(!rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().phase, Phase::Disconnected);

        // A poll observation that changes nothing publishes nothing.
        controller.observe_lock(None);
        assert!(!rx.has_changed().unwrap());

        // A real transition publishes the new state... (a connect, then
        // that attempt's failure — the completion carries the id).
        let attempt = controller.try_begin_connect("vpn.example.com").unwrap();
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().phase, Phase::Connecting);
        controller.connect_failed(attempt, "boom".to_string());
        assert!(rx.has_changed().unwrap());
        let state = rx.borrow_and_update();
        assert_eq!(state.phase, Phase::Error);
        assert_eq!(state.last_error.as_deref(), Some("boom"));
        drop(state);

        // ...and a subscriber that joins later renders absolutely from
        // the latest value.
        let mut late = controller.subscribe();
        assert_eq!(late.borrow_and_update().phase, Phase::Error);

        // The gate messages are unchanged, and refusals publish nothing.
        // Error is an idle phase: the retry connect passes, then the
        // in-flight refusals apply.
        controller.try_begin_connect("vpn.example.com").unwrap();
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().phase, Phase::Connecting);
        assert_eq!(
            controller.try_begin_connect("other.example").unwrap_err(),
            "cannot connect while Connecting"
        );
        assert_eq!(
            controller.try_begin_disconnect().unwrap_err(),
            "cannot disconnect while Connecting"
        );
        assert!(!rx.has_changed().unwrap());

        // The poller's adoption observation publishes the graduation.
        controller.observe_lock(Some(LockObservation {
            pid: Pid(std::num::NonZeroI32::new(4242).unwrap()),
            connected_since_epoch_secs: 1_700_000_000,
        }));
        assert!(rx.has_changed().unwrap());
        let state = rx.borrow_and_update();
        assert_eq!(state.phase, Phase::Connected);
        assert_eq!(state.pid, Some(4242));
    }

    /// N8: the tailer starts at EOF unless the log on disk belongs to a
    /// live session — the root-owned log survives reboots, and replaying
    /// the previous session's log taught its stale gateway into the
    /// state and the config.
    #[test]
    fn tailer_startup_offset_is_eof_only_without_a_live_attempt() {
        // A previous session's log, tunnel down, no worker: skip it all.
        for phase in [Phase::Disconnected, Phase::Error, Phase::Disconnecting] {
            assert_eq!(initial_tail_offset(phase.clone(), false, 4096), 4096);
        }
        // A live session's log: re-read it from the start.
        assert_eq!(initial_tail_offset(Phase::Connecting, false, 4096), 0);
        assert_eq!(initial_tail_offset(Phase::Connected, false, 4096), 0);
        // The adopted tunnel: the phase has not caught up yet, the lock
        // probe has — the probe decides (the tailer may start before the
        // poller's first observation).
        assert_eq!(initial_tail_offset(Phase::Disconnected, true, 4096), 0);
        // Degenerate: an empty log is EOF either way — no lines to
        // mis-teach, and the next writer truncates before writing.
        assert_eq!(initial_tail_offset(Phase::Disconnected, false, 0), 0);
    }

    /// N8: a gateway mention teaches only while an attempt is live.
    #[test]
    fn gateway_teaching_requires_a_live_attempt() {
        assert!(gateway_teaching_allowed(Phase::Connecting));
        assert!(gateway_teaching_allowed(Phase::Connected));
        assert!(!gateway_teaching_allowed(Phase::Disconnected));
        assert!(!gateway_teaching_allowed(Phase::Disconnecting));
        assert!(!gateway_teaching_allowed(Phase::Error));
    }

    #[test]
    fn splits_lines_on_bytes_without_breaking_utf8_across_reads() {
        // The two halves of a multibyte character arrive in separate
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
        let pid = |n: i32| Pid(std::num::NonZeroI32::new(n).unwrap());
        // Values above i32::MAX are rejected at parse — the bounds are
        // the Pid type now, not a check callers must remember (a `u32`
        // `as libc::pid_t` would wrap negative: kill(-1, 0) = "every
        // process" = phantom alive).
        assert_eq!(parse_lock_pid("4242\n"), Some(pid(4242)));
        assert_eq!(parse_lock_pid("  4242  "), Some(pid(4242)));
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
    fn pids_are_positive_i32s_by_construction() {
        let pid = |n: i32| Pid(std::num::NonZeroI32::new(n).unwrap());
        // The wire payload is the plain number, never a wrapped negative.
        assert_eq!(pid(4242).get(), 4242);
        assert_eq!(pid(4242).raw(), 4242);
        // Display keeps the stale-lock warning's "PID 4242" shape.
        assert_eq!(pid(4242).to_string(), "4242");
        assert_eq!(pid(1).to_string(), "1");
        // FromStr rejects everything outside the positive i32 range.
        assert!("1".parse::<Pid>().is_ok());
        assert!("2147483647".parse::<Pid>().is_ok());
        for bad in [
            "0",
            "-1",
            "-2147483648",
            "2147483648",
            "",
            "  ",
            "x",
            "42x",
            "1.5",
        ] {
            assert!(bad.parse::<Pid>().is_err(), "{bad:?} must not be a Pid");
        }
        // The same trim the lock parse relies on.
        assert_eq!(" 42\n".parse::<Pid>().ok(), Some(pid(42)));
    }

    #[test]
    fn admin_timeout_errors_reach_the_setup_hint_unchanged() {
        // The timeout is an operational failure, not a setup one — it must
        // pass through and be surfaced by the caller.
        let setup = Path::new("/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh");
        let error = AdminError::Timeout { op: "stop" };
        assert_eq!(admin_error_hint_for(true, setup, error.clone()), error);
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

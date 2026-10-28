//! GP Bar — a menu bar client for the GlobalProtect VPN.
//!
//! It orchestrates the free, open-source `gpclient` CLI (GPL-3.0, from
//! <https://github.com/yuezk/GlobalProtect-openconnect>) and deliberately ships
//! none of the proprietary GUI licensing machinery: connections last as long
//! as the portal allows, with no client-side time limit.

mod config;
mod glass;
mod icons;
mod vpn;

use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Emitter, Listener, LogicalPosition, Manager, State, WindowEvent, Wry,
    menu::{ContextMenu, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

use config::{Config, valid_name};
use glass::PanelEffect;
use vpn::{Phase, VpnController, VpnState};

const TRAY_ID: &str = "gp-bar";

/// Handles for the right-click tray menu. The menu is NEVER attached to the
/// tray (see the tray comment in `main`): on this macOS beta a natively
/// attached status-item menu makes the system swallow left clicks before
/// the app ever sees them, killing the panel toggle. It is popped up BY
/// HAND on right-click instead — muda's position-less context-menu path
/// anchors at `NSEvent::mouseLocation` (the icon the user just
/// right-clicked) in screen coordinates, so the hidden panel window's
/// geometry never enters into it.
static TRAY_MENU: OnceLock<TrayMenuHandles> = OnceLock::new();

// Per-phase tray icons, rendered once and shared (the supersampled padlock
// render is pure and cheap, but it runs on every state event otherwise).
// tauri's image type owns its RGBA buffer, so clones hand out copies.
static CONNECTED_ICON: LazyLock<tauri::image::Image<'static>> =
    LazyLock::new(|| icons::padlock_icon(true));
static OPEN_ICON: LazyLock<tauri::image::Image<'static>> =
    LazyLock::new(|| icons::padlock_icon(false));

struct TrayMenuHandles {
    menu: Menu<Wry>,
    connect: MenuItem<Wry>,
    reconnect: MenuItem<Wry>,
    disconnect: MenuItem<Wry>,
}

/// Shared, lock-protected app state.
///
/// # Lock order
///
/// The crate has exactly three mutexes, and no code path holds two of
/// them at once — every guard is leaf-scoped, which keeps the couplings
/// below acyclic by construction:
///
/// 1. `config` — the persisted settings. Guard-scoped inside
///    `update_config` (and the plain reads): taken, cloned/mutated,
///    released; never held across an `.await` or an emit.
/// 2. `controller.inner` (vpn.rs) — the VPN state machine. Guard-scoped
///    inside `vpn::update` and the `try_begin_*` gates: the new state is
///    computed under the guard, the guard is dropped, and only then is
///    the state published on the watch channel.
/// 3. `panel_hidden_at` — a pure leaf: taken and released inline by
///    `toggle_panel` and the focus-loss handler, never nested with
///    anything.
///
/// If a nesting ever becomes unavoidable, it may only be config →
/// controller, never the reverse: `config-updated` is still emitted
/// directly (`broadcast_config`, and the gateway harvest in vpn.rs) and
/// its listener re-reads the controller (`snapshot`), so a config guard
/// held across that emit needs exactly this direction to stay
/// deadlock-free. The vpn-state path is structural — the publisher task
/// owns the only subscriber, and nothing emits it under any guard.
pub(crate) struct AppContext {
    controller: Arc<VpnController>,
    pub(crate) config: Mutex<Config>,
    /// When the panel was last hidden, to tell "the click that dismissed it"
    /// apart from a fresh open request (see `toggle_panel`).
    panel_hidden_at: Mutex<Option<Instant>>,

    /// Which panel backdrop (native liquid glass vs CSS vibrancy) was
    /// resolved at startup; reported to the frontend via `get_panel_effect`.
    panel_effect: PanelEffect,

    /// The `config::Loaded` branch the startup load took ("fresh", "from
    /// disk", "unreadable"), logged once in `setup`: `Config::load` runs
    /// before the log plugin is attached, so its failure branches can
    /// only eprintln (they do).
    config_load: &'static str,
    /// `Some(error)` only when the startup config file could not be READ
    /// (permissions, I/O — a corrupt file is a different branch: renamed
    /// aside, counts as fresh). The in-memory config is then NOT the
    /// user's data: every save is a no-op (`save_config`), and the panel
    /// shows the error once (`get_config_read_error`).
    config_read_error: Option<String>,
}

impl AppContext {
    /// The one way the config mutates: clone the live config, apply `f`,
    /// save the clone, and commit it to the live copy only when the save
    /// succeeded — a failed save must not leave memory diverged from
    /// disk, and `f` bailing with Err must touch neither. An unchanged
    /// result skips the disk write entirely.
    ///
    /// The config mutex is taken and released inside; anything that emits
    /// (`broadcast_config`) or publishes (`note_selected_portal`,
    /// `sync_reconnect_target`) may only run AFTER this returns — the
    /// guard-before-notification discipline, see the lock-order doc on
    /// `AppContext`.
    pub(crate) fn update_config<T>(
        &self,
        f: impl FnOnce(&mut Config) -> Result<T, String>,
    ) -> Result<(T, Config), String> {
        let mut cfg = self.config.lock().unwrap();
        let mut next = cfg.clone();
        let out = f(&mut next)?;
        if next != *cfg {
            self.save_config(&next)?;
            *cfg = next;
        }
        Ok((out, cfg.clone()))
    }

    /// Whether saves may write to disk: false only while the startup load
    /// found the config file unreadable — writing the in-memory defaults
    /// would then destroy the very file the app could not read. This is
    /// the `persist` flag of the config load (see `config::Loaded`).
    fn persist(&self) -> bool {
        self.config_read_error.is_none()
    }

    /// Save `cfg` to disk unless the startup load was unreadable (see
    /// `persist`): a no-op, logged on every attempt, never a hard error —
    /// the config still mutates in memory for this session.
    fn save_config(&self, cfg: &Config) -> Result<(), String> {
        if !self.persist() {
            log::warn!(
                "config: not saving — the file was unreadable at startup, writing would destroy it"
            );
            return Ok(());
        }
        cfg.save()
    }
}

// -- commands -----------------------------------------------------------------
//
// The mutating commands are `async` and run their whole `update_config`
// in `tokio::task::spawn_blocking`: non-async commands execute on the
// MAIN thread — the AppKit run loop — and every save fsyncs (tens of
// milliseconds of UI freeze per portal/gateway click); async ones run
// on tokio workers, where a blocking save silently stalls the runtime's
// capacity. Either way the file I/O belongs on the blocking pool. The
// config mutex is taken and released inside the closure, so the guard
// never crosses an `.await`.

#[tauri::command]
fn get_state(state: State<'_, AppContext>) -> VpnState {
    state.controller.snapshot()
}

/// Which backdrop the panel uses (`"liquid-glass"` or `"css-vibrancy"`), so
/// the frontend can drop its own blur/tint when the glass does the job.
#[tauri::command]
fn get_panel_effect(state: State<'_, AppContext>) -> &'static str {
    state.panel_effect.as_str()
}

#[tauri::command]
fn get_config(state: State<'_, AppContext>) -> Config {
    state.config.lock().unwrap().clone()
}

/// Why config changes cannot be saved, when the startup load hit an
/// unreadable file (permissions, I/O — a corrupt file is a different
/// branch and keeps saving). `None` while the config persists normally;
/// the panel shows the error once (see src/main.ts).
#[tauri::command]
fn get_config_read_error(state: State<'_, AppContext>) -> Option<String> {
    state.config_read_error.clone()
}

#[tauri::command]
async fn get_versions(state: State<'_, AppContext>) -> Result<VersionInfo, String> {
    let cfg = state.config.lock().unwrap().clone();
    let gpclient_version = match cfg.resolve_gpclient() {
        Some(path) => vpn::gpclient_version(&path).await,
        None => "not found".to_string(),
    };
    Ok(VersionInfo {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        gpclient_version,
    })
}

#[derive(serde::Serialize)]
struct VersionInfo {
    app_version: String,
    gpclient_version: String,
}

#[tauri::command]
async fn add_portal(
    app: AppHandle,
    state: State<'_, AppContext>,
    portal: String,
) -> Result<(), String> {
    let portal = portal.trim().to_string();
    if !valid_name(&portal) {
        return Err(
            "Portal may only contain letters, digits, dots, dashes and underscores (and must not start with a dash)"
                .to_string(),
        );
    }
    let app_for_save = app.clone();
    let (_, cfg) = tokio::task::spawn_blocking(move || {
        app_for_save.state::<AppContext>().update_config(|cfg| {
            if !cfg.portals.contains(&portal) {
                cfg.portals.push(portal.clone());
            }
            if cfg.selected.is_none() {
                cfg.selected = Some(portal);
            }
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    // Publish outside the config lock — update_config has dropped it.
    state.controller.note_selected_portal(cfg.selected.clone());
    Ok(())
}

#[tauri::command]
async fn remove_portal(
    app: AppHandle,
    state: State<'_, AppContext>,
    portal: String,
) -> Result<(), String> {
    let app_for_save = app.clone();
    let (_, cfg) = tokio::task::spawn_blocking(move || {
        app_for_save.state::<AppContext>().update_config(|cfg| {
            cfg.portals.retain(|p| p != &portal);
            if cfg.selected.as_deref() == Some(portal.as_str()) {
                cfg.selected = cfg.portals.first().cloned();
            }
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    state.controller.note_selected_portal(cfg.selected.clone());
    Ok(())
}

#[tauri::command]
async fn select_portal(
    app: AppHandle,
    state: State<'_, AppContext>,
    portal: String,
) -> Result<(), String> {
    let app_for_save = app.clone();
    let (_, cfg) = tokio::task::spawn_blocking(move || {
        app_for_save.state::<AppContext>().update_config(|cfg| {
            // An unknown portal must fail loud — not silently re-save and Ok.
            if !cfg.portals.contains(&portal) {
                return Err(format!("unknown portal: {portal}"));
            }
            cfg.selected = Some(portal);
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    state.controller.note_selected_portal(cfg.selected.clone());
    Ok(())
}

#[tauri::command]
async fn set_reuse_cookie(app: AppHandle, value: bool) -> Result<(), String> {
    // An unchanged value skips the disk write (see update_config).
    tokio::task::spawn_blocking(move || {
        app.state::<AppContext>().update_config(|cfg| {
            cfg.reuse_cookie = value;
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    Ok(())
}

/// A failed panel/tray action, typed the same way as [`vpn::AdminError`] so
/// the frontend can tell the classes apart: `Message` is a plain refusal
/// (the connect gates, an invalid gateway name), `Admin` wraps the
/// privileged-helper failure with its own `{kind, detail}` shape inside.
/// `Display` keeps the exact texts the old `Result<_, String>` carried —
/// the tray menu's `{error}` log lines and the panel's error row stay
/// byte-identical.
#[derive(Debug, thiserror::Error, serde::Serialize)]
#[serde(tag = "kind", content = "detail")]
enum ActionError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Admin(#[from] vpn::AdminError),
}

#[tauri::command]
async fn connect(
    app: AppHandle,
    state: State<'_, AppContext>,
    portal: String,
    reuse_cookie: bool,
    gateway: Option<String>,
) -> Result<(), ActionError> {
    do_connect(app, &state, &portal, reuse_cookie, gateway).await
}

#[tauri::command]
async fn disconnect(app: AppHandle, state: State<'_, AppContext>) -> Result<(), ActionError> {
    do_disconnect(app, &state).await
}

/// The armed gateway switch, from the panel's amber Reconnect button: the
/// backend owns the whole chain (`do_reconnect`), reading the connect
/// target from the config. Unlike `connect` — which still takes the
/// panel's parameters — there is nothing to forward from the frontend:
/// the armed flag and the target agree by construction.
#[tauri::command]
async fn reconnect(app: AppHandle, state: State<'_, AppContext>) -> Result<(), ActionError> {
    do_reconnect(app, &state, "reconnect").await
}

#[tauri::command]
async fn cancel(app: AppHandle, state: State<'_, AppContext>) -> Result<(), String> {
    let controller = state.controller.clone();
    // Interrupt a connecting worker by unloading the one-shot launchd job:
    // launchd terminates it, which runs the worker's own teardown instead of
    // a blind kill.
    if let Err(error) = vpn::run_admin("cancel", &[]).await {
        log::warn!(
            "cancel failed: {}",
            vpn::admin_error_hint(&vpn::setup_script_path(&app), error)
        );
    }
    controller.connect_failed("Connection canceled".to_string());
    Ok(())
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

// -- gateway selection -----------------------------------------------------------

/// Gateway precedence for a connect: the panel's explicit pick (forwarded
/// by the tray menu as `selected_gateway`) wins over the preferred
/// gateway, and None + None defers to gpclient's own auto-gateway order.
fn resolve_connect_gateway(explicit: Option<String>, preferred: Option<String>) -> Option<String> {
    explicit.or(preferred)
}

/// What a panel-equivalent Connect would use right now: the selected portal
/// (falling back to the first), the cookie-reuse setting, and the explicit
/// gateway pick. The tray menu and `do_reconnect` share it — the reconnect
/// chain must connect through exactly what the panel would.
fn connect_target_from_config(cfg: &Config) -> (String, bool, Option<String>) {
    let portal = cfg
        .selected
        .clone()
        .or_else(|| cfg.portals.first().cloned())
        .unwrap_or_default();
    (portal, cfg.reuse_cookie, cfg.selected_gateway.clone())
}

/// Keep the controller's notion of what a fresh connect would target, so
/// `reconnect_armed` (vpn::apply_reconnect_armed) stays truthful without the
/// controller ever reading the config: every command that changes the
/// resolution (the gateway picker, the preferred star, the list itself)
/// re-notes after its save, and setup seeds once at startup. `cfg` is the
/// post-update config, lock-free by contract.
fn sync_reconnect_target(controller: &VpnController, cfg: &Config) {
    let target =
        resolve_connect_gateway(cfg.selected_gateway.clone(), cfg.preferred_gateway.clone());
    controller.note_reconnect_target(target);
}

fn broadcast_config(app: &AppHandle, cfg: &Config) {
    // Emit outside the config lock (lock-order doc on `AppContext`).
    let _ = app.emit("config-updated", cfg);
}

#[tauri::command]
async fn select_gateway(
    app: AppHandle,
    state: State<'_, AppContext>,
    gateway: Option<String>,
) -> Result<(), String> {
    let app_for_save = app.clone();
    let (_, cfg) = tokio::task::spawn_blocking(move || {
        app_for_save.state::<AppContext>().update_config(|cfg| {
            match gateway.as_deref() {
                // null = "Auto": back to auto-gateway (preferred still applies).
                None => cfg.selected_gateway = None,
                Some(name) => {
                    if !valid_name(name) {
                        return Err(
                            "Gateway may only contain letters, digits, dots, dashes and underscores"
                                .to_string(),
                        );
                    }
                    if !cfg.gateways.iter().any(|g| g == name) {
                        return Err("Unknown gateway".to_string());
                    }
                    cfg.selected_gateway = Some(name.to_string());
                }
            }
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    // Guard dropped before emitting (lock-order doc on `AppContext`).
    broadcast_config(&app, &cfg);
    // The picker changed the resolution: re-note the target so the armed
    // flag (vpn-state → panel and tray) reflects the switch while Connected.
    sync_reconnect_target(&state.controller, &cfg);
    Ok(())
}

#[tauri::command]
async fn set_preferred_gateway(
    app: AppHandle,
    state: State<'_, AppContext>,
    gateway: Option<String>,
) -> Result<(), String> {
    let app_for_save = app.clone();
    let (_, cfg) = tokio::task::spawn_blocking(move || {
        app_for_save.state::<AppContext>().update_config(|cfg| {
            match gateway.as_deref() {
                None => cfg.preferred_gateway = None,
                Some(name) => {
                    if !valid_name(name) {
                        return Err("Invalid gateway name".to_string());
                    }
                    if !cfg.gateways.iter().any(|g| g == name) {
                        return Err("Unknown gateway".to_string());
                    }
                    cfg.preferred_gateway = Some(name.to_string());
                }
            }
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    broadcast_config(&app, &cfg);
    // The star changes what "Automatic" resolves to: re-note the target.
    sync_reconnect_target(&state.controller, &cfg);
    Ok(())
}

#[tauri::command]
async fn remove_gateway(
    app: AppHandle,
    state: State<'_, AppContext>,
    name: String,
) -> Result<(), String> {
    let app_for_save = app.clone();
    let (_, cfg) = tokio::task::spawn_blocking(move || {
        app_for_save.state::<AppContext>().update_config(|cfg| {
            cfg.gateways.retain(|g| g != &name);
            if cfg.preferred_gateway.as_deref() == Some(name.as_str()) {
                cfg.preferred_gateway = None;
            }
            if cfg.selected_gateway.as_deref() == Some(name.as_str()) {
                cfg.selected_gateway = None;
            }
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    broadcast_config(&app, &cfg);
    // A removal can zero the preferred/selected picks: re-note the target.
    sync_reconnect_target(&state.controller, &cfg);
    Ok(())
}

/// Manual gateway add. On this deployment the portal is password-auth (its
/// getconfig wants the real password, which GP Bar never handles by design),
/// so the full list cannot be fetched pre-connect: the user reads it once
/// from gpclient's interactive prompt and enters the names here. Repeated
/// unauthenticated getconfig attempts risk portal-side session
/// invalidations during connects — no gateway-list fetching, ever.
/// Gateways are harvested from connect logs and added manually.
#[tauri::command]
async fn add_gateway(
    app: AppHandle,
    state: State<'_, AppContext>,
    name: String,
) -> Result<(), String> {
    let name = name.trim().to_string();
    if !valid_name(&name) {
        return Err(
            "Gateway may only contain letters, digits, dots, dashes and underscores".to_string(),
        );
    }
    let app_for_save = app.clone();
    let (_, cfg) = tokio::task::spawn_blocking(move || {
        app_for_save.state::<AppContext>().update_config(|cfg| {
            if !cfg.gateways.iter().any(|g| g == &name) {
                cfg.gateways.push(name.clone());
            }
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    // Guard dropped before emitting (lock-order doc on `AppContext`).
    broadcast_config(&app, &cfg);
    // Uniform with the other gateway commands: re-note the target (an add
    // cannot change the resolution, but one rule for one family of paths).
    sync_reconnect_target(&state.controller, &cfg);
    Ok(())
}

// -- network repair ---------------------------------------------------------------

#[tauri::command]
async fn get_network_health(state: State<'_, AppContext>) -> Result<vpn::NetworkHealth, String> {
    // While the VPN is up (or transitioning) its DNS settings are legitimate.
    let phase = state.controller.snapshot().phase;
    if matches!(
        phase,
        Phase::Connecting | Phase::Connected | Phase::Disconnecting
    ) {
        return Ok(vpn::NetworkHealth {
            stale: false,
            service: None,
            servers: Vec::new(),
        });
    }
    let (service, servers) = vpn::active_service_dns().await;
    // The banner is advisory: "stale" = manual DNS that is NOT the recorded
    // pre-VPN state (the wrapper's snapshot — see scripts/gp-admin). A
    // snapshot-equal DNS is the user's own configuration, not a VPN leftover;
    // `gp-admin repair` makes the real decision and refuses to guess.
    let stale = !servers.is_empty()
        && !service
            .as_deref()
            .is_some_and(|svc| vpn::dns_matches_pre_vpn_snapshot(svc, &servers));
    Ok(vpn::NetworkHealth {
        stale,
        service,
        servers,
    })
}

/// The privileged repair's failure IS the typed admin failure (the setup
/// hint is folded in server-side): the frontend receives `{kind, detail}`
/// and maps it back to the Display texts.
#[tauri::command]
async fn repair_network(app: AppHandle) -> Result<String, vpn::AdminError> {
    // Refuses loudly while the VPN is up — see `gp-admin repair`.
    match vpn::run_admin("repair", &[]).await {
        Ok(message) => Ok(message),
        Err(error) => Err(vpn::admin_error_hint(&vpn::setup_script_path(&app), error)),
    }
}

// -- shared action logic --------------------------------------------------------

async fn do_connect(
    app: AppHandle,
    state: &State<'_, AppContext>,
    portal: &str,
    reuse_cookie: bool,
    gateway: Option<String>,
) -> Result<(), ActionError> {
    let controller = state.controller.clone();

    // Gateway resolution: an explicit pick from the panel or the tray menu
    // wins, then the preferred gateway, then gpclient's own auto-gateway
    // order (resolve_connect_gateway). The wrapper re-validates whatever
    // lands in the args.
    let resolved_gateway = resolve_connect_gateway(
        gateway,
        state.config.lock().unwrap().preferred_gateway.clone(),
    );
    if let Some(name) = &resolved_gateway {
        if !valid_name(name) {
            return Err(ActionError::Message("Invalid gateway name".to_string()));
        }
    }
    let gateway_arg = resolved_gateway.clone().unwrap_or_else(|| "-".to_string());

    // An explicit gateway pick is meaningless with a cached cookie: while
    // the cookie is valid gpclient reconnects to `last_gateway` and ignores
    // `--gateway` entirely. Drop the reuse in that case — the connect then
    // goes through the portal flow, which honors the pick. (Auto mode is
    // the exception: auto-gateway is what the cookie path would do anyway.)
    let reuse_cookie = reuse_cookie && resolved_gateway.is_none();

    // The root-owned wrapper (scripts/gp-admin) generates and loads the
    // launchd job itself: no staged files, no credentials, no dialogs. It
    // returns fast; the status poller adopts the worker from the lock file
    // once the tunnel is established.
    let args = [
        portal,
        if reuse_cookie { "1" } else { "0" },
        gateway_arg.as_str(),
    ];
    // Gate and transition atomically: a second concurrent Connect (double
    // click, panel + tray menu) must be refused BEFORE it can launch another
    // `admin start` and unload-kill the first worker mid-SSO.
    controller
        .try_begin_connect(portal)
        .map_err(ActionError::Message)?;
    let _ = app.emit("vpn-log-cleared", ());

    // No focus bookkeeping around the admin call: the helper is NOPASSWD so
    // no dialog steals the panel's focus mid-operation; popover hide-on-blur
    // (see `on_window_event`) behaves normally during connects.
    let result = vpn::run_admin("start", &args).await;
    match result {
        Ok(_) => {
            // The start is confirmed: the launchd job now exists and belongs to
            // THIS attempt. Only now may the poller blame a job without a pid
            // on a dead worker (see `connecting_worker_started`).
            controller.mark_worker_started();
            Ok(())
        }
        Err(error) => {
            let error = vpn::admin_error_hint(&vpn::setup_script_path(&app), error);
            controller.connect_failed(error.to_string());
            Err(error.into())
        }
    }
}

async fn do_disconnect(app: AppHandle, state: &State<'_, AppContext>) -> Result<(), ActionError> {
    let controller = state.controller.clone();
    // Gate and transition atomically (see try_begin_disconnect): a concurrent
    // connect that lands between a `snapshot()` check and the transition must
    // not be torn down by a second disconnect.
    controller
        .try_begin_disconnect()
        .map_err(ActionError::Message)?;
    // No focus bookkeeping here either (see `do_connect`): the NOPASSWD
    // helper never raises a dialog, so hide-on-blur stays in effect.
    let result = vpn::run_admin("stop", &[]).await;
    if let Err(error) = result {
        log::warn!("disconnect failed: {error}");
        let error = vpn::admin_error_hint(&vpn::setup_script_path(&app), error);
        // Fail loud, never SIGKILL: the worker may ignore the signal and a
        // killed worker skips the vpnc-script teardown, leaving macOS DNS
        // settings pointing at VPN resolvers. The poller reports the
        // still-connected truth from the lock file.
        controller.disconnect_failed(format!("disconnect failed: {error}"));
    }
    Ok(())
}

/// Wait for the backend to observe its own disconnect: `gp-admin stop`
/// returns once the worker is gone, but the phase flips to Disconnected
/// via the status poller — a connect sent straight away is refused
/// ("cannot connect while Disconnecting"). Bounded: 15 s in 250 ms steps,
/// the same budget the frontend-chained implementation used.
async fn wait_out_disconnecting(controller: &VpnController, context: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if controller.snapshot().phase != Phase::Disconnecting {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            log::warn!("{context}: timed out waiting for Disconnecting");
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The armed gateway switch: tear the tunnel down, wait out the
/// Disconnecting observation, then connect through what a fresh connect
/// would target right now (`connect_target_from_config`; `do_connect`
/// itself resolves the fallback chain). The target is read BEFORE the
/// teardown: it is what `reconnect_armed` was computed against, and
/// re-reading after would chase a picker change made mid-operation.
/// `context` ("reconnect" / "tray reconnect") prefixes the log lines,
/// which are stable grep targets — keep the formats byte-identical.
async fn do_reconnect(
    app: AppHandle,
    state: &State<'_, AppContext>,
    context: &str,
) -> Result<(), ActionError> {
    let (portal, reuse_cookie, gateway) = {
        let cfg = state.config.lock().unwrap();
        connect_target_from_config(&cfg)
        // Guard dropped before the disconnect work.
    };
    if let Err(error) = do_disconnect(app.clone(), state).await {
        log::warn!("{context}: disconnect failed: {error}");
        return Err(error);
    }
    wait_out_disconnecting(&state.controller, context).await;
    if let Err(error) = do_connect(app.clone(), state, &portal, reuse_cookie, gateway).await {
        log::warn!("{context}: connect failed: {error}");
        return Err(error);
    }
    Ok(())
}

// -- panel window ---------------------------------------------------------------
//
// The toggle/positioning log lines below are the incident-forensics
// channel (debug level → the panel.log file target; see `log_targets`):
// wrong-position reports only reproduce in real GUI usage, so every
// anchor/monitor/set_position decision leaves its numbers behind.

fn toggle_panel(app: &AppHandle, anchor: Option<(f64, f64)>) {
    let Some(window) = app.get_webview_window("main") else {
        log::warn!("toggle: no 'main' window found");
        return;
    };
    let state = app.state::<AppContext>();

    // Native popover toggle: a click while the panel is open dismisses it.
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        *state.panel_hidden_at.lock().unwrap() = Some(Instant::now());
        log::info!("toggle: dismissed (panel was visible)");
        return;
    }

    // Race: the very click that opened the tray icon also made the panel lose
    // focus, and `on_window_event` already hid it a few milliseconds ago. If
    // that happened within this window, this is the SAME physical click — keep
    // the panel closed instead of reopening it behind the user's back.
    let just_hidden = state
        .panel_hidden_at
        .lock()
        .unwrap()
        // take(): one lock instead of the is_some-then-clear double acquire —
        // the check and the reset must be one atomic step anyway.
        .take()
        .is_some_and(|hidden| hidden.elapsed() < Duration::from_millis(250));
    if just_hidden {
        log::info!("toggle: ignored (same physical click that hid the panel)");
        return;
    }

    // Anchor the panel under the tray icon: the handler passes the click
    // position (or the icon rect when it agrees with the click). The monitor
    // holding the anchor is resolved BY HAND in one canonical space — logical
    // points, top-left origin — never through tauri's monitor_from_point.
    // macOS really runs TWO screen spaces here, flipped against each other
    // in y:
    //
    //   * AppKit (NSEvent::mouseLocation, NSScreen/NSWindow frames) is
    //     BOTTOM-LEFT origin, y up. The tray payload is born there and is
    //     flipped into top-left by tray-icon (and by tao's cursor helper)
    //     using the MAIN screen's height from CGDisplayPixelsHigh — which
    //     behaves as POINTS on this OS build (only the logical height fits
    //     both the single- and the dual-display payloads) — then scaled by
    //     the tray window's own factor. Pre-scale, the payload IS canonical.
    //   * Quartz display space (CGDisplayBounds — what tao's
    //     Monitor::position() passes through RAW; tauri's work_area just adds
    //     the menu-bar/dock insets to it) is TOP-LEFT origin, y down: the
    //     monitor table below is already canonical, no flip needed.
    //
    // Invariant: never flip tao's raw positions with the main screen's
    // height. The flip is correct only for the primary (its origin (0,0)
    // is invariant under it); for every other monitor it mirrors the
    // rectangle across the primary — a display above the laptop would be
    // recorded below, and anchors on the external menu bar would never
    // match. The monitor table below is built in the canonical space
    // directly instead.

    // Tray payload, or the cursor fallback recovered to pure logical points
    // (tao converts the cursor with the PRIMARY scale — its cross-monitor
    // quirk — so undo that before hit-testing).
    let (payload, logical) = match anchor {
        Some(payload) => (Some(payload), None),
        None => {
            let cursor = window.cursor_position().ok();
            log::debug!("toggle: no tray payload; cursor_position={cursor:?}");
            let primary_scale = app
                .primary_monitor()
                .ok()
                .flatten()
                .map(|m| m.scale_factor())
                .unwrap_or(1.0);
            (
                None,
                cursor.map(|c| (c.x / primary_scale, c.y / primary_scale + 18.0)),
            )
        }
    };

    // Every monitor in canonical logical points, top-left origin, with its
    // visibleFrame work area in the same space (tauri's work_area() is
    // bottom-left too, so its y is flipped here).
    struct Mon {
        name: String,
        scale: f64,
        /// Monitor bounds: (x, y_top_left, width, height).
        bounds: (f64, f64, f64, f64),
        /// visibleFrame: same shape as `bounds`.
        work: (f64, f64, f64, f64),
    }
    let mons: Vec<Mon> = app
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let scale = m.scale_factor();
            let pos = m.position();
            let size = m.size();
            let work = m.work_area();
            Mon {
                name: m.name().cloned().unwrap_or_default(),
                scale,
                bounds: (
                    pos.x as f64 / scale,
                    pos.y as f64 / scale,
                    size.width as f64 / scale,
                    size.height as f64 / scale,
                ),
                work: (
                    work.position.x as f64 / scale,
                    work.position.y as f64 / scale,
                    work.size.width as f64 / scale,
                    work.size.height as f64 / scale,
                ),
            }
        })
        .collect();
    for mon in &mons {
        log::debug!(
            "positioning: monitor {} scale={:.1} bounds={:?} work={:?}",
            mon.name,
            mon.scale,
            mon.bounds,
            mon.work
        );
    }

    // Hit-test the anchor. A payload point is scaled by the tray screen's
    // OWN factor, so try each monitor's scale and keep the first whose
    // logical bounds contain the result; on a real desk (no overlapping
    // mixed-DPI projections) the match is unambiguous.
    let hit = mons.iter().find_map(|mon| {
        let (lx, ly) = match (payload, logical) {
            (Some((ax, ay)), _) => (ax / mon.scale, ay / mon.scale),
            (_, Some((lx, ly))) => (lx, ly),
            (None, None) => return None,
        };
        let (bx, by, bw, bh) = mon.bounds;
        (lx >= bx && lx < bx + bw && ly >= by && ly < by + bh).then_some((mon, lx, ly))
    });

    match (hit, window.outer_size()) {
        (Some((mon, lx, ly)), Ok(size)) => {
            // Everything is in logical points and set_position(Logical) is
            // scale-independent on tao (to_logical is the identity for a
            // Logical position), so a mixed-DPI desk is plain geometry here.
            let (wx, wy, ww, wh) = mon.work;
            let w_scale = window.scale_factor().unwrap_or(mon.scale);
            let panel_w = size.width as f64 / w_scale;
            let panel_h = size.height as f64 / w_scale;
            // `clamp` panics when min exceeds max: a work area smaller than
            // the panel must degrade to an edge, not take the app down.
            let x = if ww < panel_w {
                wx
            } else {
                (lx - panel_w / 2.0).clamp(wx, wx + ww - panel_w)
            };
            // Right under the tray icon, but never above the work area's top
            // edge (the menu bar) and never past its bottom.
            let y = if wh < panel_h {
                ly.max(wy)
            } else {
                ly.max(wy).min(wy + wh - panel_h)
            };
            log::debug!(
                "positioning: anchor logical=({lx:.1},{ly:.1}) monitor={} scale={:.1} \
                 work=({wx:.1},{wy:.1},{ww:.1},{wh:.1}) panel={panel_w:.1}x{panel_h:.1} \
                 -> set_position logical=({x:.1},{y:.1})",
                mon.name,
                mon.scale
            );
            if let Err(error) = window.set_position(LogicalPosition::new(x, y)) {
                log::warn!("set_position failed: {error}");
            }
        }
        (None, Ok(size)) => {
            // Never seen on a real desk; degrade to the primary screen's
            // top-right corner instead of leaving the panel at its
            // creation-default mid-screen position.
            log::warn!("positioning: NO monitor matched the anchor — primary top-right fallback");
            if let Some(primary) = app.primary_monitor().ok().flatten() {
                let scale = primary.scale_factor();
                let work = primary.work_area();
                let wx = work.position.x as f64 / scale;
                let wy = work.position.y as f64 / scale;
                let w_scale = window.scale_factor().unwrap_or(scale);
                let panel_w = size.width as f64 / w_scale;
                let (x, y) = (wx + work.size.width as f64 / scale - panel_w, wy);
                if let Err(error) = window.set_position(LogicalPosition::new(x, y)) {
                    log::warn!("set_position failed: {error}");
                }
            }
        }
        (_, Err(error)) => log::warn!("positioning: outer_size error: {error}"),
    }

    let _ = window.show();
    let _ = window.set_focus();
    // The ACTUAL placement after show(): compare against the intended
    // logical position above — a divergence here means tao/AppKit moved it.
    let actual = window
        .outer_position()
        .map(|p| (p.x, p.y))
        .map_err(|e| e.to_string());
    log::debug!(
        "shown: visible={} actual outer physical={actual:?}",
        window.is_visible().unwrap_or(false)
    );
}

/// The one subscriber of the controller's watch channel: every state
/// change goes to the tray first, then to the frontend — the same
/// relative order the old direct emit produced (the Rust tray listener
/// ran synchronously before the JS listener could see the event). The
/// frontend renders absolutely (from the state, never from a diff), so
/// the extra hop only relaxes vpn-state's ordering against unrelated
/// events (vpn-log-cleared, a command's rejection), which nothing
/// depends on; watch coalescing may likewise drop transient intermediates
/// between two reads.
fn spawn_state_publisher(app: AppHandle, controller: Arc<VpnController>) {
    // Subscribe BEFORE spawning: `subscribe` marks the current value seen
    // (the setup seed is not re-published), and no change between this
    // line and the task's first `changed().await` can be missed — watch
    // keeps the latest value for whoever reads next.
    let mut receiver = controller.subscribe();
    tauri::async_runtime::spawn(async move {
        while receiver.changed().await.is_ok() {
            let state = receiver.borrow_and_update().clone();
            sync_tray_with_state(&app, &state);
            let _ = app.emit("vpn-state", &state);
        }
    });
}

fn sync_tray_with_state(app: &AppHandle, state: &VpnState) {
    // Called from setup and the state publisher's task; tauri's TrayIcon
    // hops to the main thread itself (run_item_main_thread).
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let connected = state.phase == Phase::Connected;
        let tooltip = match state.phase {
            Phase::Connected => "GP Bar — Connected",
            Phase::Connecting => "GP Bar — Connecting…",
            Phase::Disconnecting => "GP Bar — Disconnecting…",
            Phase::Error => "GP Bar — Error",
            Phase::Disconnected => "GP Bar — Disconnected",
        };
        let image = if connected {
            CONNECTED_ICON.clone()
        } else {
            OPEN_ICON.clone()
        };
        let _ = tray.set_icon_with_as_template(Some(image), true);
        let _ = tray.set_tooltip(Some(tooltip));
    }
    sync_tray_menu(state);
}

/// Keep the right-click menu items consistent with the phase and the armed
/// flag: Connect where the panel would offer it; while Connected, either
/// the armed Reconnect (a gateway switch: `reconnect_armed` in VpnState,
/// computed backend-side — see `vpn::apply_reconnect_armed`) or the plain
/// Disconnect — never both; during the in-flight phases all stay dimmed
/// (do_connect/do_disconnect guard the transitions, this just prevents the
/// misleading click). The flag travels in every vpn-state event and is
/// re-derived on every relevant change, so this reads the state alone: no
/// config lock, no controller lock, nothing that could re-introduce the
/// cross-lock coupling the `update()` discipline avoids.
fn sync_tray_menu(state: &VpnState) {
    let Some(handles) = TRAY_MENU.get() else {
        return;
    };
    let armed = state.reconnect_armed;
    let _ = handles
        .connect
        .set_enabled(matches!(state.phase, Phase::Disconnected | Phase::Error));
    let _ = handles.reconnect.set_enabled(armed);
    let _ = handles
        .disconnect
        .set_enabled(state.phase == Phase::Connected && !armed);
}

/// Pop up the right-click tray menu. `Menu::popup` WITHOUT a position takes
/// muda's mouse-location branch: the menu appears exactly under the cursor
/// (the icon just right-clicked), independent of the hidden panel window's
/// geometry — the native status-item-menu UX without attaching anything to
/// the tray (see the tray comment in `main`).
fn show_tray_menu(app: &AppHandle) {
    let Some(handles) = TRAY_MENU.get() else {
        return;
    };
    let Some(panel) = app.get_webview_window("main") else {
        log::warn!("tray menu: main window not found — menu not shown");
        return;
    };
    // ContextMenu::popup takes the hosting Window: unwrap the webview window
    // down to it (AsRef<Webview> → Webview::window()).
    match handles.menu.popup(panel.as_ref().window()) {
        Ok(()) => log::info!("tray menu: popup at mouse location"),
        Err(error) => log::warn!("tray menu: popup failed: {error}"),
    }
}

// -- entry ------------------------------------------------------------------------

// -- logging ---------------------------------------------------------------------
//
// tauri-plugin-log owns the global `log` logger with two targets:
//
//   * stderr — terminal launches; kept at info and above so the console
//     view stays an operational one-liner stream, not the forensic dump;
//   * ~/Library/Logs/gp-bar/panel.log — the file that closed two real
//     incidents without a terminal (see RECAP.md §13): it keeps the
//     debug-level positioning/tray/toggle lines, rotated at 512 KiB
//     with 2 archives kept (~1.5 MiB worst case on disk).
//
// `level_for("gp_bar", Debug)` raises ONLY this crate's modules to debug
// while dependencies stay at the default info level; the message texts are
// a stable grep contract (analysis scripts and RECAP quotes match on
// them) — keep them byte-identical when touching log call sites.
fn log_targets() -> Vec<tauri_plugin_log::Target> {
    let mut targets = vec![
        tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stderr)
            .filter(|metadata| metadata.level() <= log::Level::Info),
    ];
    // Without a home dir (vanishingly rare) the file target is skipped;
    // stderr still carries everything at info and above.
    if let Some(home) = dirs::home_dir() {
        targets.push(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::Folder {
                path: home.join("Library/Logs/gp-bar"),
                file_name: Some("panel".to_string()),
            },
        ));
    }
    targets
}

fn main() {
    let controller = VpnController::new();
    let publisher_controller = controller.clone();
    let poller_controller = controller.clone();
    let tailer_controller = controller.clone();

    // Which load branch happened decides whether saves may write (see
    // `AppContext::persist`): Fresh/FromDisk persist; Unreadable serves
    // the defaults for the session and gates every save to a no-op.
    let (config, config_load, config_read_error) = match Config::load() {
        config::Loaded::Fresh(config) => (config, "fresh", None),
        config::Loaded::FromDisk(config) => (config, "from disk", None),
        config::Loaded::Unreadable(config, error) => {
            (config, "unreadable", Some(error.to_string()))
        }
    };
    // Auto: liquid glass on macOS 26+ unless the user forces the CSS
    // fallback (see Config::liquid_glass and glass::resolve_effect).
    let panel_effect = glass::resolve_effect(config.liquid_glass);

    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets(log_targets())
                .level(log::LevelFilter::Info)
                .level_for("gp_bar", log::LevelFilter::Debug)
                .max_file_size(512 * 1024)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(2))
                .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
                .build(),
        )
        .manage(AppContext {
            controller: controller.clone(),
            config: Mutex::new(config),
            panel_hidden_at: Mutex::new(None),
            panel_effect,
            config_load,
            config_read_error,
        })
        .on_window_event(|window, event| {
            // Popover behaviour: dismiss the panel when it loses focus. The
            // admin helper is NOPASSWD (no dialog can steal the focus), so
            // no exception is needed while connecting/disconnecting.
            if let WindowEvent::Focused(false) = event {
                let state = window.state::<AppContext>();
                let _ = window.hide();
                *state.panel_hidden_at.lock().unwrap() = Some(Instant::now());
            }
        })
        .setup(move |app| {
            // Menu bar-only app: no Dock icon, no Cmd+Tab entry.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Which config-load branch won at startup: `Config::load` runs
            // before the log plugin exists (its failure branches
            // eprintln), so it is recorded here, once, where the targets
            // are already attached.
            let ctx = app.state::<AppContext>();
            if let Some(error) = &ctx.config_read_error {
                log::warn!(
                    "config: unreadable at startup ({error}) — serving the defaults, saves are no-ops this session"
                );
            } else {
                log::debug!("config load: {}", ctx.config_load);
            }

            // Panel backdrop: native liquid glass where the OS (26+) and the
            // config allow it, CSS vibrancy otherwise (see `glass`). Applied
            // at window creation so the first `show` already looks right;
            // `apply_liquid_glass` must run on the main thread, and `setup`
            // does.
            let effect = app.state::<AppContext>().panel_effect;
            if effect == PanelEffect::LiquidGlass {
                if let Some(panel) = app.get_webview_window("main") {
                    if let Err(error) = glass::apply_liquid_glass(&panel) {
                        log::warn!("liquid glass failed: {error}");
                    }
                }
            }

            // Seed the idle state with the selected portal so the Connect
            // button is reachable in a fresh session (`state.portal` is what
            // enables it, see `note_selected_portal`), and the same snapshot
            // seeds the reconnect target: `reconnect_armed` is derived from
            // it on every state update, and the poller can adopt an
            // outside-started tunnel as Connected at any moment — before
            // any gateway command has re-noted it.
            let cfg = app.state::<AppContext>().config.lock().unwrap().clone();
            controller.note_selected_portal(cfg.selected.clone());
            sync_reconnect_target(&controller, &cfg);

            // Tray through tauri's own API, NOT a raw tray-icon one: tauri claims
            // the global TrayIconEvent handler (an OnceLock: first setter wins,
            // `build()` runs before `setup`) and drops events for trays it does
            // not manage, so a raw tray never sees a single click here. Through
            // tauri's builder the clicks flow through on_tray_icon_event instead.
            //
            // NO menu is ATTACHED on purpose: on this macOS beta the status-item
            // button opens a natively attached menu itself on left click and the
            // click never reaches our event handler (only enter/leave/move/right
            // events arrive — see the debug log). The right-click menu lives in
            // TRAY_MENU (built below) and is popped up BY HAND instead: nothing
            // for the button to open, so clicks flow to us and the panel keeps
            // taking over.
            let _tray = TrayIconBuilder::with_id(TRAY_ID)
                .icon(OPEN_ICON.clone())
                .icon_as_template(true)
                .tooltip("GP Bar")
                .on_tray_icon_event(|tray, event| {
                    // Right click opens the tray menu (TRAY_MENU, popped up by
                    // hand — never attached, see above). tray-icon delivers
                    // Click{Right, Down} from rightMouseDown whether or not a
                    // menu is attached, so this works on the beta as well.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Right,
                        button_state: MouseButtonState::Down,
                        ..
                    } = event
                    {
                        show_tray_menu(tray.app_handle());
                    }
                    // Left click toggles the panel. React on mouse-DOWN: it
                    // arrives within milliseconds of the focus-loss hide,
                    // keeping the race guard in toggle_panel reliable.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Down,
                        position,
                        rect,
                        ..
                    } = event
                    {
                        let (px, py) = (position.x, position.y);
                        let (rx, ry) = match rect.position {
                            tauri::Position::Physical(p) => (p.x as f64, p.y as f64),
                            tauri::Position::Logical(l) => (l.x, l.y),
                        };
                        let (rw, rh) = match rect.size {
                            tauri::Size::Physical(s) => (s.width as f64, s.height as f64),
                            tauri::Size::Logical(s) => (s.width, s.height),
                        };
                        // The click is the robust anchor (evidence: GP Connect
                        // anchors its popover on the cursor too): it always
                        // lives in the coordinates of the display the user is
                        // actually looking at. Trust the icon rect only when
                        // the click falls inside it — some macOS builds report
                        // the status-item rect in a different space, and a bad
                        // rect would drag the panel onto the wrong screen.
                        let rect_valid = (px >= rx - 2.0 && px <= rx + rw + 2.0)
                            && (py >= ry - 2.0 && py <= ry + rh + 2.0);
                        let anchor = if rect_valid {
                            // Centered on the icon, right under it.
                            (rx + rw / 2.0, ry + rh)
                        } else {
                            (px, py)
                        };
                        // Tray payload diagnostics: the tray-icon crate's
                        // coordinate space is the recurring suspect (see
                        // the positioning comments in toggle_panel).
                        log::debug!(
                            "tray click: position=({px:.1},{py:.1}) rect=({rx:.1},{ry:.1} \
                             {rw:.1}x{rh:.1}) valid={rect_valid} anchor=({:.1},{:.1})",
                            anchor.0,
                            anchor.1
                        );
                        toggle_panel(tray.app_handle(), Some(anchor));
                    }
                })
                .build(app)?;

            // The right-click tray menu: built once here, popped up by hand in
            // the tray handler (never attached — see above). Connect mirrors the
            // panel's action button exactly: the selected portal and the
            // cookie-reuse setting from config, plus the panel's explicit
            // gateway pick (`selected_gateway`) — do_connect itself resolves
            // the fallback chain (preferred, then gpclient auto-gateway).
            // Reconnect replaces a plain Disconnect while the panel's amber
            // button would show (a Connected gateway switch, the
            // backend-owned `reconnect_armed` in VpnState); enablement lives
            // in `sync_tray_menu`.
            let connect_item = MenuItem::with_id(app, "connect", "Connect", true, None::<&str>)?;
            let reconnect_item =
                MenuItem::with_id(app, "reconnect", "Reconnect", false, None::<&str>)?;
            let disconnect_item =
                MenuItem::with_id(app, "disconnect", "Disconnect", false, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit GP Bar", true, None::<&str>)?;
            let tray_menu = Menu::with_items(
                app,
                &[&connect_item, &reconnect_item, &disconnect_item, &quit_item],
            )?;
            if TRAY_MENU
                .set(TrayMenuHandles {
                    menu: tray_menu,
                    connect: connect_item,
                    reconnect: reconnect_item,
                    disconnect: disconnect_item,
                })
                .is_err()
            {
                log::warn!("tray menu: TRAY_MENU already set — built twice?");
            }
            // Match the items to the current phase before any state change
            // can fire. Safe lock: setup runs on the main thread and the
            // status poller has not started yet.
            sync_tray_with_state(app.handle(), &controller.snapshot());

            // The publisher is spawned AFTER the seeding above (its
            // `subscribe` marks the current value seen, so the seed is not
            // re-published) and BEFORE the poller (no transition can then
            // race past a missing subscriber).
            spawn_state_publisher(app.handle().clone(), publisher_controller);
            vpn::spawn_status_poller(poller_controller);
            vpn::spawn_log_tailer(app.handle().clone(), tailer_controller);

            // Defense-in-depth for the menu enablement: the armed flag now
            // travels in the published state (noted on every gateway save,
            // see `sync_reconnect_target`), so the state publisher is the
            // primary sync path — this one re-syncs the menu on every
            // config-updated in case a future config path ever misses the
            // re-note. Safe snapshot: the config lock is always dropped
            // before that event is emitted (see `broadcast_config` and
            // vpn::learn_gateway_host/harvest_gateway).
            let menu_handle = app.handle().clone();
            app.listen_any("config-updated", move |_| {
                sync_tray_menu(&menu_handle.state::<AppContext>().controller.snapshot());
            });

            Ok(())
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "connect" => {
                // Same contract as the panel's Connect button: selected portal,
                // configured cookie reuse, and the panel's explicit gateway
                // pick (`selected_gateway`) — None only means Automatic when
                // the user never picked one.
                let state = app.state::<AppContext>();
                let (portal, reuse_cookie, gateway) = {
                    let cfg = state.config.lock().unwrap();
                    connect_target_from_config(&cfg)
                    // Guard dropped before the async work.
                };
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppContext>();
                    if let Err(error) =
                        do_connect(app.clone(), &state, &portal, reuse_cookie, gateway).await
                    {
                        log::warn!("tray menu connect failed: {error}");
                    }
                });
            }
            "reconnect" => {
                // The armed gateway switch: the same chain as the panel's
                // amber Reconnect button, now backend-owned — do_reconnect
                // reads the target from the config, tears the tunnel down,
                // waits out the Disconnecting observation and connects
                // through it. Its warnings carry the "tray reconnect"
                // prefix, byte-identical to the old inline chain.
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppContext>();
                    let _ = do_reconnect(app.clone(), &state, "tray reconnect").await;
                });
            }
            "disconnect" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppContext>();
                    if let Err(error) = do_disconnect(app.clone(), &state).await {
                        log::warn!("tray menu disconnect failed: {error}");
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_panel_effect,
            get_config,
            get_config_read_error,
            get_versions,
            add_portal,
            remove_portal,
            select_portal,
            set_reuse_cookie,
            select_gateway,
            set_preferred_gateway,
            remove_gateway,
            add_gateway,
            get_network_health,
            repair_network,
            connect,
            disconnect,
            reconnect,
            cancel,
            quit
        ])
        .run(tauri::generate_context!())
        .expect("error while running gp-bar");
}

#[cfg(test)]
mod tests {
    use super::resolve_connect_gateway;
    // `valid_name` (the wrapper charset) is tested in config::tests.
    // The armed-condition test moved to vpn.rs with the condition itself
    // (`reconnect_arms_only_on_a_real_gateway_switch`); the resolution it
    // feeds on stays here, tested below.

    /// Gateway precedence: the explicit pick wins over the preferred
    /// gateway, and None + None defers to gpclient's auto-gateway order.
    #[test]
    fn explicit_gateway_picks_win_over_preferred() {
        assert_eq!(
            resolve_connect_gateway(Some("gw-1".into()), Some("gw-2".into())),
            Some("gw-1".to_string())
        );
        assert_eq!(
            resolve_connect_gateway(None, Some("gw-2".into())),
            Some("gw-2".to_string())
        );
        assert_eq!(resolve_connect_gateway(None, None), None);
        // Automatic ("-" in the wrapper args) is the None case, not a name.
        assert_eq!(
            resolve_connect_gateway(Some("-".into()), None),
            Some("-".to_string())
        );
    }
}

//! GP Bar — a menu bar client for the GlobalProtect VPN.
//!
//! It orchestrates the free, open-source `gpclient` CLI (GPL-3.0, from
//! https://github.com/yuezk/GlobalProtect-openconnect) and deliberately ships
//! none of the proprietary GUI licensing machinery: connections last as long
//! as the portal allows, with no client-side time limit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod icons;
mod vpn;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Listener, LogicalPosition, Manager, State, WindowEvent,
};

use config::Config;
use vpn::{Phase, VpnController, VpnState};

const TRAY_ID: &str = "gp-bar";

pub(crate) struct AppContext {
    controller: Arc<VpnController>,
    pub(crate) config: Mutex<Config>,
    /// When the panel was last hidden, to tell "the click that dismissed it"
    /// apart from a fresh open request (see `toggle_panel`).
    panel_hidden_at: Mutex<Option<Instant>>,
    /// While an admin password dialog is up it steals the panel's focus; keep
    /// the panel on screen so the user can watch the Connecting/Disconnecting
    /// state (see `pin_panel`).
    panel_pinned: AtomicBool,
}

// -- commands -----------------------------------------------------------------

#[tauri::command]
fn get_state(state: State<'_, AppContext>) -> VpnState {
    state.controller.snapshot()
}

#[tauri::command]
fn get_config(state: State<'_, AppContext>) -> Config {
    state.config.lock().unwrap().clone()
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
fn add_portal(app: AppHandle, state: State<'_, AppContext>, portal: String) -> Result<(), String> {
    let portal = portal.trim().to_string();
    if portal.is_empty() {
        return Err("Portal is empty".to_string());
    }
    if !portal
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(
            "Portal may only contain letters, digits, dots, dashes and underscores".to_string(),
        );
    }
    let selected = {
        let mut cfg = state.config.lock().unwrap();
        if !cfg.portals.contains(&portal) {
            cfg.portals.push(portal.clone());
        }
        if cfg.selected.is_none() {
            cfg.selected = Some(portal);
        }
        cfg.save()?;
        cfg.selected.clone()
    };
    state.controller.note_selected_portal(&app, selected);
    Ok(())
}

#[tauri::command]
fn remove_portal(
    app: AppHandle,
    state: State<'_, AppContext>,
    portal: String,
) -> Result<(), String> {
    let selected = {
        let mut cfg = state.config.lock().unwrap();
        cfg.portals.retain(|p| p != &portal);
        if cfg.selected.as_deref() == Some(portal.as_str()) {
            cfg.selected = cfg.portals.first().cloned();
        }
        cfg.save()?;
        cfg.selected.clone()
    };
    state.controller.note_selected_portal(&app, selected);
    Ok(())
}

#[tauri::command]
fn select_portal(
    app: AppHandle,
    state: State<'_, AppContext>,
    portal: String,
) -> Result<(), String> {
    let selected = {
        let mut cfg = state.config.lock().unwrap();
        if cfg.portals.contains(&portal) {
            cfg.selected = Some(portal);
        }
        cfg.save()?;
        cfg.selected.clone()
    };
    state.controller.note_selected_portal(&app, selected);
    Ok(())
}

#[tauri::command]
fn set_reuse_cookie(state: State<'_, AppContext>, value: bool) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    cfg.reuse_cookie = value;
    cfg.save()
}

#[tauri::command]
async fn connect(
    app: AppHandle,
    state: State<'_, AppContext>,
    portal: String,
    reuse_cookie: bool,
    gateway: Option<String>,
) -> Result<(), String> {
    do_connect(app, &state, &portal, reuse_cookie, gateway).await
}

#[tauri::command]
async fn disconnect(app: AppHandle, state: State<'_, AppContext>) -> Result<(), String> {
    do_disconnect(app, &state).await
}

#[tauri::command]
async fn cancel(app: AppHandle, state: State<'_, AppContext>) -> Result<(), String> {
    let controller = state.controller.clone();
    // Interrupt a connecting worker by unloading the one-shot launchd job:
    // launchd terminates it, which runs the worker's own teardown instead of
    // a blind kill. No admin dialog anymore: the NOPASSWD helper never asks.
    pin_panel(&app, true);
    if let Err(error) = vpn::run_admin("cancel", &[]).await {
        eprintln!(
            "gp-bar: cancel failed: {}",
            vpn::admin_error_hint(&vpn::setup_script_path(&app), &error)
        );
    }
    pin_panel(&app, false);
    controller.connect_failed(&app, "Connection canceled".to_string());
    Ok(())
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

// -- gateway selection -----------------------------------------------------------

/// Same charset the wrapper enforces (the name lands in the job plist).
fn valid_gateway_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

fn broadcast_config(app: &AppHandle, cfg: &Config) {
    // See vpn::update: emit outside the config lock.
    let _ = app.emit("config-updated", cfg);
}

#[tauri::command]
fn select_gateway(
    app: AppHandle,
    state: State<'_, AppContext>,
    gateway: Option<String>,
) -> Result<(), String> {
    let cfg = {
        let mut cfg = state.config.lock().unwrap();
        match gateway.as_deref() {
            // null = "Auto": back to auto-gateway (preferred still applies).
            None => cfg.selected_gateway = None,
            Some(name) => {
                if !valid_gateway_name(name) {
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
        cfg.save()?;
        cfg.clone()
        // Guard dropped before emitting (deadlock discipline, see vpn::update).
    };
    broadcast_config(&app, &cfg);
    Ok(())
}

#[tauri::command]
fn set_preferred_gateway(
    app: AppHandle,
    state: State<'_, AppContext>,
    gateway: Option<String>,
) -> Result<(), String> {
    let cfg = {
        let mut cfg = state.config.lock().unwrap();
        match gateway.as_deref() {
            None => cfg.preferred_gateway = None,
            Some(name) => {
                if !valid_gateway_name(name) {
                    return Err("Invalid gateway name".to_string());
                }
                if !cfg.gateways.iter().any(|g| g == name) {
                    return Err("Unknown gateway".to_string());
                }
                cfg.preferred_gateway = Some(name.to_string());
            }
        }
        cfg.save()?;
        cfg.clone()
    };
    broadcast_config(&app, &cfg);
    Ok(())
}

#[tauri::command]
fn remove_gateway(
    app: AppHandle,
    state: State<'_, AppContext>,
    name: String,
) -> Result<(), String> {
    let cfg = {
        let mut cfg = state.config.lock().unwrap();
        cfg.gateways.retain(|g| g != &name);
        if cfg.preferred_gateway.as_deref() == Some(name.as_str()) {
            cfg.preferred_gateway = None;
        }
        if cfg.selected_gateway.as_deref() == Some(name.as_str()) {
            cfg.selected_gateway = None;
        }
        cfg.save()?;
        cfg.clone()
    };
    broadcast_config(&app, &cfg);
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
    Ok(vpn::NetworkHealth {
        stale: !servers.is_empty(),
        service,
        servers,
    })
}

#[tauri::command]
async fn repair_network(app: AppHandle) -> Result<String, String> {
    // Refuses loudly while the VPN is up — see `gp-admin repair`.
    match vpn::run_admin("repair", &[]).await {
        Ok(message) => Ok(message),
        Err(error) => Err(vpn::admin_error_hint(&vpn::setup_script_path(&app), &error)),
    }
}

// -- shared action logic --------------------------------------------------------

async fn do_connect(
    app: AppHandle,
    state: &State<'_, AppContext>,
    portal: &str,
    reuse_cookie: bool,
    gateway: Option<String>,
) -> Result<(), String> {
    let controller = state.controller.clone();

    match controller.snapshot().phase {
        Phase::Connected | Phase::Connecting | Phase::Disconnecting => {
            return Err(format!(
                "cannot connect while {:?}",
                controller.snapshot().phase
            ));
        }
        Phase::Disconnected | Phase::Error => {}
    }

    std::fs::create_dir_all(Config::log_dir())
        .map_err(|error| format!("cannot create the log directory: {error}"))?;

    // Gateway resolution: an explicit pick from the panel wins, then the
    // preferred gateway, then gpclient's own auto-gateway order. The
    // wrapper re-validates whatever lands in the args.
    let resolved_gateway = {
        let cfg = state.config.lock().unwrap();
        gateway.or_else(|| cfg.preferred_gateway.clone())
    };
    if let Some(name) = &resolved_gateway {
        if !valid_gateway_name(name) {
            return Err("Invalid gateway name".to_string());
        }
    }
    let gateway_arg = resolved_gateway.clone().unwrap_or_else(|| "-".to_string());

    // The root-owned wrapper (scripts/gp-admin) generates and loads the
    // launchd job itself: no staged files, no credentials, no dialogs. It
    // returns fast; the status poller adopts the worker from the lock file
    // once the tunnel is established.
    let args = [
        portal,
        if reuse_cookie { "1" } else { "0" },
        gateway_arg.as_str(),
    ];
    controller.begin_connect(&app, portal.to_string());
    let _ = app.emit("vpn-log-cleared", ());

    // Keep the panel open while the operation runs.
    pin_panel(&app, true);
    let result = vpn::run_admin("start", &args).await;
    pin_panel(&app, false);
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            let error = vpn::admin_error_hint(&vpn::setup_script_path(&app), &error);
            controller.connect_failed(&app, error.clone());
            Err(error)
        }
    }
}

async fn do_disconnect(app: AppHandle, state: &State<'_, AppContext>) -> Result<(), String> {
    let controller = state.controller.clone();
    match controller.snapshot().phase {
        Phase::Connected => {}
        Phase::Disconnecting => return Ok(()),
        _ => return Err("not connected".to_string()),
    }
    controller.begin_disconnect(&app);
    pin_panel(&app, true);
    let result = vpn::run_admin("stop", &[]).await;
    pin_panel(&app, false);
    if let Err(error) = result {
        eprintln!("gp-bar: disconnect failed: {error}");
        let error = vpn::admin_error_hint(&vpn::setup_script_path(&app), &error);
        // Fail loud, never SIGKILL: the worker may ignore the signal and a
        // killed worker skips the vpnc-script teardown, leaving macOS DNS
        // settings pointing at VPN resolvers. The poller reports the
        // still-connected truth from the lock file.
        controller.disconnect_failed(&app, format!("disconnect failed: {error}"));
    }
    Ok(())
}

// -- panel window ---------------------------------------------------------------

fn toggle_panel(app: &AppHandle, anchor: Option<(f64, f64)>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let state = app.state::<AppContext>();

    // Native popover toggle: a click while the panel is open dismisses it.
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        *state.panel_hidden_at.lock().unwrap() = Some(Instant::now());
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
        .is_some_and(|hidden| hidden.elapsed() < Duration::from_millis(250));
    *state.panel_hidden_at.lock().unwrap() = None;
    if just_hidden {
        return;
    }

    // Anchor the panel under the tray icon. The handler passes the click
    // position (or the icon rect when it agrees with the click), and macOS
    // physical coordinates are global (primary monitor at 0,0, others e.g.
    // negative to its left), so resolve the monitor that HOLDS the anchor —
    // never the primary — and clamp inside its work area.
    let anchor = anchor.or_else(|| {
        window
            .cursor_position()
            .ok()
            .map(|cursor| (cursor.x, cursor.y + 18.0))
    });

    if let (Some((ax, ay)), Ok(size)) = (anchor, window.outer_size()) {
        if let Ok(Some(monitor)) = app.monitor_from_point(ax, ay) {
            // The anchor is per-monitor PHYSICAL, so the monitor lookup must
            // happen in that space. Everything after runs in GLOBAL LOGICAL
            // points instead: tao's set_outer_position converts a Physical
            // position using the window's CURRENT scale factor, so on a
            // mixed-DPI desk (retina laptop + 1x external) a physical
            // position aimed at the other monitor lands off by the scale
            // ratio and the panel opens far from the tray icon. GP Connect
            // positions its panel with Position::Logical for this reason.
            let m_scale = monitor.scale_factor();
            let (ax, ay) = (ax / m_scale, ay / m_scale);
            // Clamp inside the work area (the visibleFrame: below the menu
            // bar, beside the Dock) of the monitor holding the anchor.
            // Evidence: GP Connect resolves "the monitor under the cursor"
            // and clamps with visibleFrame — clamping against the full
            // monitor bounds would let the panel slide under the menu bar.
            let work = monitor.work_area();
            let (wx, wy, ww, wh) = (
                work.position.x as f64 / m_scale,
                work.position.y as f64 / m_scale,
                work.size.width as f64 / m_scale,
                work.size.height as f64 / m_scale,
            );
            // The panel's logical size is scale-invariant: once everything
            // is in points, no DPI re-projection is needed.
            let scale = window.scale_factor().unwrap_or(1.0);
            let panel_w = size.width as f64 / scale;
            let panel_h = size.height as f64 / scale;
            // `clamp` panics when min exceeds max: a work area smaller than
            // the panel must degrade to an edge, not take the app down.
            let x = if ww < panel_w {
                wx
            } else {
                (ax - panel_w / 2.0).clamp(wx, wx + ww - panel_w)
            };
            // Right under the tray icon, but never above the work area's top
            // edge (the menu bar) and never past its bottom.
            let y = if wh < panel_h {
                ay.max(wy)
            } else {
                ay.max(wy).min(wy + wh - panel_h)
            };
            let _ = window.set_position(LogicalPosition::new(x, y));
        }
    }

    let _ = window.show();
    let _ = window.set_focus();
}

fn pin_panel(app: &AppHandle, pinned: bool) {
    app.state::<AppContext>()
        .panel_pinned
        .store(pinned, Ordering::Relaxed);
    // Hand focus back to the still-visible panel when the dialog is gone, so
    // the next click elsewhere dismisses it again as usual.
    if !pinned {
        if let Some(window) = app.get_webview_window("main") {
            if window.is_visible().unwrap_or(false) {
                let _ = window.set_focus();
            }
        }
    }
}

fn sync_tray_with_state(app: &AppHandle, phase: Phase) {
    // State events are emitted from the status poller's worker task, but
    // tauri's TrayIcon hops to the main thread itself (run_item_main_thread).
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let connected = phase == Phase::Connected;
        let tooltip = match phase {
            Phase::Connected => "GP Bar — Connected",
            Phase::Connecting => "GP Bar — Connecting…",
            Phase::Disconnecting => "GP Bar — Disconnecting…",
            Phase::Error => "GP Bar — Error",
            Phase::Disconnected => "GP Bar — Disconnected",
        };
        let image = icons::padlock_icon(connected);
        let _ = tray.set_icon_with_as_template(Some(image), true);
        let _ = tray.set_tooltip(Some(tooltip));
    }
}

// -- entry ------------------------------------------------------------------------

fn main() {
    let controller = VpnController::new();
    let poller_controller = controller.clone();
    let tailer_controller = controller.clone();

    tauri::Builder::default()
        .manage(AppContext {
            controller: controller.clone(),
            config: Mutex::new(Config::load()),
            panel_hidden_at: Mutex::new(None),
            panel_pinned: AtomicBool::new(false),
        })
        .on_window_event(|window, event| {
            // Popover behaviour: dismiss the panel when it loses focus — except
            // while `panel_pinned` is set, i.e. an admin password dialog is up and
            // the user is mid connect/disconnect watching the state updates.
            if let WindowEvent::Focused(false) = event {
                let state = window.state::<AppContext>();
                if !state.panel_pinned.load(Ordering::Relaxed) {
                    let _ = window.hide();
                    *state.panel_hidden_at.lock().unwrap() = Some(Instant::now());
                }
            }
        })
        .setup(move |app| {
            // Menu bar-only app: no Dock icon, no Cmd+Tab entry.
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Seed the idle state with the selected portal so the Connect button
            // is reachable in a fresh session. The removed native tray menu used
            // to be the implicit seeding path (it connected straight from
            // `config.selected`); without this a restart left the button dead.
            let selected = app
                .state::<AppContext>()
                .config
                .lock()
                .unwrap()
                .selected
                .clone();
            controller.note_selected_portal(app.handle(), selected);

            // Tray through tauri's own API, NOT a raw tray-icon one: tauri claims
            // the global TrayIconEvent handler (an OnceLock: first setter wins,
            // `build()` runs before `setup`) and drops events for trays it does
            // not manage, so a raw tray never sees a single click here. Through
            // tauri's builder the clicks flow through on_tray_icon_event instead.
            //
            // NO menu is attached on purpose: on this macOS beta the status-item
            // button opens a natively attached menu itself on left click and the
            // click never reaches our event handler (only enter/leave/move/right
            // events arrive — see the debug log). Without a menu there is nothing
            // for the button to open, clicks flow to us, and the panel takes over
            // (quit lives in the panel footer).
            let image = icons::padlock_icon(false);
            let _tray = TrayIconBuilder::with_id(TRAY_ID)
                .icon(image)
                .icon_as_template(true)
                .tooltip("GP Bar")
                .on_tray_icon_event(|tray, event| {
                    // Left click toggles the panel (right click does nothing: the
                    // dropdown menu is incompatible with this macOS beta, which opens
                    // a natively attached menu on left click itself). React on
                    // mouse-DOWN: it arrives within milliseconds of the focus-loss
                    // hide, keeping the race guard in toggle_panel reliable.
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
                        toggle_panel(tray.app_handle(), Some(anchor));
                    }
                })
                .build(app)?;

            vpn::spawn_status_poller(app.handle().clone(), poller_controller);
            vpn::spawn_log_tailer(app.handle().clone(), tailer_controller);

            // Reflect state transitions onto the tray icon and tooltip. Read the
            // phase from the event payload instead of calling `snapshot()`:
            // listeners run synchronously on the emitting thread, and re-locking
            // the controller here deadlocked the poller (see vpn::update).
            let icon_handle = app.handle().clone();
            app.listen_any("vpn-state", move |event| {
                if let Ok(state) = serde_json::from_str::<VpnState>(event.payload()) {
                    sync_tray_with_state(&icon_handle, state.phase);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_config,
            get_versions,
            add_portal,
            remove_portal,
            select_portal,
            set_reuse_cookie,
            select_gateway,
            set_preferred_gateway,
            remove_gateway,
            get_network_health,
            repair_network,
            connect,
            disconnect,
            cancel,
            quit
        ])
        .run(tauri::generate_context!())
        .expect("error while running gp-bar");
}

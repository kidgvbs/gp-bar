//! GP Bar — a menu bar client for the GlobalProtect VPN.
//!
//! It orchestrates the free, open-source `gpclient` CLI (GPL-3.0, from
//! https://github.com/yuezk/GlobalProtect-openconnect) and deliberately ships
//! none of the proprietary GUI licensing machinery: connections last as long
//! as the portal allows, with no client-side time limit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod glass;
mod icons;
mod vpn;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{
    menu::{ContextMenu, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Listener, LogicalPosition, Manager, State, WindowEvent, Wry,
};

use config::Config;
use glass::PanelEffect;
use vpn::{Phase, VpnController, VpnState};

const TRAY_ID: &str = "gp-bar";

/// Handles for the right-click tray menu. The menu is NEVER attached to the
/// tray (see the tray comment in `main`): on this macOS beta a natively
/// attached status-item menu makes the system swallow left clicks before the
/// app ever sees them, which used to kill the panel. It is popped up BY HAND
/// on right-click instead — muda's position-less context-menu path anchors at
/// `NSEvent::mouseLocation` (the icon the user just right-clicked) in screen
/// coordinates, so the hidden panel window's geometry never enters into it.
static TRAY_MENU: OnceLock<TrayMenuHandles> = OnceLock::new();

struct TrayMenuHandles {
    menu: Menu<Wry>,
    connect: MenuItem<Wry>,
    disconnect: MenuItem<Wry>,
}

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
    /// Which panel backdrop (native liquid glass vs CSS vibrancy) was
    /// resolved at startup; reported to the frontend via `get_panel_effect`.
    panel_effect: PanelEffect,
}

// -- commands -----------------------------------------------------------------

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
    if !valid_name(&portal) {
        return Err(
            "Portal may only contain letters, digits, dots, dashes and underscores (and must not start with a dash)"
                .to_string(),
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

/// Shared charset for every name that reaches the privileged wrapper
/// (portals and gateways): ASCII letters, digits, dots, dashes and
/// underscores. A LEADING DASH is rejected on purpose: gpclient's option
/// parser would read it as an option (argument injection, S4), and `-`
/// collides with the wrapper's "auto-gateway" sentinel argument.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
}

/// Same charset the wrapper enforces (the name lands in the job plist).
fn valid_gateway_name(name: &str) -> bool {
    valid_name(name)
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

/// Manual gateway add. On this deployment the portal is password-auth (its
/// getconfig wants the real password, which GP Bar never handles by design),
/// so the full list cannot be fetched pre-connect: the user reads it once
/// from gpclient's interactive prompt and enters the names here. (The
/// cookie-replication attempt of commit 8475510 was removed again in 587d3ea:
/// the repeated auth-failed requests plausibly contributed to portal-side
/// session invalidations during connects.)
#[tauri::command]
fn add_gateway(app: AppHandle, state: State<'_, AppContext>, name: String) -> Result<(), String> {
    let name = name.trim().to_string();
    if !valid_gateway_name(&name) {
        return Err(
            "Gateway may only contain letters, digits, dots, dashes and underscores".to_string(),
        );
    }
    let cfg = {
        let mut cfg = state.config.lock().unwrap();
        if !cfg.gateways.iter().any(|g| g == &name) {
            cfg.gateways.push(name.clone());
            cfg.save()?;
        }
        cfg.clone()
        // Guard dropped before emitting (deadlock discipline, see vpn::update).
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

/// Positioning diagnostics: stderr (terminal launches only) AND an append
/// log at ~/Library/Logs/gp-bar/panel.log (always). GUI launches show
/// nothing on stderr, and the wrong-position reports only reproduce in
/// real usage — the file is how the numbers get captured without asking
/// anyone to run from a terminal.
fn panel_log(msg: &str) {
    eprintln!("gp-bar: {msg}");
    if let Some(home) = dirs::home_dir() {
        let path = home.join("Library/Logs/gp-bar/panel.log");
        // create_dir_all: OpenOptions never creates the parent, and a missing
        // Logs/gp-bar would silently drop every line — the one failure mode
        // this instrumentation must not have.
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let stamp = chrono::Local::now().format("%H:%M:%S%.3f");
            let _ = writeln!(file, "{stamp} {msg}");
        }
    }
}

fn toggle_panel(app: &AppHandle, anchor: Option<(f64, f64)>) {
    let Some(window) = app.get_webview_window("main") else {
        panel_log("toggle: no 'main' window found");
        return;
    };
    let state = app.state::<AppContext>();

    // Native popover toggle: a click while the panel is open dismisses it.
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        *state.panel_hidden_at.lock().unwrap() = Some(Instant::now());
        panel_log("toggle: dismissed (panel was visible)");
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
        panel_log("toggle: ignored (same physical click that hid the panel)");
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
    // The mixed-DPI bug this replaced: an earlier table flipped tao's
    // positions with the main height, mirroring every NON-primary monitor
    // across the primary (a display above the laptop was recorded below),
    // so anchors on the external menu bar never matched. The single-monitor
    // case still worked because the primary's origin (0,0) is invariant
    // under that flip.

    // Tray payload, or the cursor fallback recovered to pure logical points
    // (tao converts the cursor with the PRIMARY scale — its cross-monitor
    // quirk — so undo that before hit-testing).
    let (payload, logical) = match anchor {
        Some(payload) => (Some(payload), None),
        None => {
            let cursor = window.cursor_position().ok();
            panel_log(&format!(
                "toggle: no tray payload; cursor_position={cursor:?}"
            ));
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
        panel_log(&format!(
            "positioning: monitor {} scale={:.1} bounds={:?} work={:?}",
            mon.name, mon.scale, mon.bounds, mon.work
        ));
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
            panel_log(&format!(
                "positioning: anchor logical=({lx:.1},{ly:.1}) monitor={} scale={:.1} \
                 work=({wx:.1},{wy:.1},{ww:.1},{wh:.1}) panel={panel_w:.1}x{panel_h:.1} \
                 -> set_position logical=({x:.1},{y:.1})",
                mon.name, mon.scale
            ));
            if let Err(error) = window.set_position(LogicalPosition::new(x, y)) {
                panel_log(&format!("set_position failed: {error}"));
            }
        }
        (None, Ok(size)) => {
            // Never seen on a real desk; degrade to the primary screen's
            // top-right corner instead of leaving the panel at its
            // creation-default mid-screen position.
            panel_log("positioning: NO monitor matched the anchor — primary top-right fallback");
            if let Some(primary) = app.primary_monitor().ok().flatten() {
                let scale = primary.scale_factor();
                let work = primary.work_area();
                let wx = work.position.x as f64 / scale;
                let wy = work.position.y as f64 / scale;
                let w_scale = window.scale_factor().unwrap_or(scale);
                let panel_w = size.width as f64 / w_scale;
                let (x, y) = (wx + work.size.width as f64 / scale - panel_w, wy);
                if let Err(error) = window.set_position(LogicalPosition::new(x, y)) {
                    panel_log(&format!("set_position failed: {error}"));
                }
            }
        }
        (_, Err(error)) => panel_log(&format!("positioning: outer_size error: {error}")),
    }

    let _ = window.show();
    let _ = window.set_focus();
    // The ACTUAL placement after show(): compare against the intended
    // logical position above — a divergence here means tao/AppKit moved it.
    let actual = window
        .outer_position()
        .map(|p| (p.x, p.y))
        .map_err(|e| e.to_string());
    panel_log(&format!(
        "shown: visible={} actual outer physical={actual:?}",
        window.is_visible().unwrap_or(false)
    ));
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
    // Keep the right-click menu consistent with the same phase. Connect where
    // the panel would offer it, Disconnect while connected; during the
    // in-flight phases both stay dimmed — do_connect/do_disconnect already
    // guard the transitions, this just prevents the misleading click.
    if let Some(handles) = TRAY_MENU.get() {
        let _ = handles
            .connect
            .set_enabled(matches!(phase, Phase::Disconnected | Phase::Error));
        let _ = handles.disconnect.set_enabled(phase == Phase::Connected);
    }
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
        panel_log("tray menu: main window not found — menu not shown");
        return;
    };
    // ContextMenu::popup takes the hosting Window: unwrap the webview window
    // down to it (AsRef<Webview> → Webview::window()).
    match handles.menu.popup(panel.as_ref().window()) {
        Ok(()) => panel_log("tray menu: popup at mouse location"),
        Err(error) => panel_log(&format!("tray menu: popup failed: {error}")),
    }
}

// -- entry ------------------------------------------------------------------------

fn main() {
    let controller = VpnController::new();
    let poller_controller = controller.clone();
    let tailer_controller = controller.clone();

    let config = Config::load();
    // Auto: liquid glass on macOS 26+ unless the user forces the CSS
    // fallback (see Config::liquid_glass and glass::resolve_effect).
    let panel_effect = glass::resolve_effect(config.liquid_glass);

    tauri::Builder::default()
        .manage(AppContext {
            controller: controller.clone(),
            config: Mutex::new(config),
            panel_hidden_at: Mutex::new(None),
            panel_pinned: AtomicBool::new(false),
            panel_effect,
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

            // Panel backdrop: native liquid glass where the OS (26+) and the
            // config allow it, CSS vibrancy otherwise (see `glass`). Applied
            // at window creation so the first `show` already looks right;
            // `apply_liquid_glass` must run on the main thread, and `setup`
            // does.
            let effect = app.state::<AppContext>().panel_effect;
            if effect == PanelEffect::LiquidGlass {
                if let Some(panel) = app.get_webview_window("main") {
                    if let Err(error) = glass::apply_liquid_glass(&panel) {
                        eprintln!("gp-bar: liquid glass failed: {error}");
                    }
                }
            }

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
            // NO menu is ATTACHED on purpose: on this macOS beta the status-item
            // button opens a natively attached menu itself on left click and the
            // click never reaches our event handler (only enter/leave/move/right
            // events arrive — see the debug log). The right-click menu lives in
            // TRAY_MENU (built below) and is popped up BY HAND instead: nothing
            // for the button to open, so clicks flow to us and the panel keeps
            // taking over.
            let image = icons::padlock_icon(false);
            let _tray = TrayIconBuilder::with_id(TRAY_ID)
                .icon(image)
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
                        // panel_log / toggle_panel for the rest of the chain).
                        panel_log(&format!(
                            "tray click: position=({px:.1},{py:.1}) rect=({rx:.1},{ry:.1} \
                             {rw:.1}x{rh:.1}) valid={rect_valid} anchor=({:.1},{:.1})",
                            anchor.0, anchor.1
                        ));
                        toggle_panel(tray.app_handle(), Some(anchor));
                    }
                })
                .build(app)?;

            // The right-click tray menu: built once here, popped up by hand in
            // the tray handler (never attached — see above). Connect mirrors the
            // panel's action button exactly: the selected portal from config,
            // the cookie-reuse setting, gateway None = Automatic (do_connect
            // itself resolves preferred over --auto-gateway).
            let connect_item = MenuItem::with_id(app, "connect", "Connect", true, None::<&str>)?;
            let disconnect_item =
                MenuItem::with_id(app, "disconnect", "Disconnect", false, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit GP Bar", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&connect_item, &disconnect_item, &quit_item])?;
            if TRAY_MENU
                .set(TrayMenuHandles {
                    menu: tray_menu,
                    connect: connect_item,
                    disconnect: disconnect_item,
                })
                .is_err()
            {
                panel_log("tray menu: TRAY_MENU already set — built twice?");
            }
            // Match the items to the current phase before any vpn-state event
            // fires. Safe lock: setup runs on the main thread and the status
            // poller has not started yet.
            sync_tray_with_state(app.handle(), controller.snapshot().phase);

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
        .on_menu_event(|app, event| match event.id().as_ref() {
            "connect" => {
                // Same contract as the panel's Connect button: selected portal,
                // configured cookie reuse, gateway None = Automatic.
                let state = app.state::<AppContext>();
                let (portal, reuse_cookie) = {
                    let cfg = state.config.lock().unwrap();
                    let portal = cfg
                        .selected
                        .clone()
                        .or_else(|| cfg.portals.first().cloned())
                        .unwrap_or_default();
                    (portal, cfg.reuse_cookie)
                };
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppContext>();
                    if let Err(error) =
                        do_connect(app.clone(), &state, &portal, reuse_cookie, None).await
                    {
                        panel_log(&format!("tray menu connect failed: {error}"));
                    }
                });
            }
            "disconnect" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppContext>();
                    if let Err(error) = do_disconnect(app.clone(), &state).await {
                        panel_log(&format!("tray menu disconnect failed: {error}"));
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
            cancel,
            quit
        ])
        .run(tauri::generate_context!())
        .expect("error while running gp-bar");
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn accepts_plain_hostnames() {
        for name in ["vpn.example.com", "vpn-1", "gw_2", "a", "10.0.0.1.x"] {
            assert!(valid_name(name), "{name:?} should be valid");
        }
    }

    #[test]
    fn rejects_leading_dash_option_injection_and_bad_charset() {
        // S4: a leading '-' would be read as an option by gpclient's
        // parser, and '-' collides with the wrapper's auto-gateway
        // sentinel.
        for name in [
            "",
            "-",
            "--ignore-tls-errors",
            "-auto",
            "space name",
            "vpn/example",
            "https://vpn.example.com",
            "café",
        ] {
            assert!(!valid_name(name), "{name:?} should be rejected");
        }
    }
}

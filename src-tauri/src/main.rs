//! GP Bar — a menu bar client for the GlobalProtect VPN.
//!
//! It orchestrates the free, open-source `gpclient` CLI (GPL-3.0, from
//! https://github.com/yuezk/GlobalProtect-openconnect) and deliberately ships
//! none of the proprietary GUI licensing machinery: connections last as long
//! as the portal allows, with no client-side time limit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod helper;
mod icons;
mod proto;
mod vpn;
mod xpc;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{
  tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
  AppHandle, Emitter, Listener, Manager, PhysicalPosition, State, WindowEvent,
};

use config::Config;
use vpn::{Phase, VpnController, VpnState};

const TRAY_ID: &str = "gp-bar";

struct AppContext {
  controller: Arc<VpnController>,
  config: Mutex<Config>,
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
  // Same charset rule the gp-helper daemon re-checks (defense in depth).
  proto::validate_portal(&portal)?;
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
) -> Result<(), String> {
  do_connect(app, &state, &portal, reuse_cookie).await
}

#[tauri::command]
async fn disconnect(app: AppHandle, state: State<'_, AppContext>) -> Result<(), String> {
  do_disconnect(app, &state).await
}

#[tauri::command]
async fn cancel(app: AppHandle, state: State<'_, AppContext>) -> Result<(), String> {
  let controller = state.controller.clone();
  // Interrupt a connecting worker through the root daemon: SIGTERM the
  // worker GP Bar's helper spawned, so it runs its own teardown. No admin
  // prompt: the helper is already registered.
  if let Err(error) = helper::cancel().await {
    eprintln!("gp-bar: cancel failed: {error}");
    controller.connect_failed(&app, format!("cancel failed: {error}"));
    return Ok(());
  }
  controller.connect_failed(&app, "Connection canceled".to_string());
  Ok(())
}

#[tauri::command]
fn get_helper_status() -> helper::HelperStatus {
  helper::status()
}

#[tauri::command]
fn quit(app: AppHandle) {
  app.exit(0);
}

// -- shared action logic --------------------------------------------------------

async fn do_connect(
  app: AppHandle,
  state: &State<'_, AppContext>,
  portal: &str,
  reuse_cookie: bool,
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

  // Same charset the daemon re-checks: no shell metacharacters reach argv.
  proto::validate_portal(portal)?;

  controller.begin_connect(&app, portal.to_string());
  let _ = app.emit("vpn-log-cleared", ());

  // Register the helper daemon on first use: the ONE place a system
  // authorization prompt can appear (afterwards `status` is Enabled and
  // everything runs promptless over XPC). Done AFTER begin_connect so a
  // failure lands in the Error state instead of vanishing silently.
  pin_panel(&app, true);
  let register = helper::ensure_ready().await;
  pin_panel(&app, false);
  if let Err(error) = register {
    controller.connect_failed(&app, error.clone());
    return Err(error);
  }

  // The root daemon truncates the connect log and spawns gpclient as its
  // own child (SUDO_UID/SUDO_USER of the console user in its env); the
  // status poller adopts the worker from the lock file once the tunnel is
  // up, and the log tailer streams its JSON records into the panel.
  let result = helper::start(portal, reuse_cookie).await;
  match result {
    Ok(_) => Ok(()),
    Err(error) => {
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
  // `gpclient disconnect` via the daemon, verified against the lock file
  // (10s): the daemon fails loud instead of leaving a stuck
  // "Disconnecting", and never SIGKILLs (a killed worker skips the
  // vpnc-script teardown that restores DNS settings and routes). The
  // poller reports the still-connected truth from the lock file.
  if let Err(error) = helper::stop().await {
    eprintln!("gp-bar: disconnect failed: {error}");
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

  // Anchor the panel under the tray icon: the event carries the icon's
  // global rect, and macOS physical coordinates are global (primary monitor
  // at 0,0, others e.g. negative to its left), so clamp against the bounds
  // of the monitor that HOLDS the icon — never against 0, which would drag
  // the panel back onto the primary screen.
  let anchor = anchor.or_else(|| {
    window
      .cursor_position()
      .ok()
      .map(|cursor| (cursor.x, cursor.y + 18.0))
  });

  if let (Some((ax, ay)), Ok(size)) = (anchor, window.outer_size()) {
    if let Ok(Some(monitor)) = app.monitor_from_point(ax, ay) {
      let origin = monitor.position();
      let msize = monitor.size();
      // `outer_size` is physical ON THE CURRENT MONITOR: re-project it onto
      // the anchor monitor so the panel lands correctly when it hops between
      // retina and 1x displays (720 vs 360 physical px for the same panel).
      let scale = window.scale_factor().unwrap_or(1.0);
      let panel_w = size.width as f64 / scale * monitor.scale_factor() as f64;
      let x = (ax - panel_w / 2.0).clamp(
        origin.x as f64,
        (origin.x + msize.width as i32) as f64 - panel_w,
      );
      // Right under the menu bar: the tray-rect anchor already sits at the
      // bottom of the menu bar, so this is as high as the panel can go.
      let y = ay.max(origin.y as f64 + 24.0 * monitor.scale_factor() as f64);
      let _ = window.set_position(PhysicalPosition::new(x as i32, y as i32));
    }
  }

  let _ = window.show();
  let _ = window.set_focus();
}

fn pin_panel(app: &AppHandle, pinned: bool) {
  app
    .state::<AppContext>()
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
            rect,
            ..
          } = event
          {
            let (rx, ry) = match rect.position {
              tauri::Position::Physical(p) => (p.x as f64, p.y as f64),
              tauri::Position::Logical(l) => (l.x, l.y),
            };
            let rh = match rect.size {
              tauri::Size::Physical(s) => s.height as f64,
              tauri::Size::Logical(s) => s.height,
            };
            // Global coordinates of the tray icon: anchor the panel right
            // below it (see toggle_panel).
            toggle_panel(tray.app_handle(), Some((rx, ry + rh)));
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
      get_helper_status,
      add_portal,
      remove_portal,
      select_portal,
      set_reuse_cookie,
      connect,
      disconnect,
      cancel,
      quit
    ])
    .run(tauri::generate_context!())
    .expect("error while running gp-bar");
}

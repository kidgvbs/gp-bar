//! GP Bar — a menu bar client for the GlobalProtect VPN.
//!
//! It orchestrates the free, open-source `gpclient` CLI (GPL-3.0, from
//! https://github.com/yuezk/GlobalProtect-openconnect) and deliberately ships
//! none of the proprietary GUI licensing machinery: connections last as long
//! as the portal allows, with no client-side time limit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod icons;
mod osascript;
mod vpn;

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Listener, Manager, PhysicalPosition, State, WindowEvent};
use tray_icon::{
  menu::{Menu, MenuItem, PredefinedMenuItem},
  Icon, TrayIcon, TrayIconBuilder,
};

use config::Config;
use vpn::{Phase, VpnController, VpnState};

const TRAY_ID: &str = "gp-bar";

// tray-icon and muda handles are `Rc`-based and single-threaded: they are
// created on the main thread in `setup` and kept alive here for the whole
// lifetime of the app.
thread_local! {
  static TRAY: RefCell<Option<(TrayIcon, Menu)>> = const { RefCell::new(None) };
}

struct AppContext {
  controller: Arc<VpnController>,
  config: Mutex<Config>,
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
fn add_portal(state: State<'_, AppContext>, portal: String) -> Result<(), String> {
  let portal = portal.trim().to_string();
  if portal.is_empty() {
    return Err("Portal is empty".to_string());
  }
  if !portal.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
    return Err("Portal may only contain letters, digits, dots, dashes and underscores".to_string());
  }
  let mut cfg = state.config.lock().unwrap();
  if !cfg.portals.contains(&portal) {
    cfg.portals.push(portal.clone());
  }
  if cfg.selected.is_none() {
    cfg.selected = Some(portal);
  }
  cfg.save()
}

#[tauri::command]
fn remove_portal(state: State<'_, AppContext>, portal: String) -> Result<(), String> {
  let mut cfg = state.config.lock().unwrap();
  cfg.portals.retain(|p| p != &portal);
  if cfg.selected.as_deref() == Some(portal.as_str()) {
    cfg.selected = cfg.portals.first().cloned();
  }
  cfg.save()
}

#[tauri::command]
fn select_portal(state: State<'_, AppContext>, portal: String) -> Result<(), String> {
  let mut cfg = state.config.lock().unwrap();
  if cfg.portals.contains(&portal) {
    cfg.selected = Some(portal);
  }
  cfg.save()
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
async fn cancel(state: State<'_, AppContext>) -> Result<(), String> {
  let controller = state.controller.clone();
  // Interrupting a worker we started needs root as well, so this also shows
  // the admin dialog. Cancel is rare; the upcoming daemon removes the prompt.
  if let Some(pid) = controller.connected_worker_pid() {
    if let Err(error) = vpn::run_admin_script(vpn::build_kill_script(pid)).await {
      eprintln!("gp-bar: cancel failed: {error}");
    }
  }
  Ok(())
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
      return Err(format!("cannot connect while {:?}", controller.snapshot().phase));
    }
    Phase::Disconnected | Phase::Error => {}
  }

  let cfg = state.config.lock().unwrap().clone();
  let gpclient = cfg.resolve_gpclient().ok_or_else(|| {
    "gpclient not found. Install it with `brew install gpclient` or set its path in the config file.".to_string()
  })?;

  std::fs::create_dir_all(Config::log_dir())
    .map_err(|error| format!("cannot create the log directory: {error}"))?;
  let log_path = Config::connect_log_path();

  let script = vpn::build_connect_script(&gpclient, portal, reuse_cookie, &log_path);
  controller.begin_connect(&app, portal.to_string());
  let _ = app.emit("vpn-log-cleared", ());

  match vpn::run_admin_script(script).await {
    Ok((stdout, _stderr)) => {
      let worker_pid = stdout.trim().parse::<u32>().ok();
      match worker_pid {
        Some(pid) => {
          controller.connect_started(&app, Some(pid));
          Ok(())
        }
        None => {
          controller.connect_failed(&app, "The privileged shell did not report a worker PID".to_string());
          Err("connect failed".to_string())
        }
      }
    }
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
  let cfg = state.config.lock().unwrap().clone();
  let gpclient = cfg
    .resolve_gpclient()
    .ok_or_else(|| "gpclient not found".to_string())?;

  controller.begin_disconnect(&app);
  if let Err(error) = vpn::run_admin_script(vpn::build_disconnect_script(&gpclient)).await {
    eprintln!("gp-bar: disconnect failed: {error}");
    // The status poller reconciles the phase from the lock file either way.
  }
  Ok(())
}

// -- panel window ---------------------------------------------------------------

fn show_panel(app: &AppHandle) {
  let Some(window) = app.get_webview_window("main") else {
    return;
  };

  // Anchor the panel just below the menu bar. Opened from a tray click the
  // cursor sits on the icon itself (menu-bar height); opened from the menu
  // it sits a few items further down. Either way it approximates the icon.
  if let Ok(size) = window.outer_size() {
    if let Ok(cursor) = window.cursor_position() {
      let mut x = (cursor.x - size.width as f64 / 2.0).max(0.0);
      // Keep the panel on the screen that holds the cursor.
      if let Ok(Some(monitor)) = app.monitor_from_point(cursor.x, cursor.y) {
        let max_x =
          monitor.position().x as f64 + monitor.size().width as f64 - size.width as f64;
        x = x.min(max_x.max(0.0));
      }
      let y = (cursor.y + 18.0).max(32.0);
      let _ = window.set_position(PhysicalPosition::new(x as i32, y as i32));
    }
  }

  let _ = window.show();
  let _ = window.set_focus();
}

fn sync_tray_with_state(app: &AppHandle, phase: Phase) {
  // tray-icon handles are main-thread only, and vpn-state events are emitted
  // from the status poller's worker task, so hop to the main thread.
  let _ = app.run_on_main_thread(move || {
    let connected = phase == Phase::Connected;
    let tooltip = match phase {
      Phase::Connected => "GP Bar — Connected",
      Phase::Connecting => "GP Bar — Connecting…",
      Phase::Disconnecting => "GP Bar — Disconnecting…",
      Phase::Error => "GP Bar — Error",
      Phase::Disconnected => "GP Bar — Disconnected",
    };
    let image = icons::padlock_icon(connected);
    TRAY.with(|slot| {
      let guard = slot.borrow();
      let Some((tray, _menu)) = guard.as_ref() else { return };
      if let Ok(icon) = Icon::from_rgba(image.rgba().to_vec(), image.width(), image.height()) {
        let _ = tray.set_icon_with_as_template(Some(icon), true);
      }
      let _ = tray.set_tooltip(Some(tooltip));
    });
  });
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
    })
    // Menu selections. On macOS the tray menu is presented by AppKit itself
    // (see `setup`), but its items still surface here through muda.
    .on_menu_event(|app, event| match event.id.as_ref() {
      "show" => show_panel(app),
      "connect" => {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
          let state = app.state::<AppContext>();
          let (portal, reuse) = {
            let cfg = state.config.lock().unwrap();
            (cfg.selected.clone(), cfg.reuse_cookie)
          };
          if let Some(portal) = portal {
            if let Err(error) = do_connect(app.clone(), &state, &portal, reuse).await {
              eprintln!("gp-bar: tray connect failed: {error}");
            }
          }
        });
      }
      "disconnect" => {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
          let state = app.state::<AppContext>();
          if let Err(error) = do_disconnect(app.clone(), &state).await {
            eprintln!("gp-bar: tray disconnect failed: {error}");
          }
        });
      }
      "quit" => {
        app.exit(0);
      }
      _ => {}
    })
    .on_window_event(|window, event| {
      // Popover behaviour: dismiss the panel when it loses focus.
      if let WindowEvent::Focused(false) = event {
        let _ = window.hide();
      }
    })
    .setup(move |app| {
      // Menu bar-only app: no Dock icon, no Cmd+Tab entry.
      let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

      let show_item = MenuItem::with_id("show", "Show GP Bar", true, None);
      let connect_item = MenuItem::with_id("connect", "Connect to selected portal", true, None);
      let disconnect_item = MenuItem::with_id("disconnect", "Disconnect", true, None);
      let quit_item = MenuItem::with_id("quit", "Quit GP Bar", true, None);
      let menu = Menu::with_items(&[
        &show_item,
        &PredefinedMenuItem::separator(),
        &connect_item,
        &disconnect_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
      ])?;

      let image = icons::padlock_icon(false);
      let icon = Icon::from_rgba(image.rgba().to_vec(), image.width(), image.height())
        .map_err(|error| format!("invalid tray icon: {error}"))?;

      // Tray created directly (not through tauri::tray), pinned to 0.25.0 —
      // the configuration empirically verified on this machine. The early
      // "menu attach" theories around macOS 27 beta were red herrings: the
      // tray had always worked, but the app was frozen by a poller deadlock
      // (see vpn::update). With menu-on-left-click off, a left click
      // surfaces as a raw event in the handler below and opens the panel;
      // a right click lets tray-icon present the native menu.
      let tray = TrayIconBuilder::new()
        .with_id(TRAY_ID)
        .with_icon(icon)
        .with_icon_as_template(true)
        .with_tooltip("GP Bar")
        .with_menu(Box::new(menu.clone()))
        .with_menu_on_left_click(false)
        .build()?;

      // Keep the tray and menu handles alive for the lifetime of the app.
      TRAY.with(|slot| *slot.borrow_mut() = Some((tray, menu)));

      // This handler replaces tauri's global TrayIconEvent dispatcher (the
      // app no longer keeps a tauri tray of its own), so raw tray events
      // surface here directly. One click on the padlock opens the panel —
      // the same interaction model as GP Connect.
      let click_handle = app.handle().clone();
      tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
        match event {
          tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
          } => show_panel(&click_handle),
          _ => {}
        }
      }));

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
      connect,
      disconnect,
      cancel
    ])
    .run(tauri::generate_context!())
    .expect("error while running gp-bar");
}

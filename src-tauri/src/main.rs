//! GP Bar — a menu bar client for the GlobalProtect VPN.
//!
//! It orchestrates the free, open-source `gpclient` CLI (GPL-3.0, from
//! https://github.com/yuezk/GlobalProtect-openconnect) and deliberately ships
//! none of the proprietary GUI licensing machinery: connections last as long
//! as the portal allows, with no client-side time limit.

#![cfg_attr(not(debug_assertions), tauri::windows_subsystem = "windows")]

mod config;
mod icons;
mod osascript;
mod vpn;

use std::sync::{Arc, Mutex};

use tauri::{
  menu::{Menu, MenuItem, PredefinedMenuItem},
  tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
  AppHandle, Emitter, Listener, Manager, PhysicalPosition, State, WindowEvent,
};

use config::Config;
use vpn::{Phase, VpnController, VpnState};

const TRAY_ID: &str = "gp-bar";

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

fn toggle_panel(app: &AppHandle, tray_rect: Option<tauri::Rect>) {
  let Some(window) = app.get_webview_window("main") else {
    return;
  };

  if window.is_visible().unwrap_or(false) {
    let _ = window.hide();
    return;
  }

  if let Some(rect) = tray_rect {
    if let Ok(size) = window.outer_size() {
      // The tray rect carries Position/Size enums; resolve both to physical
      // pixels using this window's scale factor (a physical value passes
      // through to_physical unchanged).
      let scale = window.scale_factor().unwrap_or(2.0);
      let pos: tauri::PhysicalPosition<i32> = rect.position.to_physical(scale);
      let rect_size: tauri::PhysicalSize<u32> = rect.size.to_physical(scale);
      let x = (pos.x as f64 + rect_size.width as f64 / 2.0 - size.width as f64 / 2.0).max(0.0);
      let y = pos.y as f64 + rect_size.height as f64 + 6.0;
      let position: tauri::PhysicalPosition<i32> = PhysicalPosition::new(x as i32, y as i32);
      let _ = window.set_position(position);
    }
  }

  let _ = window.show();
  let _ = window.set_focus();
}

fn sync_tray_with_state(app: &AppHandle) {
  let Some(state) = app.try_state::<AppContext>() else { return };
  let phase = state.controller.snapshot().phase;
  let Some(tray) = app.tray_by_id(TRAY_ID) else { return };

  let connected = phase == Phase::Connected;
  let _ = tray.set_icon(Some(icons::padlock_icon(connected)));
  let _ = tray.set_icon_as_template(true);
  let tooltip = match phase {
    Phase::Connected => "GP Bar — Connected",
    Phase::Connecting => "GP Bar — Connecting…",
    Phase::Disconnecting => "GP Bar — Disconnecting…",
    Phase::Error => "GP Bar — Error",
    Phase::Disconnected => "GP Bar — Disconnected",
  };
  let _ = tray.set_tooltip(Some(tooltip));
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
    .on_window_event(|window, event| {
      // Popover behaviour: dismiss the panel when it loses focus.
      if let WindowEvent::Focused(false) = event {
        let _ = window.hide();
      }
    })
    .setup(move |app| {
      // Menu bar-only app: no Dock icon, no Cmd+Tab entry.
      #[cfg(target_os = "macos")]
      let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

      let show_item = MenuItem::with_id(app, "show", "Show GP Bar", true, None::<&str>)?;
      let connect_item = MenuItem::with_id(app, "connect", "Connect to selected portal", true, None::<&str>)?;
      let disconnect_item = MenuItem::with_id(app, "disconnect", "Disconnect", true, None::<&str>)?;
      let quit_item = MenuItem::with_id(app, "quit", "Quit GP Bar", true, None::<&str>)?;
      let menu = Menu::with_items(
        app,
        &[
          &show_item,
          &PredefinedMenuItem::separator(app)?,
          &connect_item,
          &disconnect_item,
          &PredefinedMenuItem::separator(app)?,
          &quit_item,
        ],
      )?;

      let _tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icons::padlock_icon(false))
        .icon_as_template(true)
        .tooltip("GP Bar")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
          "show" => toggle_panel(app, None),
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
        .on_tray_icon_event(|tray, event| {
          if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            rect,
            ..
          } = event
          {
            let app = tray.app_handle().clone();
            toggle_panel(&app, Some(rect));
          }
        })
        .build(app)?;

      vpn::spawn_status_poller(app.handle().clone(), poller_controller);
      vpn::spawn_log_tailer(app.handle().clone(), tailer_controller);

      // Reflect state transitions onto the tray icon and tooltip.
      let icon_handle = app.handle().clone();
      app.listen_any("vpn-state", move |_event| {
        sync_tray_with_state(&icon_handle);
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

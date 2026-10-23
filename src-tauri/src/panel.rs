//! The popover as a native macOS `NSPanel` (see docs/nspanel-design.md).
//!
//! The `main` window declared in tauri.conf.json is converted into an
//! `NSPanel` subclass at setup. The flags mirror GP Connect 3.2.0's popover:
//! `hidesOnDeactivate`, `becomesKeyOnlyIfNeeded`, `worksWhenModal`,
//! `floatingPanel`, plus the `NonactivatingPanel` style mask, so clicking the
//! tray icon never activates the app or steals focus from the foreground app.

use std::ptr::NonNull;

use tauri::Manager;
use tauri_nspanel::{tauri_panel, ManagerExt, PanelLevel, WebviewWindowExt};

// NSEvent/NSWindowStyleMask come pre-imported by the tauri_panel! expansion.
use objc2_app_kit::{NSEventMask, NSEventType, NSWindowStyleMask};

/// The `main` window label.
pub(crate) const PANEL_LABEL: &str = "main";

tauri_panel! {
    // The boolean getters are overridden at the class level; the setters with
    // no getter counterpart (`works_when_modal`) are applied per-instance in
    // `convert_to_panel`.
    panel!(PopoverPanel {
        config: {
            can_become_key_window: true,
            can_become_main_window: false,
            becomes_key_only_if_needed: true,
            is_floating_panel: true,
            hides_on_deactivate: true
        }
    })
}

/// A margin around the panel frame in which a click still counts as "inside".
const EDGE_TOLERANCE: f64 = 2.0;

/// Convert the `main` window into the native panel and install the global
/// click-outside monitor. Must run on the main thread (`setup`).
#[cfg(target_os = "macos")]
pub(crate) fn convert_to_panel(app: &tauri::AppHandle) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window(PANEL_LABEL) else {
        return Ok(());
    };
    let panel = window.to_panel::<PopoverPanel>()?;

    // canBecomeKey* / floating / hidesOnDeactivate come from the class config;
    // the ones without a getter override are applied on the instance.
    panel.set_works_when_modal(true);
    panel.set_level(PanelLevel::Floating.value());

    // GP Connect ships the NonactivatingPanel style bit: the panel never
    // activates the app on click. ADD the bit — replacing the whole mask would
    // drop tauri's structural window styles (see ahkohd/tauri-nspanel#123).
    let _ = panel.add_style_mask(NSWindowStyleMask::NonactivatingPanel);

    install_click_out_monitor(app.clone());
    Ok(())
}

/// Hide the panel when the user clicks outside of it.
///
/// `addGlobalMonitorForEventsMatchingMask` only fires for events NOT
/// delivered to our own app, so clicks on the tray icon or inside the panel
/// (HTML dropdowns included — same process) never reach this handler: no
/// self-dismissal, no double-hide against `toggle_panel`.
#[cfg(target_os = "macos")]
fn install_click_out_monitor(app: tauri::AppHandle) {
    let mask = NSEventMask::LeftMouseDown | NSEventMask::LeftMouseDragged;

    let block = block2::RcBlock::new(move |event: NonNull<NSEvent>| {
        let Ok(panel) = app.get_webview_panel(PANEL_LABEL) else {
            return;
        };
        if !panel.is_visible() {
            return;
        }
        // Pin takes precedence: a connect/disconnect in flight keeps the panel
        // on screen (see AppContext::panel_pinned).
        if app
            .try_state::<crate::AppContext>()
            .is_some_and(|state| state.panel_pinned())
        {
            return;
        }
        let event = unsafe { event.as_ref() };
        // Only real clicks, not the drags allowed into the mask.
        if event.r#type() != NSEventType::LeftMouseDown {
            return;
        }
        // Global-monitor coordinates are screen-global (bottom-left origin):
        // compare against the panel frame directly.
        let location = event.locationInWindow();
        let frame = panel.as_panel().frame();
        let inside = location.x >= frame.origin.x - EDGE_TOLERANCE
            && location.x <= frame.origin.x + frame.size.width + EDGE_TOLERANCE
            && location.y >= frame.origin.y - EDGE_TOLERANCE
            && location.y <= frame.origin.y + frame.size.height + EDGE_TOLERANCE;
        if !inside {
            panel.hide();
        }
    });
    let monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block);
    // Both the block and the monitor token are app-lifetime objects on the
    // main thread; they are intentionally not stored for removal.
    let _ = monitor;
}
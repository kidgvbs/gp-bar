//! Panel placement: pure geometry in ONE canonical screen space.
//!
//! macOS really runs TWO screen spaces here, flipped against each other
//! in y — this module's one job is to never mix them:
//!
//! * AppKit (`NSEvent::mouseLocation`, NSScreen/NSWindow frames) is
//!   BOTTOM-LEFT origin, y up. The tray payload is born there and is
//!   flipped into top-left by tray-icon (and by tao's cursor helper)
//!   using the MAIN screen's height from CGDisplayPixelsHigh — which
//!   behaves as POINTS on this OS build (only the logical height fits
//!   both the single- and the dual-display payloads) — then scaled by
//!   the tray window's own factor. Pre-scale, the payload IS canonical.
//! * Quartz display space (`CGDisplayBounds` — what tao's
//!   `Monitor::position()` passes through RAW; tauri's `work_area` just
//!   adds the menu-bar/dock insets to it) is TOP-LEFT origin, y down:
//!   [`monitor_layout`] output is already canonical, no flip needed.
//!
//! Invariant: never flip tao's raw positions with the main screen's
//! height. The flip is correct only for the primary (its origin (0,0)
//! is invariant under it); for every other monitor it mirrors the
//! rectangle across the primary — a display above the laptop would be
//! recorded below, and anchors on the external menu bar would never
//! match. `monitor_layout` builds the table by pure division instead,
//! and [`place_panel`] hit-tests the payload per-monitor-scale.

use tauri::AppHandle;

/// One monitor in canonical logical points, top-left origin. `bounds` is
/// the full screen rectangle; `work` is the AppKit visibleFrame (menu
/// bar and dock excluded), same shape: `(x, y_top_left, width, height)`.
#[derive(Clone)]
pub struct MonitorLayout {
    pub name: String,
    pub scale: f64,
    pub bounds: (f64, f64, f64, f64),
    pub work: (f64, f64, f64, f64),
}

/// The point the panel should appear under: either a raw tray-icon
/// payload (logical points × the tray screen's OWN scale — see the
/// module doc) or an already-canonical logical point (the cursor
/// fallback, un-scaled by the caller).
#[derive(Clone, Copy)]
pub enum Anchor {
    TrayPayload(f64, f64),
    Logical(f64, f64),
}

/// Where [`place_panel`] decided to put the window.
#[derive(Debug, PartialEq)]
pub struct Placement {
    /// Canonical logical position, for `set_position(LogicalPosition)`.
    pub position: (f64, f64),
    /// The anchor after un-scaling: the winning monitor's logical point.
    pub anchor: (f64, f64),
    /// Index into the `monitors` slice that matched.
    pub monitor: usize,
}

/// tauri monitor → canonical layout: pure division by the scale, no
/// flip (tao's positions are already in the top-left Quartz space, see
/// the module doc).
pub fn monitor_layout(monitor: &tauri::Monitor) -> MonitorLayout {
    let scale = monitor.scale_factor();
    let pos = monitor.position();
    let size = monitor.size();
    let work = monitor.work_area();
    MonitorLayout {
        name: monitor.name().cloned().unwrap_or_default(),
        scale,
        bounds: (
            f64::from(pos.x) / scale,
            f64::from(pos.y) / scale,
            f64::from(size.width) / scale,
            f64::from(size.height) / scale,
        ),
        work: (
            f64::from(work.position.x) / scale,
            f64::from(work.position.y) / scale,
            f64::from(work.size.width) / scale,
            f64::from(work.size.height) / scale,
        ),
    }
}

/// Every monitor, in tauri's order, in canonical logical points.
pub fn monitor_layouts(app: &AppHandle) -> Vec<MonitorLayout> {
    app.available_monitors()
        .unwrap_or_default()
        .iter()
        .map(monitor_layout)
        .collect()
}

/// Where to put the panel's top-left corner so the panel sits centered
/// under `anchor`, inside the matching monitor's work area.
///
/// The hit-test tries each monitor's scale for a payload point (the
/// payload is scaled by the tray screen's own factor) and keeps the
/// first whose logical bounds contain the result; on a real desk (no
/// overlapping mixed-DPI projections) the match is unambiguous. A
/// logical anchor is tested as-is. `None` (no anchor, or none matched)
/// defers to the caller's fallback.
///
/// Everything is in logical points and `set_position(Logical)` is
/// scale-independent on tao (`to_logical` is the identity for a Logical
/// position), so a mixed-DPI desk is plain geometry here.
pub fn place_panel(
    anchor: Option<Anchor>,
    monitors: &[MonitorLayout],
    panel: (f64, f64),
) -> Option<Placement> {
    let (monitor, lx, ly) = monitors.iter().enumerate().find_map(|(index, mon)| {
        let (lx, ly) = match anchor {
            Some(Anchor::TrayPayload(ax, ay)) => (ax / mon.scale, ay / mon.scale),
            Some(Anchor::Logical(lx, ly)) => (lx, ly),
            None => return None,
        };
        let (bx, by, bw, bh) = mon.bounds;
        (lx >= bx && lx < bx + bw && ly >= by && ly < by + bh).then_some((index, lx, ly))
    })?;
    let mon = &monitors[monitor];
    let (wx, wy, ww, wh) = mon.work;
    let (panel_w, panel_h) = panel;
    // `clamp` panics when min exceeds max: a work area smaller than the
    // panel must degrade to an edge, not take the app down.
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
    Some(Placement {
        position: (x, y),
        anchor: (lx, ly),
        monitor,
    })
}

/// The primary screen's top-right corner — the never-seen-on-a-real-desk
/// degradation for when no monitor matched (see `toggle_panel`).
pub fn fallback_position(work: (f64, f64, f64, f64), panel_w: f64) -> (f64, f64) {
    let (wx, wy, ww, _) = work;
    (wx + ww - panel_w, wy)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel window's logical size from tauri.conf.json.
    const PANEL: (f64, f64) = (360.0, 540.0);

    /// The laptop from the RECAP §13 dual-display payloads: built-in
    /// 1440×900 logical @2x, menu bar 30 pt, work area 870 pt tall.
    fn laptop() -> MonitorLayout {
        MonitorLayout {
            name: "Built-in Retina Display".to_string(),
            scale: 2.0,
            bounds: (0.0, 0.0, 1440.0, 900.0),
            work: (0.0, 30.0, 1440.0, 870.0),
        }
    }

    /// The external from the same payloads: 2560×1440 @1x, ABOVE the
    /// laptop (negative y in the canonical space), 30 pt of its own
    /// menu bar at the top.
    fn external() -> MonitorLayout {
        MonitorLayout {
            name: "DELL U2720Q".to_string(),
            scale: 1.0,
            bounds: (0.0, -1440.0, 2560.0, 1440.0),
            work: (0.0, -1410.0, 2560.0, 1410.0),
        }
    }

    fn desk() -> Vec<MonitorLayout> {
        vec![laptop(), external()]
    }

    /// The real incident payloads: on the laptop the tray payload is
    /// physical-style points ×2 (2118, 60) → logical (1059, 30) → the
    /// panel centered under it; on the external @1x the payload is
    /// already the logical anchor. Numbers from RECAP §13, both verified
    /// on the real desk.
    #[test]
    fn tray_payloads_anchor_on_the_right_monitor() {
        let monitors = desk();
        let placement =
            place_panel(Some(Anchor::TrayPayload(2118.0, 60.0)), &monitors, PANEL).unwrap();
        assert_eq!(placement.monitor, 0);
        assert_eq!(placement.anchor, (1059.0, 30.0));
        assert_eq!(placement.position, (879.0, 30.0));

        let placement =
            place_panel(Some(Anchor::TrayPayload(2070.0, -1410.0)), &monitors, PANEL).unwrap();
        assert_eq!(placement.monitor, 1);
        assert_eq!(placement.anchor, (2070.0, -1410.0));
        assert_eq!(placement.position, (1890.0, -1410.0));
    }

    /// The clamps: a centered x can leave the work area at either edge,
    /// and y never crosses the menu bar at the top nor the bottom edge
    /// of the work area.
    #[test]
    fn clamps_keep_the_panel_inside_the_work_area() {
        let monitors = vec![laptop()];
        // Left edge: half the panel would hang off the screen's left.
        let placement = place_panel(Some(Anchor::TrayPayload(0.0, 60.0)), &monitors, PANEL);
        assert_eq!(placement.unwrap().position, (0.0, 30.0));
        // Right edge: payload x = 2800 → logical 1400 → centered at 1220,
        // past the work area's right edge.
        let placement =
            place_panel(Some(Anchor::TrayPayload(2800.0, 60.0)), &monitors, PANEL).unwrap();
        assert_eq!(placement.position, (1080.0, 30.0));
        // Top: an anchor above the work area (in the menu bar) clamps to
        // the work area's top edge.
        let placement = place_panel(Some(Anchor::TrayPayload(1758.0, 20.0)), &monitors, PANEL);
        assert_eq!(placement.unwrap().position, (699.0, 30.0));
        // Bottom: an anchor near the bottom edge never pushes the panel
        // past the work area's bottom.
        let placement =
            place_panel(Some(Anchor::TrayPayload(1758.0, 1760.0)), &monitors, PANEL).unwrap();
        assert_eq!(placement.position, (699.0, 360.0));
    }

    /// A work area smaller than the panel degrades to an edge instead of
    /// panicking inside `clamp` (the reason the x/y branches exist).
    #[test]
    fn a_work_area_smaller_than_the_panel_degrades_to_the_edge() {
        let tiny = MonitorLayout {
            name: "Tiny".to_string(),
            scale: 1.0,
            bounds: (0.0, 0.0, 200.0, 200.0),
            work: (0.0, 0.0, 200.0, 200.0),
        };
        let placement =
            place_panel(Some(Anchor::TrayPayload(100.0, 100.0)), &[tiny], PANEL).unwrap();
        assert_eq!(placement.position, (0.0, 100.0));
    }

    /// No anchor, or an anchor no monitor contains: the caller's
    /// fallback decides, not us.
    #[test]
    fn no_matching_monitor_falls_back_to_the_caller() {
        let monitors = desk();
        assert_eq!(place_panel(None, &monitors, PANEL), None);
        // Off every screen (the old monitor_from_point false negative).
        assert_eq!(
            place_panel(Some(Anchor::Logical(2000.0, 500.0)), &monitors, PANEL),
            None
        );
    }

    /// The hit-test result must not depend on the order tauri happens to
    /// list the monitors: the payload matches exactly one screen's
    /// bounds whichever is tried first.
    #[test]
    fn the_match_is_independent_of_monitor_order() {
        let forward = desk();
        let reversed: Vec<_> = forward.iter().rev().cloned().collect();
        let a = place_panel(Some(Anchor::TrayPayload(2118.0, 60.0)), &forward, PANEL);
        let b = place_panel(Some(Anchor::TrayPayload(2118.0, 60.0)), &reversed, PANEL);
        assert_eq!(a.unwrap().position, b.unwrap().position);

        let a = place_panel(Some(Anchor::TrayPayload(2070.0, -1410.0)), &forward, PANEL);
        let b = place_panel(Some(Anchor::TrayPayload(2070.0, -1410.0)), &reversed, PANEL);
        assert_eq!(a.unwrap().position, b.unwrap().position);
    }

    /// A logical anchor passes through un-scaled: the cursor fallback
    /// must land exactly where an equivalent payload would.
    #[test]
    fn logical_anchors_pass_through_unscaled() {
        let monitors = desk();
        let placement = place_panel(Some(Anchor::Logical(1059.0, 30.0)), &monitors, PANEL).unwrap();
        assert_eq!(placement.monitor, 0);
        assert_eq!(placement.position, (879.0, 30.0));
    }

    /// The fallback: the primary's top-right corner, clear of the menu
    /// bar — (1080, 30) on the RECAP §13 laptop, the position the
    /// pre-fix bug actually landed on mid-screen being the symptom.
    #[test]
    fn the_fallback_is_the_primary_top_right_corner() {
        assert_eq!(
            fallback_position((0.0, 30.0, 1440.0, 870.0), 360.0),
            (1080.0, 30.0)
        );
    }
}

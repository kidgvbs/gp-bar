//! Tray template images, drawn in code from signed distance functions.
//!
//! Template images are monochrome: black pixels with an alpha channel, which
//! macOS automatically adapts to light and dark menu bars. The two states
//! follow Apple's SF Symbols pair `lock.fill` / `lock.open.fill`: the same
//! solid padlock, closed with a keyhole when connected and with the shackle
//! swung open on the right when disconnected.

const LOGICAL: usize = 22; // points; the size of a standard menu bar glyph
const SUPERSAMPLE: usize = 4; // draw 4x and box-average for anti-aliasing

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Signed distance to a rounded rectangle centred in a 0..22 box.
fn sd_rounded_rect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let dx = (x - cx).abs() - hw + r;
    let dy = (y - cy).abs() - hh + r;
    let outside = (dx.max(0.0), dy.max(0.0)).length_f32();
    outside.max(0.0) + (dx.max(dy)).min(0.0) - r
}

fn sd_circle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r
}

trait Length {
    fn length_f32(&self) -> f32;
}
impl Length for (f32, f32) {
    fn length_f32(&self) -> f32 {
        self.0.hypot(self.1)
    }
}

/// Per-point coverage: 1.0 = black, 0.0 = transparent.
///
/// Geometry (points in a 0..22 box):
/// * body: a 16x11 rounded rect centred at (11, 13.9), corners 2.8 — the
///   glyph fills more of the canvas than a stock menulet (3pt margins).
/// * shackle: a 2.2pt-thick ring, mid radius 3.9, centred at (11, 8.4).
///   The left leg always drops into the body; in the closed state the
///   right one does too, while in the open state it stops well above the
///   body (cut at y=6.9 with a rounded cap), like SF Symbols' lock.open.
fn padlock_coverage(x: f32, y: f32, connected: bool) -> f32 {
    let body = sd_rounded_rect(x, y, 11.0, 13.9, 8.0, 5.5, 2.8);

    let ring_mid = 3.9f32;
    let ring_half = 1.1f32;
    let ring = sd_circle(x, y, 11.0, 8.4, ring_mid).abs() - ring_half;

    // Where the shackle exists: above the cut line the ring shows, below it
    // the ring is irrelevant (the body covers the joint).
    let leg_cut = if connected || x <= 11.0 { 10.5 } else { 6.9 };
    let shackle = if y < leg_cut { ring } else { 1.0 };

    let body_fill = 1.0 - smoothstep(-0.5, 0.5, body);
    let shackle_fill = 1.0 - smoothstep(-0.5, 0.5, shackle);
    let mut alpha = body_fill.max(shackle_fill);

    if !connected {
        // Rounded cap on the open shackle's free end: eat a soft disc centred
        // on the cut, which trims the stub's corners without touching the body.
        let cap = 1.0 - smoothstep(0.0, 1.3, (x - 14.9).hypot(y - 6.9));
        alpha *= 1.0 - cap;
    } else {
        // Keyhole: a small circle plus a stem, cut out of the body.
        let hole =
            sd_circle(x, y, 11.0, 12.4, 1.6).min(sd_rounded_rect(x, y, 11.0, 14.7, 0.65, 1.5, 0.6));
        let cut = 1.0 - smoothstep(-0.5, 0.5, hole);
        // The cut only applies inside the body area.
        if body < 0.5 {
            alpha *= 1.0 - cut;
        }
    }
    alpha
}

/// Render the padlock as an RGBA buffer of a template image (22x22).
pub fn padlock_icon(connected: bool) -> tauri::image::Image<'static> {
    let mut rgba = Vec::with_capacity(LOGICAL * LOGICAL * 4);

    for py in 0..LOGICAL {
        for px in 0..LOGICAL {
            let mut acc = 0.0f32;
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let x = (px * SUPERSAMPLE + sx) as f32 / SUPERSAMPLE as f32
                        + 0.5 / SUPERSAMPLE as f32;
                    let y = (py * SUPERSAMPLE + sy) as f32 / SUPERSAMPLE as f32
                        + 0.5 / SUPERSAMPLE as f32;
                    acc += padlock_coverage(x, y, connected);
                }
            }
            let alpha = ((acc / (SUPERSAMPLE * SUPERSAMPLE) as f32) * 255.0).round() as u8;
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }

    tauri::image::Image::new_owned(rgba, LOGICAL as u32, LOGICAL as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_22x22_with_alpha() {
        for filled in [true, false] {
            let icon = padlock_icon(filled);
            assert_eq!(icon.width(), 22);
            assert_eq!(icon.height(), 22);
            assert_eq!(icon.rgba().len(), 22 * 22 * 4);
        }
    }

    #[test]
    fn connected_and_open_differ() {
        let connected = padlock_icon(true).rgba().to_vec();
        let open = padlock_icon(false).rgba().to_vec();
        assert_ne!(connected, open);
    }
}

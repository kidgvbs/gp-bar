//! Tray template images, drawn in code from signed distance functions.
//!
//! Template images are monochrome: black pixels with an alpha channel, which
//! macOS automatically adapts to light and dark menu bars. Two states are
//! rendered: a filled padlock (connected) and a hollow one (disconnected).

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
fn padlock_coverage(x: f32, y: f32, filled: bool) -> f32 {
  // Body: rounded rect centred at (11, 14), 13x9 with 2pt corners.
  let body = sd_rounded_rect(x, y, 11.0, 14.0, 6.5, 4.5, 2.0);

  // Shackle: ring centred at (11, 8.5), clipped above the body.
  let ring_mid = 3.2f32;
  let ring_half = 1.0f32;
  let ring = sd_circle(x, y, 11.0, 8.5, ring_mid).abs() - ring_half;
  let shackle = if y < 10.0 { ring } else { 1.0 };

  let mut alpha = 0.0f32;
  if filled {
    let body_fill = 1.0 - smoothstep(-0.5, 0.5, body);
    let shackle_fill = 1.0 - smoothstep(-0.5, 0.5, shackle);
    alpha = alpha.max(body_fill).max(shackle_fill);

    // Keyhole: a small circle plus a stem, cut out of the body.
    let hole = sd_circle(x, y, 11.0, 12.9, 1.4)
      .min(sd_rounded_rect(x, y, 11.0, 14.6, 0.55, 1.4, 0.5));
    let cut = 1.0 - smoothstep(-0.5, 0.5, hole);
    // The cut only applies inside the body area.
    if body < 0.5 {
      alpha *= 1.0 - cut;
    }
  } else {
    // Hollow body: a 1.4pt outline around the same rounded rect.
    let body_band = 1.0 - smoothstep(-0.7, 0.7, body.abs() - 0.0);
    let shackle_band = 1.0 - smoothstep(-0.5, 0.5, shackle);
    alpha = alpha.max(body_band).max(shackle_band);
  }
  alpha
}

/// Render the padlock as an RGBA buffer of a template image (22x22).
pub fn padlock_icon(filled: bool) -> tauri::image::Image<'static> {
  let mut rgba = Vec::with_capacity(LOGICAL * LOGICAL * 4);

  for py in 0..LOGICAL {
    for px in 0..LOGICAL {
      let mut acc = 0.0f32;
      for sy in 0..SUPERSAMPLE {
        for sx in 0..SUPERSAMPLE {
          let x = (px * SUPERSAMPLE + sx) as f32 / SUPERSAMPLE as f32 + 0.5 / SUPERSAMPLE as f32;
          let y = (py * SUPERSAMPLE + sy) as f32 / SUPERSAMPLE as f32 + 0.5 / SUPERSAMPLE as f32;
          acc += padlock_coverage(x, y, filled);
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
  fn filled_and_hollow_differ() {
    let filled = padlock_icon(true).rgba().to_vec();
    let hollow = padlock_icon(false).rgba().to_vec();
    assert_ne!(filled, hollow);
  }
}

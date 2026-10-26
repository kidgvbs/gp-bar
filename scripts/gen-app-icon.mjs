// Generates the GP Bar app icon (src-tauri/icons/icon.icns) with zero image
// dependencies: an RGBA raster is rendered in JS, encoded as PNG by hand
// (zlib + CRC32), and packed into an .icns with iconutil.
//
// The artwork is original: a rounded-square gradient with a white padlock,
// matching the tray template glyph drawn in src-tauri/src/icons.rs.

import { execFileSync } from "node:child_process";
import { deflateSync } from "node:zlib";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const SIZE = 1024;

// -- padlock geometry (mirrors src-tauri/src/icons.rs) --------------------------

function sdRoundedRect(x, y, cx, cy, hw, hh, r) {
  const dx = Math.abs(x - cx) - hw + r;
  const dy = Math.abs(y - cy) - hh + r;
  const outside = Math.hypot(Math.max(dx, 0), Math.max(dy, 0));
  return Math.max(0, outside) + Math.min(Math.max(dx, dy), 0) - r;
}

function sdCircle(x, y, cx, cy, r) {
  return Math.hypot(x - cx, y - cy) - r;
}

function smoothstep(a, b, x) {
  const t = Math.min(Math.max((x - a) / (b - a), 0), 1);
  return t * t * (3 - 2 * t);
}

function padlockCoverage(x, y) {
  const s = SIZE / 22; // scale from the 22pt design grid
  const px = x / s;
  const py = y / s;

  const body = sdRoundedRect(px, py, 11, 14, 6.5, 4.5, 2);
  const ring = Math.abs(sdCircle(px, py, 11, 8.5, 3.2)) - 1.0;
  const shackle = py < 10 ? ring : 1;
  const hole = Math.min(
    sdCircle(px, py, 11, 12.9, 1.4),
    sdRoundedRect(px, py, 11, 14.6, 0.55, 1.4, 0.5)
  );

  let alpha = Math.max(
    1 - smoothstep(-0.5, 0.5, body),
    1 - smoothstep(-0.5, 0.5, shackle)
  );
  if (body < 0.5) alpha *= 1 - (1 - smoothstep(-0.5, 0.5, hole));
  return alpha;
}

// macOS squircle corner radius ~= 22.37% of the side for a 1024 icon.
function insideSquircle(x, y) {
  const r = SIZE * 0.2237;
  const d = sdRoundedRect(x, y, SIZE / 2, SIZE / 2, SIZE / 2, SIZE / 2, r);
  return 1 - smoothstep(-1.5, 1.5, d);
}

function renderIcon() {
  const rgba = new Uint8Array(SIZE * SIZE * 4);
  for (let y = 0; y < SIZE; y++) {
    for (let x = 0; x < SIZE; x++) {
      // Diagonal gradient from #0a6cff to #0a3d80.
      const t = (x + y) / (2 * SIZE);
      const r = Math.round(10 + (10 - 10) * t);
      const g = Math.round(108 + (61 - 108) * t);
      const b = Math.round(255 + (128 - 255) * t);

      const glyph = padlockCoverage(x, y);
      const mask = insideSquircle(x, y);

      // Blend the white glyph over the gradient.
      const R = Math.round(255 * glyph + r * (1 - glyph));
      const G = Math.round(255 * glyph + g * (1 - glyph));
      const B = Math.round(255 * glyph + b * (1 - glyph));
      const A = Math.round(255 * mask);

      const i = (y * SIZE + x) * 4;
      rgba[i] = R;
      rgba[i + 1] = G;
      rgba[i + 2] = B;
      rgba[i + 3] = A;
    }
  }
  return rgba;
}

// -- minimal PNG encoder ---------------------------------------------------------

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(bytes) {
  let c = -1;
  for (const byte of bytes) c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const out = new Uint8Array(12 + data.length);
  const view = new DataView(out.buffer);
  view.setUint32(0, data.length);
  for (let i = 0; i < 4; i++) out[4 + i] = type.charCodeAt(i);
  out.set(data, 8);
  view.setUint32(8 + data.length, crc32(out.subarray(4, 8 + data.length)));
  return out;
}

function encodePng(rgba, width, height) {
  // Prepend the filter byte (0 = None) to every scanline.
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  for (let y = 0; y < height; y++) {
    raw[y * (stride + 1)] = 0;
    Buffer.from(rgba.buffer, y * stride, stride).copy(raw, y * (stride + 1) + 1);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  ihdr[10] = 0; // deflate
  ihdr[11] = 0; // adaptive filtering
  ihdr[12] = 0; // no interlace

  const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const idat = deflateSync(raw, { level: 9 });
  return Buffer.concat([
    signature,
    chunk("IHDR", ihdr),
    chunk("IDAT", idat),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// -- bilinear resize (for the smaller icns sizes) --------------------------------

function resize(rgba, from, to) {
  const out = new Uint8Array(to * to * 4);
  for (let y = 0; y < to; y++) {
    const sy = ((y + 0.5) * from) / to - 0.5;
    const y0 = Math.max(0, Math.floor(sy));
    const y1 = Math.min(from - 1, y0 + 1);
    const fy = sy - y0;
    for (let x = 0; x < to; x++) {
      const sx = ((x + 0.5) * from) / to - 0.5;
      const x0 = Math.max(0, Math.floor(sx));
      const x1 = Math.min(from - 1, x0 + 1);
      const fx = sx - x0;
      for (let c = 0; c < 4; c++) {
        const a = rgba[(y0 * from + x0) * 4 + c];
        const b = rgba[(y0 * from + x1) * 4 + c];
        const cc = rgba[(y1 * from + x0) * 4 + c];
        const d = rgba[(y1 * from + x1) * 4 + c];
        out[(y * to + x) * 4 + c] = Math.round(
          a * (1 - fx) * (1 - fy) + b * fx * (1 - fy) + cc * (1 - fx) * fy + d * fx * fy
        );
      }
    }
  }
  return out;
}

// -- main -------------------------------------------------------------------------

const rgba = renderIcon();
const iconset = join(tmpdir(), `gp-bar-icon.iconset`);
rmSync(iconset, { recursive: true, force: true });
mkdirSync(iconset, { recursive: true });

const sizes = [
  [16, "icon_16x16.png"],
  [32, "icon_16x16@2x.png"],
  [32, "icon_32x32.png"],
  [64, "icon_32x32@2x.png"],
  [128, "icon_128x128.png"],
  [256, "icon_128x128@2x.png"],
  [256, "icon_256x256.png"],
  [512, "icon_256x256@2x.png"],
  [512, "icon_512x512.png"],
  [1024, "icon_512x512@2x.png"],
];

for (const [size, name] of sizes) {
  const pixels = size === SIZE ? rgba : resize(rgba, SIZE, size);
  writeFileSync(join(iconset, name), encodePng(pixels, size, size));
}

const outDir = new URL("../src-tauri/icons/", import.meta.url).pathname;
mkdirSync(outDir, { recursive: true });
execFileSync("/usr/bin/iconutil", ["-c", "icns", iconset, "-o", join(outDir, "icon.icns")]);

// A plain PNG is required by the Tauri context macro (default window icon).
const png512 = resize(rgba, SIZE, 512);
writeFileSync(join(outDir, "icon.png"), encodePng(png512, 512, 512));
console.log("Wrote src-tauri/icons/icon.icns and icon.png");

// Finalize the release bundle for the privileged-helper flow:
//
//   1. build the `gp-helper` daemon binary (release),
//   2. copy it into `…/GP Bar.app/Contents/MacOS/gp-helper` — where the
//      LaunchDaemon plist written by the app (see src/helper.rs) points —
//      as a Tauri `externalBin` sidecar would have been placed (Tauri's
//      `externalBin` cannot be used here: tauri-build validates those paths
//      at plain `cargo build` time, before the helper exists, which would
//      break `cargo build`/`cargo test`),
//   3. re-sign the WHOLE bundle ad-hoc with the bundle identifier, as the
//      release linker signature does not match it (codesign walks the
//      nested executables, so the helper gets signed too).
//
// Run through `npm run build:bundle`.
import { execSync } from "node:child_process";
import { cpSync } from "node:fs";
import { join } from "node:path";

execSync("cargo build --release --bin gp-helper", {
  cwd: "src-tauri",
  stdio: ["ignore", "ignore", "inherit"],
});

const bundle = join("src-tauri", "target", "release", "bundle", "macos", "GP Bar.app");
const macos = join(bundle, "Contents", "MacOS");
cpSync(join("src-tauri", "target", "release", "gp-helper"), join(macos, "gp-helper"));

execSync(`codesign --force --sign - --identifier dev.gpbar.app "${bundle}"`, {
  stdio: "inherit",
});
console.log(`finalized bundle: ${bundle} (sidecar gp-helper + ad-hoc re-sign)`);
// Finalize the release bundle for the privileged-helper flow:
//
//   1. build the `gp-helper` daemon binary (release),
//   2. copy it into `…/GP Bar S1.app/Contents/MacOS/gp-helper` — where the
//      LaunchDaemon plist points — as a Tauri `externalBin` sidecar would
//      have been placed (Tauri's `externalBin` cannot be used here:
//      tauri-build validates those paths at plain `cargo build` time,
//      before the helper exists, which would break `cargo build`/`cargo
//      test`),
//   3. write the LaunchDaemon plist into
//      `…/Contents/Library/LaunchDaemons/dev.gpbar.helper.plist` — the same
//      content the app used to write at runtime (MachServices,
//      KeepAlive, RunAtLoad false, Standard{Out,Error}Path on the user's
//      gp-bar log dir, ProgramArguments = the in-bundle gp-helper path),
//   4. re-sign the WHOLE bundle ad-hoc with the bundle identifier, as the
//      release linker signature does not match it (codesign walks the
//      nested executables, so the helper gets signed too).
//
// ORDER MATTERS: the plist is written BEFORE the codesign step, so it is
// covered by the bundle signature. BTM refuses to load a LaunchDaemons
// plist that is not (error -67054), and the app must NEVER write (and
// re-sign) it at runtime: a runtime re-sign changes the cdhash at every
// launch — our prime suspect for the BTM quarantina (RECAP.md §13).
//
// Run through `npm run build:bundle`.
import { execSync } from "node:child_process";
import { cpSync, mkdirSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

execSync("cargo build --release --bin gp-helper", {
  cwd: "src-tauri",
  stdio: ["ignore", "ignore", "inherit"],
});

// The bundle path defaults to the freshly built one; pass an explicit path
// to finalize a bundle that was ALREADY copied somewhere stable (the install
// flow: `cp -R` to ~/Applications, then re-run this script with that path).
// The plist's ProgramArguments is absolute, so it must be rewritten against
// the FINAL location — a bundle moved after the plist was baked would point
// launchd at the build tree, and break as soon as `target/` is cleaned.
const bundle = resolve(process.argv[2] ?? join("src-tauri", "target", "release", "bundle", "macos", "GP Bar S1.app"));
const macos = join(bundle, "Contents", "MacOS");
cpSync(join("src-tauri", "target", "release", "gp-helper"), join(macos, "gp-helper"));

const escape = (value) =>
  value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&apos;");

const daemonLabel = "dev.gpbar.helper";
const helper = join(macos, "gp-helper");
const daemonLog = join(homedir(), "Library", "Logs", "gp-bar", "gp-helper.log");
const daemonDir = join(bundle, "Contents", "Library", "LaunchDaemons");
mkdirSync(daemonDir, { recursive: true });
writeFileSync(
  join(daemonDir, `${daemonLabel}.plist`),
  `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
\t<key>Label</key><string>${daemonLabel}</string>
\t<key>ProgramArguments</key>
\t<array><string>${escape(helper)}</string></array>
\t<key>MachServices</key>
\t<dict><key>${daemonLabel}</key><true/></dict>
\t<key>KeepAlive</key><true/>
\t<key>RunAtLoad</key><false/>
\t<key>StandardOutPath</key><string>${escape(daemonLog)}</string>
\t<key>StandardErrorPath</key><string>${escape(daemonLog)}</string>
</dict>
</plist>
`,
);

execSync(`codesign --force --sign - --identifier dev.gpbar.s1 "${bundle}"`, {
  stdio: "inherit",
});
console.log(`finalized bundle: ${bundle} (sidecar gp-helper + LaunchDaemons plist + ad-hoc re-sign)`);
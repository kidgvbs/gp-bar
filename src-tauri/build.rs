fn main() {
  println!("cargo:rustc-env=BUILD_REV={}", build_rev());
  tauri_build::build()
}

/// Short git HEAD hash of this build, baked into EVERY binary of the crate
/// (the app AND the gp-helper daemon: build.rs is per-crate, so both see the
/// same `BUILD_REV`). The client and the daemon handshake on it over XPC:
/// a resident (KeepAlive) daemon from a stale build is asked to exit so
/// launchd re-spawns it from the fresh binary (see `helper::call`).
/// Falls back to the crate version when git is unavailable (e.g. a source
/// tarball build).
fn build_rev() -> String {
  std::process::Command::new("git")
    .args(["rev-parse", "--short", "HEAD"])
    .output()
    .ok()
    .filter(|out| out.status.success())
    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    .filter(|rev| !rev.is_empty())
    .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}
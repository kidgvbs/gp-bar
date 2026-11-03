/// The root-owned admin script both privileged transports use: the sudo
/// path runs it via sudoers, the `gp-helper` daemon execs it as root
/// (Fase 2). Installed by `scripts/setup-admin.sh`. It is the security
/// boundary — validated args, a root-only prefix and launchd discipline
/// all live inside it, so neither transport reimplements any of it.
///
/// Cross-platform on purpose: the macOS-only modules re-export it and
/// `vpn.rs`'s sudo path aliases it from code the Linux CI compiles too.
pub const ADMIN_SCRIPT: &str = "/Library/PrivilegedHelperTools/gp-bar/admin";

#[cfg(target_os = "macos")]
pub mod peer;
#[cfg(target_os = "macos")]
pub mod proto;
#[cfg(target_os = "macos")]
pub mod smapp;
#[cfg(target_os = "macos")]
pub mod xpc;

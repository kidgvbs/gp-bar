//! Admin elevation: `sudo -A`, with Touch ID when the system allows it.
//!
//! sudo authenticates through PAM, and `/etc/pam.d/sudo` includes
//! `/etc/pam.d/sudo_local` FIRST: with `auth sufficient pam_tid.so` enabled
//! there, the admin prompt is Touch ID (or Apple Watch). The askpass helper
//! below only provides the classic password dialog when biometrics are
//! unavailable or fail. This replaces the old
//! `do shell script … with administrator privileges` path, whose dialog
//! ignores PAM entirely (no Touch ID) and whose root children are
//! signal-deaf on this macOS beta (see vpn.rs).

use std::{
  fs,
  io,
  os::unix::fs::PermissionsExt,
  path::PathBuf,
  process::{Command, Output},
};

use crate::config::Config;

/// Quote a single argument for a POSIX shell.
pub fn sh_quote(value: &str) -> String {
  let mut quoted = String::with_capacity(value.len() + 2);
  quoted.push('\'');
  for ch in value.chars() {
    if ch == '\'' {
      // Close the quote, emit an escaped quote, reopen.
      quoted.push_str("'\\''");
    } else {
      quoted.push(ch);
    }
  }
  quoted.push('\'');
  quoted
}

/// The askpass helper sudo invokes when Touch ID is not an option. It runs
/// as the invoking user, shows a native password dialog and prints the
/// answer; canceling the dialog exits non-zero, which sudo reports as a
/// canceled authentication.
const ASKPASS_SCRIPT: &str = r#"#!/bin/sh
# GP Bar sudo askpass: password fallback when Touch ID is unavailable.
exec /usr/bin/osascript -e 'display dialog "GP Bar needs your administrator password to manage the VPN tunnel:" default answer "" with hidden answer' -e 'text returned of result'
"#;

/// Where the askpass helper lives (user-owned, written once).
fn askpass_path() -> PathBuf {
  Config::config_dir().join("sudo-askpass.sh")
}

/// Make sure the askpass helper exists and is executable; return its path.
fn ensure_askpass_helper() -> io::Result<PathBuf> {
  let path = askpass_path();
  if let Some(dir) = path.parent() {
    fs::create_dir_all(dir)?;
  }
  fs::write(&path, ASKPASS_SCRIPT)?;
  fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
  Ok(path)
}

/// Run a shell script as root through `sudo -A`, blocking until it exits.
///
/// Returns the command output. A non-zero exit does not panic; the caller
/// inspects `Output`. The script text goes into a user-owned 0600 file
/// instead of the command line, where `ps` would expose it; the same
/// local-TOCTOU caveat as the staged launchd plist applies (single-user
/// machines: irrelevant). The user can cancel the Touch ID/password
/// prompt, in which case sudo exits non-zero with "no password was
/// provided" and the fallback in `run_admin_script` maps it to a cancel.
pub fn run_as_admin(script: &str) -> io::Result<Output> {
  let helper = ensure_askpass_helper()?;
  let dir = Config::log_dir();
  fs::create_dir_all(&dir)?;
  let script_path = dir.join("admin.sh");
  fs::write(&script_path, script)?;
  fs::set_permissions(&script_path, fs::Permissions::from_mode(0o600))?;
  Command::new("/usr/bin/sudo")
    .env("SUDO_ASKPASS", helper)
    .arg("-A")
    .arg("/bin/sh")
    .arg(&script_path)
    .output()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn quotes_shell_metacharacters() {
    assert_eq!(sh_quote("gp.imm.re"), "'gp.imm.re'");
    assert_eq!(sh_quote("it's"), r#"'it'\''s'"#);
    assert_eq!(sh_quote("a b;rm -rf /"), "'a b;rm -rf /'");
  }

  #[test]
  fn askpass_is_a_hidden_password_dialog() {
    assert!(ASKPASS_SCRIPT.trim_start().starts_with("#!/bin/sh"));
    assert!(ASKPASS_SCRIPT.contains("with hidden answer"));
    // No double quotes inside the single-quoted AppleScript literal would
    // break; the dialog text must not contain any.
    assert!(ASKPASS_SCRIPT.lines().count() == 3);
  }
}

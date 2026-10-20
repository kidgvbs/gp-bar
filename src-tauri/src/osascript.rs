//! Running shell commands with administrator privileges through
//! `osascript`, which shows the native macOS password dialog.

use std::{
  io,
  process::{Command, Output},
};

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

/// Escape a shell script for embedding inside an AppleScript string literal
/// (`do shell script "…"`).
pub fn applescript_escape(script: &str) -> String {
  script.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Run a shell command as root, blocking until it finishes.
///
/// Returns the command output. A non-zero exit does not panic; the caller
/// inspects `Output`. The user can cancel the password dialog, in which case
/// osascript exits with an error like "User canceled.".
pub fn run_as_admin(script: &str) -> io::Result<Output> {
  let applescript = format!(
    "do shell script \"{}\" with administrator privileges",
    applescript_escape(script)
  );
  Command::new("/usr/bin/osascript")
    .arg("-e")
    .arg(applescript)
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
  fn escapes_applescript_string_literals() {
    assert_eq!(applescript_escape("say \"hi\""), "say \\\"hi\\\"");
    assert_eq!(applescript_escape("a\\b"), "a\\\\b");
  }
}

//! The daemon-side peer policy: accept only the uid that owns the app bundle.
//!
//! This is the minimal-but-real peer check the real Fase-2 implementation
//! will grow from (review B2: at minimum, verify the peer's euid). The
//! decision is a pure function; the inputs are:
//!
//! * the peer's euid, read with `xpc_connection_get_euid` at accept time;
//! * the uid of the `.app` bundle directory, stat-ed by the daemon at startup
//!   from its own executable path (root can stat anything; the metadata of
//!   the bundle is the identity of whoever installed the probe).
//!
//! If the stat fails the daemon refuses **everyone**: a check that cannot
//! establish its policy fails closed.
//!
//! Residual limits, accepted and documented: this is **parity with a
//! sudoers rule for that user, not stronger**. Any process running as the
//! bundle owner can ping; the check does not bind to a code-signing
//! requirement (impossible with ad-hoc signing, no Team ID) nor per-request
//! authentication. The daemon performs no privileged operation beyond
//! answering `ping`, so the blast radius of the residual risk is nil —
//! but the *mechanism* (read peer euid, compare to recorded owner, refuse
//! the rest) is exactly what needs validating for the real daemon.

use std::path::{Path, PathBuf};

/// The verdict for one connection: does the peer's euid match the bundle
/// owner's uid?
pub fn peer_allowed(peer_euid: u32, bundle_owner_uid: Option<u32>) -> bool {
  // `None` = we could not stat the bundle (or the binary is not running
  // from a bundle): refuse all, fail closed.
  bundle_owner_uid == Some(peer_euid)
}

/// `<bundle>/Contents/MacOS/<exe>` -> `<bundle>`, or `None` when the
/// executable does not live in the standard bundle layout.
pub fn bundle_root(exe: &Path) -> Option<&Path> {
  // exe = <bundle>/Contents/MacOS/<name>
  let macos = exe.parent()?;
  if macos.file_name()? != "MacOS" {
    return None;
  }
  let contents = macos.parent()?;
  if contents.file_name()? != "Contents" {
    return None;
  }
  contents.parent()
}

/// Resolve the bundle root for the currently running daemon binary.
pub fn current_bundle_root() -> Option<PathBuf> {
  let exe = std::env::current_exe().ok()?;
  bundle_root(&exe).map(Path::to_path_buf)
}

/// Uid of the `.app` bundle directory owner, or `None` when the stat fails
/// (which makes the daemon refuse all peers).
pub fn bundle_owner_uid(bundle_root: &Path) -> Option<u32> {
  use std::os::unix::fs::MetadataExt;
  std::fs::metadata(bundle_root).ok().map(|m| m.uid())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_owner_is_allowed_and_nobody_else() {
    assert!(peer_allowed(501, Some(501)));
    assert!(!peer_allowed(502, Some(501)));
    assert!(!peer_allowed(501, Some(502)));
  }

  #[test]
  fn root_peers_are_refused_unless_root_owns_the_bundle() {
    // Root does not need this daemon; an unexpected root peer is exactly
    // the kind of thing a probe should notice.
    assert!(!peer_allowed(0, Some(501)));
    // But the rule is "peer == owner", not "peer != 0".
    assert!(peer_allowed(0, Some(0)));
  }

  #[test]
  fn when_the_owner_is_unknown_everybody_is_refused() {
    // Fail closed: no stat, no policy, no peers.
    assert!(!peer_allowed(501, None));
    assert!(!peer_allowed(0, None));
  }

  #[test]
  fn bundle_root_strips_the_standard_layout() {
    assert_eq!(
      bundle_root(Path::new(
        "/Users/x/Applications/SMProbe.app/Contents/MacOS/smprobe-helper"
      )),
      Some(Path::new("/Users/x/Applications/SMProbe.app"))
    );
  }

  #[test]
  fn non_bundle_layouts_are_rejected() {
    assert_eq!(
      bundle_root(Path::new("/usr/local/bin/smprobe-helper")),
      None
    );
    assert_eq!(
      bundle_root(Path::new(
        "/x/SMProbe.app/Contents/Resources/smprobe-helper"
      )),
      None
    );
    assert_eq!(
      bundle_root(Path::new("/x/SMProbe.app/Other/MacOS/smprobe-helper")),
      None
    );
    assert_eq!(
      bundle_root(Path::new("/x/SMProbe.app/Contents/MacOS")),
      None
    );
    assert_eq!(bundle_root(Path::new("smprobe-helper")), None);
  }
}

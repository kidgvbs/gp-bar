#!/bin/sh
# One-time setup for GP Bar's privileged helper: installs the fixed-operation
# wrapper root-owned and grants passwordless sudo on that exact path only.
#
# Run as the logged-in admin user (Terminal will offer Touch ID):
#   sudo bash "/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh"
#
# Re-running after an app update refreshes both files (idempotent).
set -eu

if [ "$(id -u)" != 0 ]; then
  echo "setup-admin: run me with sudo" >&2
  exit 1
fi
if [ -z "${SUDO_USER:-}" ]; then
  echo "setup-admin: SUDO_USER is not set; run me via sudo from your account" >&2
  exit 1
fi

SRC="$(cd "$(dirname "$0")" && pwd)"
DEST_DIR=/usr/local/lib/gp-bar
DEST="$DEST_DIR/admin"

mkdir -p "$DEST_DIR"
install -m 0755 -o root -g wheel "$SRC/gp-admin" "$DEST"

SUDOERS=/etc/sudoers.d/gp-bar
# NOPASSWD on this exact path only; the wrapper's fixed operations are the
# security boundary (see gp-admin header).
echo "${SUDO_USER} ALL=(root) NOPASSWD: $DEST" > "$SUDOERS"
chmod 0440 "$SUDOERS"
if ! visudo -c >/dev/null; then
  echo "setup-admin: sudoers syntax check failed, rolling back" >&2
  rm -f "$SUDOERS"
  exit 1
fi

# Prove the whole chain before declaring success: root drops to SUDO_USER,
# the user's sudo hits the new NOPASSWD rule, the wrapper runs its selftest.
if ! sudo -u "$SUDO_USER" sudo -n "$DEST" selftest; then
  echo "setup-admin: end-to-end selftest failed" >&2
  exit 1
fi

echo "GP Bar helper installed: $DEST"
echo "Passwordless, fixed operations only (start/stop/cancel the VPN)."

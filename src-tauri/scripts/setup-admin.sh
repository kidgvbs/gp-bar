#!/bin/sh
# One-time setup for GP Bar's privileged helper: installs the fixed-operation
# wrapper AND the worker binaries it executes, all root-owned under
# /Library/PrivilegedHelperTools/gp-bar, and grants passwordless sudo on the
# wrapper path only.
#
# Run as the logged-in admin user (Terminal will offer Touch ID):
#   sudo bash "/Applications/GP Bar.app/Contents/Resources/scripts/setup-admin.sh"
#
# Re-running after an app update OR after a `brew upgrade` of gpclient /
# an update of GP Connect (the binary source) refreshes all copies
# (idempotent). It also migrates away the pre-v1.1 install under
# /usr/local/lib/gp-bar.

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
DEST_DIR=/Library/PrivilegedHelperTools/gp-bar
DEST="$DEST_DIR/admin"
OLD_DEST_DIR=/usr/local/lib/gp-bar
LOG_DIR=/Library/Logs/gp-bar

# S3: the sudoers grant is only as sound as EVERY directory along the wrapper
# path. On Intel Macs Homebrew chowns /usr/local (and its lib) to the user,
# which is exactly why the install moved to the system-owned
# /Library/PrivilegedHelperTools. Fail loudly if any parent is not
# root-owned or is group/world-writable instead of installing silently.
d="$DEST_DIR"
while [ "$d" != / ]; do
  if [ ! -e "$d" ]; then
    d=$(dirname "$d")
    continue
  fi
  [ "$(stat -f %u "$d")" = 0 ] || { echo "setup-admin: $d is not root-owned" >&2; exit 1; }
  case "$(stat -f %Sp "$d")" in
    ?????w????|????????w?) echo "setup-admin: $d is group/world-writable" >&2; exit 1 ;;
  esac
  d=$(dirname "$d")
done

install -d -m 0755 "$DEST_DIR"
install -m 0755 -o root -g wheel "$SRC/gp-admin" "$DEST"

# S2: close the Homebrew hole. The root exec chain must never touch a
# user-writable executable: /opt/homebrew/bin is the user's, and today's
# gpclient there is a symlink into the GP Connect.app bundle (user-writable
# too). Copy gpclient and the @executable_path Frameworks dylibs it links
# into the root-owned prefix; copy the "GP Connect Login.app" bundle for
# gpauth (see the comment below — it must exec in bundle context).
# gp-admin resolves binaries ONLY from here (see its header).
SRC_GPCLIENT=''
for candidate in /opt/homebrew/bin/gpclient /usr/local/bin/gpclient; do
  if [ -x "$candidate" ]; then
    SRC_GPCLIENT=$candidate
    break
  fi
done
[ -n "$SRC_GPCLIENT" ] || { echo "setup-admin: gpclient not found (brew install gpclient / install GP Connect)" >&2; exit 1; }
REAL_GPCLIENT=$(realpath "$SRC_GPCLIENT")
REAL_DIR=$(dirname "$REAL_GPCLIENT")

install -d -m 0755 "$DEST_DIR/bin"
install -m 0755 -o root -g wheel "$REAL_GPCLIENT" "$DEST_DIR/bin/gpclient"
# The SAML drop-in helper (gpauth) is Hardened Runtime: its code signature
# validates against its BUNDLE context (Info.plist + _CodeSignature), and
# the kernel kills such a binary at exec when the context is broken — a
# bare Mach-O copy dies instantly (SIGKILL). So the whole
# "GP Connect Login.app" bundle is copied root-owned (ditto preserves the
# bundle layout, modes and signature), and $PREFIX/bin/gpauth becomes a
# RELATIVE symlink into it: gpclient resolves gpauth as a sibling of its
# own realpath'd executable, and the symlink keeps that discovery working
# while the kernel execs the binary in its bundle context.
LOGIN_APP_DST="$DEST_DIR/GP Connect Login.app"
LOGIN_APP_SRC="$REAL_DIR/GP Connect Login.app"
if [ ! -d "$LOGIN_APP_SRC" ]; then
  echo "setup-admin: $LOGIN_APP_SRC not found (SAML browser auth needs the GP Connect Login bundle)" >&2
  exit 1
fi
rm -rf "$LOGIN_APP_DST"
ditto "$LOGIN_APP_SRC" "$LOGIN_APP_DST"
chown -R root:wheel "$LOGIN_APP_DST"
# Root-owned, non-writable by anyone else: dirs 0755, files keep their
# executable bits and lose group/world write. (Mode changes do not touch
# the code signature; chown/chmod here are the same values the bundle
# ships with, enforced anyway.)
find "$LOGIN_APP_DST" -type d -exec chmod 0755 {} +
find "$LOGIN_APP_DST" -type f -exec chmod go-w {} +
# ln -sf also replaces a stale bare gpauth file from an earlier install.
ln -sfn '../GP Connect Login.app/Contents/MacOS/gpauth' "$DEST_DIR/bin/gpauth"
# gpclient links @executable_path/../Frameworks/*.dylib (libopenconnect,
# libgnutls and their transitive closure): a layout-preserving copy keeps
# those relative load commands valid, so no install_name_tool rewriting is
# needed and the whole dylib closure becomes root-owned.
FW_SRC="$REAL_DIR/../Frameworks"
if [ -d "$FW_SRC" ]; then
  install -d -m 0755 "$DEST_DIR/Frameworks"
  for dylib in "$FW_SRC"/*.dylib; do
    [ -e "$dylib" ] || continue
    install -m 0755 -o root -g wheel "$dylib" "$DEST_DIR/Frameworks/"
  done
fi

# S1: the connect log lives in a root-owned, world-readable directory; the
# wrapper creates the file per attempt (0644). Never under ~/Library.
install -d -m 0755 -o root -g wheel "$LOG_DIR"

# S7: write the sudoers file atomically. A dotted temp name is inert (sudo
# ignores files containing a dot in sudoers.d), so a malformed file can never
# be live; validate THE FILE ALONE with visudo -cf, then rename into place.
SUDOERS_DIR=/etc/sudoers.d
tmp=$(mktemp "$SUDOERS_DIR/.gp-bar.XXXXXX")
trap 'rm -f "$tmp"' EXIT
# NOPASSWD on this exact path only; the wrapper's fixed operations are the
# security boundary (see the gp-admin header).
printf '%s ALL=(root) NOPASSWD: %s\n' "$SUDO_USER" "$DEST" > "$tmp"
chmod 0440 "$tmp"
if ! visudo -cf "$tmp"; then
  echo "setup-admin: sudoers syntax check failed; the previous grant (if any) is untouched" >&2
  exit 1
fi
mv -f "$tmp" "$SUDOERS_DIR/gp-bar"

# Migration from the pre-v1.1 install (S3): the old wrapper lived under
# /usr/local/lib, whose parents Homebrew may chown to the user on Intel
# Macs. Remove it so no stale, potentially user-replaceable copy remains.
if [ "$OLD_DEST_DIR" != "$DEST_DIR" ] && [ -d "$OLD_DEST_DIR" ]; then
  rm -rf "$OLD_DEST_DIR"
  echo "setup-admin: removed the old install at $OLD_DEST_DIR"
fi

# Prove the whole chain before declaring success: root drops to SUDO_USER,
# the user's sudo hits the new NOPASSWD rule, the wrapper runs its selftest
# (which re-checks every binary's ownership chain).
if ! sudo -u "$SUDO_USER" sudo -n "$DEST" selftest; then
  echo "setup-admin: end-to-end selftest failed" >&2
  exit 1
fi

echo "GP Bar helper installed: $DEST"
echo "Worker binaries (root-owned): $DEST_DIR/bin/gpclient"
echo "Passwordless, fixed operations only (start/stop/cancel/repair the VPN)."
echo "NOTE: after a 'brew upgrade' of gpclient (or a GP Connect update) re-run"
echo "this script to refresh the root-owned copies under $DEST_DIR."
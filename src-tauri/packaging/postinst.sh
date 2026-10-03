#!/bin/sh
# Push-to-talk reads key state from /dev/input, which requires the 'input' group.
set -e
if [ -n "$SUDO_USER" ] && [ "$SUDO_USER" != root ]; then
  usermod -aG input "$SUDO_USER" || true
elif [ -n "$PKEXEC_UID" ]; then
  user=$(getent passwd "$PKEXEC_UID" | cut -d: -f1)
  [ -n "$user" ] && usermod -aG input "$user" || true
fi
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q /usr/share/icons/hicolor || true

#!/bin/sh
# Push-to-talk reads key state from /dev/input. The shipped udev rule grants the
# active logind session read access via uaccess; the 'input' group is the fallback.
set -e
udevadm control --reload-rules 2>/dev/null || true
udevadm trigger --subsystem-match=input 2>/dev/null || true
if [ -n "$SUDO_USER" ] && [ "$SUDO_USER" != root ]; then
  usermod -aG input "$SUDO_USER" || true
elif [ -n "$PKEXEC_UID" ]; then
  user=$(getent passwd "$PKEXEC_UID" | cut -d: -f1)
  [ -n "$user" ] && usermod -aG input "$user" || true
fi
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q /usr/share/icons/hicolor || true

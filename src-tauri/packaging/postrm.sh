#!/bin/sh
# Per-user files (~/.config/voice-prompt, ~/.local/share/voice-prompt, the keyring entry) stay;
# the per-user autostart entry has TryExec, so it is inert once the binary is gone.
set -e
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q /usr/share/icons/hicolor || true

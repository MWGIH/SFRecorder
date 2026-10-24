#!/usr/bin/env bash
# Removes SFRecorder: the dashboard tab and its service, the recorder, its
# settings, logs and the downloaded copy the install line made. Recordings in
# ~/Videos/SFRecorder are kept. Also run by the tab's Uninstall button.
set -uo pipefail

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/sfrecorder"
UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/sfrecorder.service"

# Read before the settings folder goes.
SOURCE="$(cat "$CONFIG_DIR/source_dir" 2>/dev/null || true)"

echo "==> Stopping the SFRecorder tab"
pkill -INT -x sfrecorder 2>/dev/null || true   # a recording still running saves what it has
systemctl --user disable --now sfrecorder.service 2>/dev/null || true
rm -f "$UNIT"
systemctl --user daemon-reload 2>/dev/null || true

echo "==> Removing the recorder and its settings"
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/sfrecorder" "$HOME/.local/lib/sfrecorder" \
       "$CONFIG_DIR" "$HOME/.local/bin/sfrecorder" \
       "${XDG_CACHE_HOME:-$HOME/.cache}/sfrecorder" "${XDG_RUNTIME_DIR:-/tmp}/sfrecorder"

# The folder the install line cloned, unless it holds changes of your own.
if [ -n "$SOURCE" ] && [ -d "$SOURCE/.git" ] && [ -f "$SOURCE/install.sh" ] && [ -f "$SOURCE/uninstall.sh" ]; then
    if [ -z "$(git -C "$SOURCE" status --porcelain 2>/dev/null)" ]; then
        echo "==> Removing the downloaded copy in $SOURCE"
        rm -rf "$SOURCE"
    else
        echo "==> Kept $SOURCE: it has changes of your own"
    fi
fi

echo "SFRecorder removed. Your recordings in ~/Videos/SFRecorder are still there."

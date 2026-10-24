#!/usr/bin/env bash
# Removes SFRecorder and its settings. Recordings in ~/Videos/SFRecorder are kept.
set -euo pipefail

UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/sfrecorder.service"

systemctl --user disable --now sfrecorder.service 2>/dev/null || true
rm -f "$UNIT"
systemctl --user daemon-reload
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/sfrecorder" "$HOME/.local/lib/sfrecorder" \
       "${XDG_CONFIG_HOME:-$HOME/.config}/sfrecorder" "$HOME/.local/bin/sfrecorder"

echo "SFRecorder removed. Your recordings in ~/Videos/SFRecorder are still there."

#!/usr/bin/env bash
# Tests whether SteamVR's mirror image can replace panel capture (no password
# needed). Run it on the headset while SteamVR is up, with a game running and
# the FPS overlay and a Steam overlay (e.g. the dashboard) visible. Installs
# nothing: everything goes in a temporary folder.
set -euo pipefail

TAG="${SFRECORDER_TAG:-test}"
RELEASES="https://github.com/MWGIH/SFRecorder/releases/download/$TAG"
OPENVR_URL="https://raw.githubusercontent.com/ValveSoftware/openvr/v2.12.14/bin/linuxarm64/libopenvr_api.so"

DIR="$(mktemp -d)"
trap 'rm -rf "$DIR"' EXIT
curl -fsSL "$RELEASES/sfrecorder-$(uname -m).tar.gz" | tar -xz -C "$DIR"
curl -fsSL "$RELEASES/ffmpeg-libs-$(uname -m).tar.gz" | tar -xz -C "$DIR"
curl -fsSL -o "$DIR/libopenvr_api.so" "$OPENVR_URL"

mkdir -p "$HOME/Videos/SFRecorder"
echo "Taking one picture per eye in 5 seconds..."
sleep 5
LD_LIBRARY_PATH="$DIR" "$DIR/sfrecorder" --probe-mirror

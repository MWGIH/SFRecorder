#!/usr/bin/env bash
# Installs (or updates) SFRecorder for the current user. Everything else is in
# the "SFRecorder" tab of the SteamVR dashboard once installed.
set -euo pipefail

PYOPENVR_VERSION="2.12.1401"
OPENVR_TAG="v2.12.14"   # matches the pyopenvr version above
# SFRECORDER_TAG=test ./install.sh installs a test build instead of the latest release.
if [ -n "${SFRECORDER_TAG:-}" ]; then
    RELEASES="https://github.com/MWGIH/SFRecorder/releases/download/$SFRECORDER_TAG"
else
    RELEASES="https://github.com/MWGIH/SFRecorder/releases/latest/download"
fi

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHARE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/sfrecorder"
LIB_DIR="$HOME/.local/lib/sfrecorder"
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
VENV="$SHARE_DIR/venv"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/sfrecorder"
HELPER="$LIB_DIR/sfrecorder-grab"

case "$(uname -m)" in
    aarch64) OPENVR_ARCH="linuxarm64" ;;
    x86_64)  OPENVR_ARCH="linux64" ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$SHARE_DIR" "$LIB_DIR" "$BIN_DIR" "$UNIT_DIR" "$CONFIG_DIR" "$HOME/Videos/SFRecorder"
# Lets the tab's Update button find this checkout and the build it came from.
echo "$SRC_DIR" > "$CONFIG_DIR/source_dir"
echo "${SFRECORDER_TAG:-}" > "$CONFIG_DIR/release_tag"

echo "==> Installing the recorder"
if [ -x "$SRC_DIR/target/release/sfrecorder" ]; then
    BUILD="$SRC_DIR/target/release"
else
    BUILD="$(mktemp -d)"
    trap 'rm -rf "$BUILD"' EXIT
    curl -fsSL "$RELEASES/sfrecorder-$(uname -m).tar.gz" | tar -xz -C "$BUILD"
fi
install -m 755 "$BUILD/sfrecorder" "$BIN_DIR/sfrecorder"
# Rewriting the helper would drop its unlock, so it's only replaced when its
# version changed (every release rebuilds it, so its bytes always differ);
# most updates then need no password.
helper_version() { "$1" --version 2>/dev/null || true; }
NEW_HELPER_VERSION="$(helper_version "$BUILD/sfrecorder-grab")"
if [ -f "$HELPER" ] && [ -n "$NEW_HELPER_VERSION" ] && [ "$(helper_version "$HELPER")" = "$NEW_HELPER_VERSION" ]; then
    HELPER_CHANGED=0
else
    install -m 755 "$BUILD/sfrecorder-grab" "$HELPER"
    HELPER_CHANGED=1
fi
# FFmpeg (for the MP4 file and AAC audio): use SteamOS's own if the recorder
# can load it, otherwise a small private copy kept with the recorder. SteamOS's
# system folders are read-only and reset by updates, so nothing goes there.
missing_libs() { ldd "$BIN_DIR/sfrecorder" | awk '/not found/ {print $1}'; }
if missing_libs | grep -qE '^lib(avcodec|avformat|avutil|swresample)\.'; then
    echo "==> FFmpeg not found, adding a private copy"
    if [ -f "$SRC_DIR/ffmpeg-libs-$(uname -m).tar.gz" ]; then
        tar -xzf "$SRC_DIR/ffmpeg-libs-$(uname -m).tar.gz" -C "$LIB_DIR"
    else
        curl -fsSL "$RELEASES/ffmpeg-libs-$(uname -m).tar.gz" | tar -xz -C "$LIB_DIR"
    fi
else
    echo "==> FFmpeg found"
fi
if [ -n "$(missing_libs)" ]; then
    echo "The recorder can't find these libraries: $(missing_libs | tr '\n' ' ')" >&2
    exit 1
fi
install -m 755 "$SRC_DIR/tab/sfrecorder_tab.py" "$SHARE_DIR/sfrecorder_tab.py"
install -m 755 "$SRC_DIR/uninstall.sh" "$SHARE_DIR/uninstall.sh"

echo "==> Setting up the dashboard tab"
[ -x "$VENV/bin/python" ] || python3 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --upgrade pip
"$VENV/bin/pip" install --quiet "openvr==$PYOPENVR_VERSION" "pillow>=10.1"

# Valve's own OpenVR loader for this CPU: for the tab (pyopenvr only bundles
# x86_64) and for the recorder, which reads the lens data through it.
echo "==> Fetching Valve's OpenVR loader ($OPENVR_ARCH)"
curl -fsSL -o "$LIB_DIR/libopenvr_api.so" \
    "https://raw.githubusercontent.com/ValveSoftware/openvr/$OPENVR_TAG/bin/$OPENVR_ARCH/libopenvr_api.so"
PKG_DIR="$("$VENV/bin/python" -c 'import importlib.util, os; print(os.path.dirname(importlib.util.find_spec("openvr").origin))')"
cp "$LIB_DIR/libopenvr_api.so" "$PKG_DIR/libopenvr_api_64.so"

# Reading what the panels show needs one capability, given only to the small
# helper that hands the recorder the on-screen buffer.
if [ "$HELPER_CHANGED" = 0 ] && getcap "$HELPER" 2>/dev/null | grep -q cap_sys_admin; then
    echo "==> Panels already unlocked"
    rm -f "$CONFIG_DIR/needs_unlock"
elif sudo -n setcap cap_sys_admin+ep "$HELPER" 2>/dev/null; then
    # sudo needs no password here (as on the Frame), so this also works when
    # updating from the tab, with no terminal.
    echo "==> Panels unlocked"
    rm -f "$CONFIG_DIR/needs_unlock"
elif [ -t 0 ]; then
    echo "==> Unlocking the panels (asks for your password)"
    if ! sudo setcap cap_sys_admin+ep "$HELPER"; then
        echo "Couldn't unlock the panels. Set a password with 'passwd' and run this again." >&2
        exit 1
    fi
    rm -f "$CONFIG_DIR/needs_unlock"
else
    # Updating from the tab: no terminal to ask for the password in.
    echo "==> The panels need unlocking again: run the install line in a terminal"
    touch "$CONFIG_DIR/needs_unlock"
fi

echo "==> Installing background service"
cat > "$UNIT_DIR/sfrecorder.service" <<UNIT
[Unit]
Description=SFRecorder (SteamVR dashboard tab)

[Service]
ExecStart=$VENV/bin/python $SHARE_DIR/sfrecorder_tab.py
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
UNIT
systemctl --user daemon-reload
systemctl --user enable sfrecorder.service
systemctl --user restart sfrecorder.service

echo
echo "Done. Open the SteamVR dashboard and pick the 'SFRecorder' tab."
echo "Recordings go to ~/Videos/SFRecorder."

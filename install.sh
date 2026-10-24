#!/usr/bin/env bash
# Installs (or updates) SFRecorder for the current user. Everything else is in
# the "SFRecorder" tab of the SteamVR dashboard once installed.
set -euo pipefail

PYOPENVR_VERSION="2.12.1401"
OPENVR_TAG="v2.12.14"   # matches the pyopenvr version above
RELEASES="https://github.com/MWGIH/SFRecorder/releases/latest/download"

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHARE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/sfrecorder"
LIB_DIR="$HOME/.local/lib/sfrecorder"
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
VENV="$SHARE_DIR/venv"

case "$(uname -m)" in
    aarch64) OPENVR_ARCH="linuxarm64" ;;
    x86_64)  OPENVR_ARCH="linux64" ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$SHARE_DIR" "$LIB_DIR" "$BIN_DIR" "$UNIT_DIR" "$HOME/Videos/SFRecorder"

echo "==> Installing the recorder"
if [ -x "$SRC_DIR/target/release/sfrecorder" ]; then
    BUILD="$SRC_DIR/target/release"
else
    BUILD="$(mktemp -d)"
    trap 'rm -rf "$BUILD"' EXIT
    curl -fsSL "$RELEASES/sfrecorder-$(uname -m).tar.gz" | tar -xz -C "$BUILD"
fi
install -m 755 "$BUILD/sfrecorder" "$BIN_DIR/sfrecorder"
install -m 755 "$BUILD/sfrecorder-grab" "$LIB_DIR/sfrecorder-grab"
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
echo "==> Unlocking the panels (asks for your password)"
if ! sudo setcap cap_sys_admin+ep "$LIB_DIR/sfrecorder-grab"; then
    echo "Couldn't unlock the panels. Set a password with 'passwd' and run this again." >&2
    exit 1
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

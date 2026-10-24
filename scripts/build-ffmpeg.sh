#!/usr/bin/env bash
# Builds the small private FFmpeg that install.sh falls back to when SteamOS
# doesn't have FFmpeg libraries the recorder can load. Only what SFRecorder
# uses is built: the AAC encoder and the MP4 writer.
#
#   scripts/build-ffmpeg.sh            -> ffmpeg-libs-<arch>.tar.gz
#
# Build the release engine against this FFmpeg too, so its library versions
# match the bundle:
#   PKG_CONFIG_PATH=build/ffmpeg/lib/pkgconfig cargo build --release
set -euo pipefail

FFMPEG_VERSION="7.1.1"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$ROOT/build/ffmpeg-src"
PREFIX="$ROOT/build/ffmpeg"
ARCH="$(uname -m)"

mkdir -p "$WORK"
if [ ! -x "$WORK/configure" ]; then
    curl -fsSL "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" | tar -xJ -C "$WORK" --strip-components=1
fi

cd "$WORK"
./configure \
    --prefix="$PREFIX" \
    --enable-shared --disable-static \
    --disable-programs --disable-doc --disable-network --disable-autodetect \
    --disable-everything \
    --disable-avdevice --disable-avfilter --disable-swscale --disable-postproc \
    --enable-encoder=aac \
    --enable-muxer=mp4 \
    --enable-protocol=file
make -j"$(nproc)"
make install

# Only the shared libraries go in the bundle, under their versioned names.
tar -czf "$ROOT/ffmpeg-libs-$ARCH.tar.gz" -C "$PREFIX/lib" \
    $(cd "$PREFIX/lib" && ls lib{avcodec,avformat,avutil,swresample}.so.*)
echo "Built $ROOT/ffmpeg-libs-$ARCH.tar.gz"

# SFRecorder

Records what the Steam Frame's panels show, Steam overlays included, with as
little cost to the game as possible. Start and stop it from its own tab in the
SteamVR dashboard.

> **Status: first build, not yet tested on a Steam Frame.** The pieces compile
> and the MP4 writer is tested, but the capture, framing and encoder still need
> checking on the headset (see [To check on the headset](#to-check-on-the-headset)).

## Install

```sh
cd ~ && (git clone https://github.com/MWGIH/SFRecorder.git || git -C SFRecorder pull) && SFRecorder/install.sh
```

SFrameOverlay (the FPS HUD) is a separate app with its own repo. To get its HUD
in recordings, install it from there:

```sh
cd ~ && (git clone https://github.com/MWGIH/SFrameOverlay.git || git -C SFrameOverlay pull) && SFrameOverlay/install.sh
```

The installer asks for your password once, to let the recorder read the panels.
To remove it: `~/.local/share/sfrecorder/uninstall.sh` (recordings are kept).

## The SFRecorder tab

| Option | What it does |
| --- | --- |
| **Record / Stop** | Starts and stops a recording. Recording pauses on its own while this tab is open, so the menu never ends up in the video. |
| **Shape** | **16:9** (default) frames one eye like Steam's own recorder. **Raw** records both panels exactly as shown; it asks you to confirm the first time and then shows ⚠ next to Shape. |
| **Eye** | Left or right for 16:9. It follows the eye picked in SteamVR's VR View window until you change it here; the **i** explains where that setting is. |
| **Microphone** | Off by default. When on, your voice is mixed into the game audio. |
| **FPS overlay** | Shows whether [SFrameOverlay](https://github.com/MWGIH/SFrameOverlay) is installed and on. When it is, choose whether its HUD is in recordings. Hiding it also hides it in the headset while recording, because the panels are recorded as shown. |

Game audio is always recorded. Recordings go to `~/Videos/SFRecorder` as
`SFRecorder_<date>_<time>.mp4` (HEVC + AAC). A file is only given its `.mp4`
name once it's finished; until then it's `.mp4.part`. Use
[FrameCopy](https://github.com/MWGIH/FrameCopy) to copy them to another device.

## How it works

```
panel scanout ──dmabuf──▶ one compute shader ──NV12──▶ hardware HEVC encoder ──▶ MP4
                          (crop + un-warp)              (V4L2)                    ▲
game audio (+ mic) ──────────────────────────────────────────────▶ AAC ───────────┘
```

- The compositor's on-screen buffer is borrowed, never copied. A tiny helper
  (`sfrecorder-grab`, the only part with extra rights) hands it over, and only
  for buffers that are on screen right now.
- One compute pass per frame reads it, removes the lens warp for 16:9 using
  SteamVR's own lens data (worked out once at start), and writes NV12 straight
  into the encoder's input buffer. The GPU queue runs at low priority so the
  game always goes first.
- No pixels touch the CPU. The recorder only wakes on vblank.
- While recording, SFRecorder writes `$XDG_RUNTIME_DIR/sfrecorder/view.json`
  with the recorded view; SFrameOverlay uses it to keep its HUD inside the video
  (or hide it).

## Building

Needs Rust, `glslangValidator`, clang, and the headers for Vulkan, libdrm,
FFmpeg (libavcodec/libavformat) and libpulse.

```sh
cargo build --release
```

`install.sh` uses `target/release` when it exists, otherwise the latest release.

### FFmpeg

The recorder uses FFmpeg's libraries for the MP4 file and AAC audio. The
installer checks whether the recorder can load SteamOS's own copy. If it can't
(not installed, or a different version), it adds a small private copy to
`~/.local/lib/sfrecorder`, which the recorder looks in first. SteamOS's system
folders are read-only and reset by updates, so the installer never puts
anything there.

`scripts/build-ffmpeg.sh` builds that private copy (only the AAC encoder and
MP4 writer). Build release binaries against it so the versions match:

```sh
scripts/build-ffmpeg.sh
PKG_CONFIG_PATH=build/ffmpeg/lib/pkgconfig cargo build --release
```

### Releases

`.github/workflows/release.yml` builds the engine and the private FFmpeg for
the Frame (aarch64) on GitHub and attaches both to a release. Push a `v*` tag
for a release, or a tag like `v0.1.0-test1` for a test pre-release. To install
a test build:

```sh
SFRECORDER_TAG=v0.1.0-test1 SFRecorder/install.sh
```

## To check on the headset

- The 16:9 crop matches Steam's recorder, and the eyes are the right way up.
- The encoder's input layout (NV12 vs NV12M, padded rows) and its largest size
  (Raw asks for 3840×1920).
- Whether SteamOS for the Frame has FFmpeg libraries the recorder can use, or
  the private copy gets installed.

## License

GPL-3.0-only. See [LICENSE](LICENSE).

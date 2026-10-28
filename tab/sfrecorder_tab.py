#!/usr/bin/env python3
"""
SFRecorder's tab in the SteamVR dashboard: start and stop recordings and pick
how they look. The recording itself is done by the `sfrecorder` engine, which
this tab starts and talks to over stdin/stdout.

Recording pauses on its own while this tab is open, so the menu never ends up
in the video.
"""

import configparser
import ctypes
import glob
import json
import os
import queue
import shlex
import signal
import subprocess
import sys
import threading
import time

import openvr
from PIL import Image, ImageDraw, ImageFont

APP_KEY = "gamesinhand.sfrecorder"
HOME = os.path.expanduser("~")
CONFIG_DIR = os.path.join(os.environ.get("XDG_CONFIG_HOME", os.path.join(HOME, ".config")), "sfrecorder")
CONFIG_FILE = os.path.join(CONFIG_DIR, "config.ini")
ENGINE = os.path.join(HOME, ".local", "bin", "sfrecorder")
OUTPUT_DIR = os.path.join(HOME, "Videos", "SFRecorder")
# The recorder's own log of the last recording, for when something goes wrong.
SOURCE_DIR_FILE = os.path.join(CONFIG_DIR, "source_dir")
RELEASE_TAG_FILE = os.path.join(CONFIG_DIR, "release_tag")
HELPER = os.path.join(HOME, ".local", "lib", "sfrecorder", "sfrecorder-grab")
UNINSTALL_SCRIPT = os.path.join(os.environ.get("XDG_DATA_HOME", os.path.join(HOME, ".local", "share")),
                                "sfrecorder", "uninstall.sh")
UPDATE_LOG = os.path.join(os.environ.get("XDG_CACHE_HOME", os.path.join(HOME, ".cache")), "sfrecorder", "update.log")
LOG_FILE = os.path.join(os.environ.get("XDG_CACHE_HOME", os.path.join(HOME, ".cache")), "sfrecorder", "last-recording.log")

# SFrameOverlay, if it's installed: its install folder and its on/off file.
OVERLAY_SCRIPT = os.path.join(os.environ.get("XDG_DATA_HOME", os.path.join(HOME, ".local", "share")),
                              "frame-fps-overlay", "frame_fps_overlay.py")
OVERLAY_STATE = os.path.join(os.environ.get("XDG_CONFIG_HOME", os.path.join(HOME, ".config")),
                             "frame-fps-overlay", "enabled")
OVERLAY_POLL_SECONDS = 2.0
OVERLAY_REPO = "github.com/MWGIH/SFrameOverlay"

RAW_WARNING = ("Raw records both panels exactly as the headset shows them: lens-warped (fish-eye), "
               "with colour fringes at the edges and both eyes side by side. It isn't a normal-looking "
               "video. It uses about 3x the GPU of 16:9 and makes larger files. Use 16:9 for videos to share.")
EYE_HELP = ("Follows Steam's VR View. To change it there: in the SteamVR window, \u2630 (top left) \u203a "
            "Display VR View, then Menu (top left) in the VR View window \u203a pick the eye.")

DEFAULTS = {"shape": "16:9", "mic": "0", "eye": "auto", "hud": "1", "raw_warned": "0", "height": "0"}
# Height: degrees the 16:9 picture aims above SFRecorder's built-in framing.
HEIGHT_STEP, HEIGHT_LIMIT = 2, 30

FONT_CANDIDATES = [
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
]

WHITE = (255, 255, 255, 255)
GREY = (147, 154, 166, 255)
YELLOW = (255, 210, 60, 255)
RED = (255, 80, 80, 255)


def load_font(size):
    for path in FONT_CANDIDATES + sorted(glob.glob("/usr/share/fonts/**/*Sans*Bold*.ttf", recursive=True)):
        if os.path.exists(path):
            return ImageFont.truetype(path, size)
    return ImageFont.load_default(size)


def engine_version():
    """The installed engine's release, e.g. "v0.1.35" ("" if it can't say)."""
    try:
        out = subprocess.run([ENGINE, "--version"], capture_output=True, text=True, timeout=5)
        return out.stdout.strip() if out.returncode == 0 else ""
    except (OSError, subprocess.SubprocessError):
        return ""


def read_text(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return None


def helper_unlocked():
    """True if the panel helper still has the capability the install line gives it.
    Checks the helper itself, not the note install.sh leaves, so the warning
    shows exactly when recording would fail."""
    try:
        return bool(os.getxattr(HELPER, "security.capability"))
    except OSError:
        return False


def run_detached(command):
    """Runs a shell command in its own systemd scope, so it survives this tab being restarted."""
    subprocess.Popen(["systemd-run", "--user", "--collect", "/bin/bash", "-c", command],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def to_texture(img):
    data = img.tobytes()
    return (ctypes.c_ubyte * len(data)).from_buffer_copy(data)


def wrap(draw, text, font, width):
    lines, line = [], ""
    for word in text.split():
        trial = f"{line} {word}".strip()
        if draw.textlength(trial, font=font) <= width:
            line = trial
        else:
            lines.append(line)
            line = word
    return lines + [line] if line else lines


class Settings:
    def __init__(self):
        cfg = configparser.ConfigParser()
        cfg.read_dict({"recorder": DEFAULTS})
        cfg.read(CONFIG_FILE)
        s = cfg["recorder"]
        self.shape = s.get("shape") if s.get("shape") in ("16:9", "raw") else "16:9"
        self.mic = s.getboolean("mic")
        self.eye = s.get("eye") if s.get("eye") in ("auto", "left", "right") else "auto"
        self.hud = s.getboolean("hud")
        self.raw_warned = s.getboolean("raw_warned")
        try:
            self.height = max(-HEIGHT_LIMIT, min(HEIGHT_LIMIT, s.getint("height")))
        except ValueError:
            self.height = 0

    def save(self):
        cfg = configparser.ConfigParser()
        cfg["recorder"] = {"shape": self.shape, "mic": "1" if self.mic else "0", "eye": self.eye,
                           "hud": "1" if self.hud else "0", "raw_warned": "1" if self.raw_warned else "0",
                           "height": str(self.height)}
        os.makedirs(CONFIG_DIR, exist_ok=True)
        with open(CONFIG_FILE, "w") as f:
            cfg.write(f)


def vr_view_eye():
    """The eye SteamVR's VR View window shows: "left" or "right" (None if unknown)."""
    try:
        mode = openvr.VRHeadsetView().getHeadsetViewMode()
        if mode in (0, 1):
            return ("left", "right")[mode]
    except Exception:
        pass
    try:
        return ("left", "right")[openvr.VRSettings().getInt32("steamvr", "mirrorViewEye")]
    except Exception:
        return None


def overlay_state():
    """SFrameOverlay: "missing", "off" or "on"."""
    if not os.path.exists(OVERLAY_SCRIPT):
        return "missing"
    if read_text(OVERLAY_STATE) == "0":
        return "off"
    active = subprocess.run(["systemctl", "--user", "is-active", "--quiet", "frame-fps-overlay.service"])
    return "on" if active.returncode == 0 else "off"


class Engine:
    """One recording: the engine process and the latest status it reported."""

    def __init__(self, settings, eye):
        args = [ENGINE, "--shape", settings.shape, "--eye", eye or "auto",
                "--hud", "show" if settings.hud else "hide", "--height", str(settings.height)]
        if settings.mic:
            args.append("--mic")
        os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
        with open(LOG_FILE, "w") as log:
            self.proc = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
                                         text=True, bufsize=1)
        self.updates = queue.Queue()
        self.status = {"state": "starting", "seconds": 0.0, "file": "", "message": ""}
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            try:
                self.updates.put(json.loads(line))
            except ValueError:
                pass
        self.updates.put(None)

    def send(self, command):
        try:
            self.proc.stdin.write(command + "\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, ValueError):
            pass

    def poll(self):
        """Takes new status lines; returns False once the engine has exited."""
        alive = True
        while True:
            try:
                update = self.updates.get_nowait()
            except queue.Empty:
                break
            if update is None:
                alive = False
            else:
                self.status = update
        return alive


VERSION = engine_version()


class Menu:
    WIDTH, HEIGHT = 800, 1010
    # Gaming on Steam site colours, matching the FPS overlay's tab.
    BG = (0, 0, 0, 255)
    BUTTON = (24, 47, 103, 255)
    ON = (24, 157, 255, 255)
    REC = (190, 40, 40, 255)
    POPUP = (16, 32, 58, 255)

    def __init__(self):
        self.title_font = load_font(52)
        self.big_font = load_font(60)
        self.font = load_font(34)
        self.small_font = load_font(26)
        self.buttons = []
        self.info_rect = None

    def _button(self, draw, rect, text, action, fill=None, font=None):
        draw.rounded_rectangle(rect, radius=18, fill=fill or self.BUTTON)
        draw.text(((rect[0] + rect[2]) // 2, (rect[1] + rect[3]) // 2), text, font=font or self.font,
                  fill=WHITE, anchor="mm")
        self.buttons.append((*rect, action))

    def _choice(self, draw, y, label, value, action, fill=None, label_color=GREY):
        draw.text((60, y + 35), label, font=self.font, fill=label_color, anchor="lm")
        self._button(draw, (400, y, 760, y + 70), value, action, fill=fill)

    def _text_block(self, draw, rect, lines, color=WHITE):
        draw.rounded_rectangle(rect, radius=18, fill=self.POPUP)
        y = rect[1] + 24
        for line in lines:
            draw.text((rect[0] + 28, y), line, font=self.small_font, fill=color, anchor="la")
            y += 34

    def render(self, s, recording, status, overlay, view_eye, popup):
        """`popup` is "raw" (the Raw warning to confirm), "eye" (where to change the eye),
        "uninstall" (Uninstall pressed once, waiting for the second press) or None."""
        self.buttons = []
        img = Image.new("RGBA", (self.WIDTH, self.HEIGHT), self.BG)
        draw = ImageDraw.Draw(img)
        draw.text((self.WIDTH // 2, 50), "SFRecorder", font=self.title_font, fill=WHITE, anchor="mm")

        if popup == "raw":
            lines = wrap(draw, RAW_WARNING, self.small_font, 640)
            self._text_block(draw, (60, 110, 740, 140 + 34 * len(lines)), lines, YELLOW)
            top = 180 + 34 * len(lines)
            self._button(draw, (60, top, 390, top + 90), "Use Raw", "raw-yes")
            self._button(draw, (410, top, 740, top + 90), "Keep 16:9", "raw-no", fill=self.ON)
            return img

        self._button(draw, (60, 100, 740, 200), "STOP" if recording else "RECORD", "record",
                     fill=self.REC if recording else self.ON, font=self.big_font)

        locked = recording  # the engine is set up once per recording
        shape = "Raw" if s.shape == "raw" else "16:9"
        self._choice(draw, 230, "Shape  ⚠" if s.shape == "raw" else "Shape", shape,
                     None if locked else "shape", label_color=YELLOW if s.shape == "raw" else GREY)

        eye = view_eye if s.eye == "auto" else s.eye
        if s.shape == "raw":
            eye_text = "Both"
        else:
            eye_text = (eye or "left").capitalize() + ("  (VR View)" if s.eye == "auto" else "")
        draw.text((60, 355), "Eye", font=self.font, fill=GREY, anchor="lm")
        self.info_rect = (150, 330, 200, 380)
        draw.ellipse(self.info_rect, outline=WHITE, width=3)
        draw.text((175, 355), "i", font=self.font, fill=WHITE, anchor="mm")
        self.buttons.append((*self.info_rect, "eye-info"))
        self._button(draw, (400, 320, 760, 390), eye_text, None if locked or s.shape == "raw" else "eye")

        self._choice(draw, 410, "Microphone", "On" if s.mic else "Off", None if locked else "mic",
                     fill=self.ON if s.mic else None)

        if s.shape == "raw":
            draw.text((60, 535), "Height", font=self.font, fill=GREY, anchor="lm")
            draw.text((760, 535), "16:9 only", font=self.font, fill=GREY, anchor="rm")
        else:
            draw.text((60, 535), "Height", font=self.font, fill=GREY, anchor="lm")
            self._button(draw, (400, 500, 480, 570), "\u25bc", None if locked else "height-down")
            draw.text((580, 535), f"{s.height:+d}\u00b0" if s.height else "0\u00b0", font=self.font, fill=WHITE,
                      anchor="mm")
            self._button(draw, (680, 500, 760, 570), "\u25b2", None if locked else "height-up")

        if overlay == "on":
            self._choice(draw, 590, "FPS overlay", "In recordings" if s.hud else "Hidden", "hud",
                         fill=self.ON if s.hud else None)
        else:
            draw.text((60, 625), "FPS overlay", font=self.font, fill=GREY, anchor="lm")
            draw.text((760, 625), "Not installed" if overlay == "missing" else "Off", font=self.font,
                      fill=GREY, anchor="rm")
            if overlay == "missing":
                draw.text((760, 670), "Get it: " + OVERLAY_REPO, font=self.small_font,
                          fill=GREY, anchor="rm")

        if status.startswith("!"):
            # Errors can be long: wrap them, and point to the full log.
            lines = wrap(draw, status[2:], self.small_font, 680)[:3]
            lines.append("Details: ~/.cache/sfrecorder/last-recording.log")
            for i, line in enumerate(lines):
                draw.text((self.WIDTH // 2, 705 + 32 * i), line, font=self.small_font, fill=RED, anchor="mm")
        else:
            draw.text((self.WIDTH // 2, 730), status, font=self.font, fill=WHITE, anchor="mm")
        draw.text((self.WIDTH // 2, 820), "Saved to Videos/SFRecorder", font=self.small_font, fill=GREY, anchor="mm")
        draw.text((self.WIDTH // 2, 860), "Share them from the SCopy tab", font=self.small_font, fill=GREY,
                  anchor="mm")
        if recording:
            draw.text((self.WIDTH // 2, 930), "Paused while this menu is open", font=self.small_font, fill=YELLOW,
                      anchor="mm")
        else:
            self._button(draw, (110, 895, 390, 960), "Update", "update")
            if popup == "uninstall":
                self._button(draw, (410, 895, 730, 960), "Tap to confirm", "uninstall", fill=self.REC,
                             font=self.small_font)
            else:
                self._button(draw, (410, 895, 730, 960), "Uninstall", "uninstall")

        draw.text((self.WIDTH // 2, self.HEIGHT - 22), "SFRecorder " + (VERSION or "version unknown"), font=self.small_font, fill=GREY,
                  anchor="mm")

        if popup == "eye":
            lines = wrap(draw, EYE_HELP, self.small_font, 600)
            self._text_block(draw, (100, 395, 740, 425 + 34 * len(lines)), lines)
        return img

    def hit(self, x, y):
        for x0, y0, x1, y1, action in self.buttons:
            if x0 <= x <= x1 and y0 <= y <= y1:
                return action
        return None

    def over_info(self, x, y):
        r = self.info_rect
        return bool(r) and r[0] <= x <= r[2] and r[1] <= y <= r[3]

    @staticmethod
    def thumbnail():
        """The dashboard icon: a red record dot filling the tile, on a fully
        transparent background so it sits on the dashboard's own colour.
        Drawn 4x larger and scaled down for smooth edges."""
        size, scale = 256, 4
        big = Image.new("RGBA", (size * scale, size * scale), (0, 0, 0, 0))
        margin = 28 * scale
        ImageDraw.Draw(big).ellipse((margin, margin, size * scale - margin, size * scale - margin), fill=Menu.REC)
        return big.resize((size, size), Image.LANCZOS)


def format_time(seconds):
    seconds = int(seconds)
    return f"{seconds // 60}:{seconds % 60:02d}"


def steamvr_running():
    try:
        openvr.init(openvr.VRApplication_Background)
    except openvr.error_code.InitError:
        return False
    openvr.shutdown()
    return True


def run():
    settings = Settings()
    vr_system = openvr.init(openvr.VRApplication_Overlay)
    overlay = openvr.VROverlay()
    menu = Menu()
    handle, thumb_handle = overlay.createDashboardOverlay(APP_KEY + ".dashboard", "SFRecorder")
    overlay.setOverlayWidthInMeters(handle, 1.5)
    overlay.setOverlayInputMethod(handle, openvr.VROverlayInputMethod_Mouse)
    scale = openvr.HmdVector2_t()
    scale[0], scale[1] = Menu.WIDTH, Menu.HEIGHT
    overlay.setOverlayMouseScale(handle, scale)
    thumb = Menu.thumbnail()
    overlay.setOverlayRaw(thumb_handle, to_texture(thumb), thumb.width, thumb.height, 4)

    engine = None
    status = "Ready"
    popup = None
    eye_popup_pinned = False
    visible = False
    overlay_status = overlay_state()
    next_overlay_poll = 0.0
    view_eye = vr_view_eye()
    shown_seconds = -1
    event = openvr.VREvent_t()

    def redraw():
        img = menu.render(settings, engine is not None, status, overlay_status, view_eye, popup)
        overlay.setOverlayRaw(handle, to_texture(img), img.width, img.height, 4)

    def start():
        nonlocal engine, status
        if not os.access(ENGINE, os.X_OK):
            status = "! Engine missing, run the installer again"
            return
        eye = None if settings.eye == "auto" else settings.eye
        engine = Engine(settings, eye or view_eye)
        status = "Starting..."
        if visible:
            engine.send("pause")

    def act(action):
        nonlocal popup, eye_popup_pinned, status, view_eye
        if action == "record":
            if engine:
                engine.send("stop")
                status = "Saving..."
            else:
                start()
        elif action == "shape":
            if settings.shape == "raw":
                settings.shape = "16:9"
            elif settings.raw_warned:
                settings.shape = "raw"
            else:
                popup = "raw"
        elif action == "raw-yes":
            settings.shape, settings.raw_warned, popup = "raw", True, None
        elif action == "raw-no":
            popup = None
        elif action == "eye":
            current = view_eye if settings.eye == "auto" else settings.eye
            settings.eye = "right" if current == "left" else "left"
            if settings.eye == view_eye:
                settings.eye = "auto"  # back to following VR View
        elif action == "eye-info":
            eye_popup_pinned = not eye_popup_pinned
            popup = "eye" if eye_popup_pinned else None
        elif action == "mic":
            settings.mic = not settings.mic
        elif action in ("height-up", "height-down"):
            step = HEIGHT_STEP if action == "height-up" else -HEIGHT_STEP
            settings.height = max(-HEIGHT_LIMIT, min(HEIGHT_LIMIT, settings.height + step))
        elif action == "update":
            source = read_text(SOURCE_DIR_FILE)
            if engine:
                status = "! Stop recording first to update"
            elif not (source and os.path.isdir(os.path.join(source, ".git"))):
                status = "! Update needs the folder the install line made"
            else:
                tag = read_text(RELEASE_TAG_FILE) or ""
                os.makedirs(os.path.dirname(UPDATE_LOG), exist_ok=True)
                status = "Updating... this tab will restart"
                run_detached(f"cd {shlex.quote(source)} && git pull --ff-only && "
                             f"SFRECORDER_TAG={shlex.quote(tag)} ./install.sh </dev/null >{shlex.quote(UPDATE_LOG)} 2>&1")
        elif action == "uninstall":
            if engine:
                status = "! Stop recording first to uninstall"
            elif popup != "uninstall":
                popup = "uninstall"
                status = "Uninstall keeps your recordings"
            elif not os.path.exists(UNINSTALL_SCRIPT):
                popup, status = None, "! Uninstall script missing: run SFRecorder/uninstall.sh in a terminal"
            else:
                popup, status = None, "Uninstalling... this tab will close"
                # Run from a copy, as the script deletes the folder it lives in.
                run_detached(f"cp {shlex.quote(UNINSTALL_SCRIPT)} /tmp/sfrecorder-uninstall.sh && "
                             f"bash /tmp/sfrecorder-uninstall.sh; rm -f /tmp/sfrecorder-uninstall.sh")
        elif action == "hud":
            settings.hud = not settings.hud
            if engine:
                engine.send("hud show" if settings.hud else "hud hide")
        if action != "eye-info" and popup == "eye":
            popup, eye_popup_pinned = None, False
        if action != "uninstall" and popup == "uninstall":
            popup = None
        settings.save()
        redraw()

    if not helper_unlocked():
        status = "! The panels are locked: run the install line in a terminal once to unlock them"
    redraw()
    while True:
        now = time.monotonic()
        while vr_system.pollNextEvent(event):
            if event.eventType == openvr.VREvent_Quit:
                if engine:
                    engine.send("stop")
                vr_system.acknowledgeQuit_Exiting()
                return

        while overlay.pollNextOverlayEvent(handle, event)[0]:
            # OpenVR mouse coordinates start at the bottom-left of the texture.
            x, y = event.data.mouse.x, Menu.HEIGHT - event.data.mouse.y
            if event.eventType == openvr.VREvent_MouseButtonUp:
                action = menu.hit(x, y)
                if action:
                    act(action)
            # Hovering only opens and closes the eye help. It must not touch the
            # Raw warning or the Uninstall confirmation: moving the pointer
            # between the two Uninstall presses used to cancel it.
            elif event.eventType == openvr.VREvent_MouseMove and not eye_popup_pinned and popup in (None, "eye"):
                hover = "eye" if menu.over_info(x, y) else None
                if hover != popup:
                    popup = hover
                    redraw()

        # Pause whenever this tab is on screen, so the menu stays out of the video.
        now_visible = overlay.isOverlayVisible(handle)
        if now_visible != visible:
            visible = now_visible
            if engine:
                engine.send("pause" if visible else "resume")
            if visible:
                view_eye = vr_view_eye()
                overlay_status = overlay_state()
                redraw()
            elif popup == "eye":
                popup, eye_popup_pinned = None, False

        if engine:
            alive = engine.poll()
            st = engine.status
            if not alive:
                if st.get("state") == "finished" and st.get("file"):
                    status = "Saved " + os.path.basename(st["file"])
                elif st.get("state") == "finished":
                    status = "Nothing was recorded"
                else:
                    status = "! " + (st.get("message") or "Recording failed")
                engine = None
                redraw()
            elif st.get("state") in ("recording", "paused") and int(st.get("seconds", 0)) != shown_seconds:
                shown_seconds = int(st.get("seconds", 0))
                status = ("Paused " if st["state"] == "paused" else "Recording ") + format_time(shown_seconds)
                if visible:
                    redraw()

        if visible and now >= next_overlay_poll:
            next_overlay_poll = now + OVERLAY_POLL_SECONDS
            state = overlay_state()
            if state != overlay_status:
                overlay_status = state
                redraw()

        # Idle when nobody's looking and nothing's recording.
        time.sleep(0.05 if visible or engine else 0.5)


def main():
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    while not steamvr_running():
        time.sleep(5)
    try:
        run()
    finally:
        openvr.shutdown()


if __name__ == "__main__":
    main()

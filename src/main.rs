//! sfrecorder: records the Steam Frame's panels until told to stop.
//!
//!   sfrecorder [--shape 16:9|raw] [--eye left|right] [--mic] [--hud show|hide] [--out FILE]
//!
//! Started by the dashboard tab, which sends pause/resume/stop on stdin.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sfrecorder::control::{self, Command, ViewFile};
use sfrecorder::encoder::{Encoder, Packet};
use sfrecorder::gpu::Gpu;
use sfrecorder::kms::{self, Kms};
use sfrecorder::openvr::{Frustum, OpenVr};
use sfrecorder::writer::Writer;
use sfrecorder::{audio, lens, now_us};

/// The widest Raw recording the encoder is asked for.
const RAW_MAX_WIDTH: u32 = 3840;
/// Above this the recording takes every second (or third) panel frame.
const MAX_FPS: f64 = 90.0;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

#[derive(PartialEq)]
enum Shape {
    Wide,
    Raw,
}

struct Args {
    shape: Shape,
    eye: Option<usize>,
    mic: bool,
    hud: bool,
    out: Option<PathBuf>,
    card: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut a = Args { shape: Shape::Wide, eye: None, mic: false, hud: true, out: None, card: PathBuf::from("/dev/dri/card0") };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--shape" => {
                a.shape = match value()?.as_str() {
                    "16:9" => Shape::Wide,
                    "raw" => Shape::Raw,
                    s => bail!("unknown shape {s} (16:9 or raw)"),
                }
            }
            "--eye" => {
                a.eye = match value()?.as_str() {
                    "left" => Some(0),
                    "right" => Some(1),
                    "auto" => None,
                    s => bail!("unknown eye {s} (left, right or auto)"),
                }
            }
            "--mic" => a.mic = true,
            "--hud" => a.hud = value()? != "hide",
            "--out" => a.out = Some(PathBuf::from(value()?)),
            "--card" => a.card = PathBuf::from(value()?),
            "--diagnose" => {
                let dir = default_output()?.parent().unwrap().to_path_buf();
                if let Err(e) = sfrecorder::diagnose::run(&a.card, &dir) {
                    println!("Stopped: {e:#}");
                    std::process::exit(1);
                }
                std::process::exit(0);
            }
            "--probe-mirror" => {
                let dir = default_output()?.parent().unwrap().to_path_buf();
                if let Err(e) = sfrecorder::mirror::probe(&dir) {
                    eprintln!("mirror test failed: {e:#}");
                    std::process::exit(1);
                }
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("usage: sfrecorder [--shape 16:9|raw] [--eye left|right|auto] [--mic] [--hud show|hide] [--out FILE]");
                std::process::exit(0);
            }
            s => bail!("unknown option {s}"),
        }
    }
    Ok(a)
}

fn default_output() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME isn't set")?;
    let dir = PathBuf::from(home).join("Videos").join("SFRecorder");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let t = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    let name = format!("SFRecorder_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.mp4", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec);
    Ok(dir.join(name))
}

fn main() {
    if let Err(e) = run() {
        control::status("error", 0.0, "", &format!("{e:#}"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        // Below the game for the CPU too.
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
    let path = match args.out.clone() {
        Some(p) => p,
        None => default_output()?,
    };

    let mut kms = Kms::open(&args.card)?;
    let vr = OpenVr::connect();
    let (out_w, out_h, grid) = match args.shape {
        Shape::Wide => {
            let vr = vr.as_ref().map_err(|e| anyhow::anyhow!("16:9 needs SteamVR's lens data: {e:#}"))?;
            let eye = args.eye.or_else(|| vr.view_eye()).unwrap_or(0);
            (1920, 1080, lens::framed(vr, eye, 1920, 1080, 1.0)?)
        }
        Shape::Raw => {
            let (pw, ph) = kms.size;
            let w = pw.min(RAW_MAX_WIDTH) & !15;
            let h = ((w as u64 * ph as u64 / pw as u64) as u32) & !15;
            let square = Frustum { left: -1.0, right: 1.0, top: -1.0, bottom: 1.0 };
            let frustums = match &vr {
                Ok(vr) => [vr.frustum(0), vr.frustum(1)],
                Err(_) => [square, square],
            };
            (w, h, lens::raw(w, h, frustums))
        }
    };
    // Only the lens data was needed; don't stay connected to SteamVR.
    drop(vr);

    let every = (kms.refresh_hz / MAX_FPS).ceil().max(1.0) as u64;
    let fps = kms.refresh_hz / every as f64;
    let bitrate = if args.shape == Shape::Raw { 40_000_000 } else { 20_000_000 };
    let mut gpu = Gpu::new(&grid, out_w, out_h)?;
    let (mut encoder, targets) = Encoder::open(out_w, out_h, fps, bitrate)?;
    for t in targets {
        gpu.add_target(t)?;
    }
    let mut writer = Writer::create(&path, out_w, out_h)?;
    let view = ViewFile::new(grid.view, args.hud);
    let commands = control::commands();
    let (audio_tx, audio_rx) = mpsc::channel();
    let _audio = audio::Capture::start(args.mic, audio_tx)?;

    let file = path.to_string_lossy().into_owned();
    let start = now_us();
    let mut paused_since: Option<u64> = None;
    let mut paused_total = 0u64;
    let mut resumed_at = start;
    let mut packets: Vec<Packet> = Vec::new();
    let mut frame = 0u64;
    let mut last_status = 0u64;
    let mut last_log = start;
    let mut pending_pts = 0i64;
    let mut stats = Stats::default();
    eprintln!(
        "recording {out_w}x{out_h} at {fps:.2} fps on {}, GPU queue {}; encoder {}",
        gpu.device_name,
        if gpu.low_priority { "low priority" } else { "normal priority" },
        encoder.info
    );
    control::status("recording", 0.0, &file, "");

    while !STOP.load(Ordering::Relaxed) {
        let vblank = kms.wait_vblank()?;
        frame += 1;
        match commands.try_recv() {
            Ok(Command::Stop) | Err(TryRecvError::Disconnected) => break,
            Ok(Command::Pause) if paused_since.is_none() => paused_since = Some(vblank),
            Ok(Command::Resume) => {
                if let Some(t) = paused_since.take() {
                    paused_total += vblank.saturating_sub(t);
                    resumed_at = vblank;
                }
            }
            Ok(Command::Hud(show)) => view.write(show),
            _ => {}
        }
        let rel = |t: u64| t as i64 - start as i64 - paused_total as i64;

        while let Ok(chunk) = audio_rx.try_recv() {
            if paused_since.is_none() && chunk.pts_us >= resumed_at {
                writer.audio(&chunk, rel(chunk.pts_us))?;
            }
        }

        let seconds = rel(vblank).max(0) as f64 / 1e6;
        if vblank >= last_status + 1_000_000 {
            last_status = vblank;
            control::status(if paused_since.is_some() { "paused" } else { "recording" }, seconds, &file, "");
        }
        if paused_since.is_some() || !frame.is_multiple_of(every) {
            continue;
        }

        // Hand a finished conversion to the encoder, then start the next one
        // unless the GPU is still busy (then this frame is skipped).
        if let Some(target) = gpu.finished()? {
            encoder.encode(target, pending_pts.max(0) as u64)?;
            stats.encoded += 1;
        }
        if gpu.busy() {
            stats.skipped_gpu += 1;
        } else {
            match capture(&mut kms, &mut gpu, &mut encoder) {
                Ok(true) => pending_pts = rel(vblank),
                Ok(false) => stats.skipped_encoder += 1,
                Err(e) => {
                    stats.failed += 1;
                    if stats.failed <= 3 {
                        eprintln!("frame skipped: {e:#}");
                    }
                }
            }
        }
        if vblank >= last_log + 10_000_000 {
            last_log = vblank;
            stats.log(&mut gpu);
        }
        encoder.packets(&mut packets)?;
        for p in packets.drain(..) {
            let pts = p.pts_us as i64;
            writer.video(&p, pts)?;
        }
    }

    control::status("finishing", 0.0, &file, "");
    stats.log(&mut gpu);
    // Let a conversion still in flight land in the recording.
    for _ in 0..100 {
        if let Some(target) = gpu.finished()? {
            encoder.encode(target, pending_pts.max(0) as u64)?;
        }
        if !gpu.busy() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    encoder.finish(&mut packets)?;
    for p in packets.drain(..) {
        let pts = p.pts_us as i64;
        writer.video(&p, pts)?;
    }
    drop(view);
    match writer.finish()? {
        Some(done) => control::status("finished", 0.0, &done.to_string_lossy(), ""),
        None => control::status("finished", 0.0, "", "nothing was recorded"),
    }
    Ok(())
}

#[derive(Default)]
struct Stats {
    encoded: u64,
    skipped_gpu: u64,
    skipped_encoder: u64,
    failed: u64,
}

impl Stats {
    /// One line to stderr (the service log) every few seconds.
    fn log(&mut self, gpu: &mut Gpu) {
        let mut t = gpu.take_timings();
        t.sort_unstable();
        let pct = |p: usize| t.get(t.len().saturating_sub(1) * p / 100).map_or(0.0, |&v| v as f64 / 1000.0);
        eprintln!(
            "frames: {} encoded, {} skipped (GPU busy), {} skipped (encoder busy), {} failed; GPU time median {:.2} ms, worst {:.2} ms",
            self.encoded, self.skipped_gpu, self.skipped_encoder, self.failed, pct(50), pct(100)
        );
        *self = Self::default();
    }
}

/// One frame: borrow whatever is on screen and start converting it into a
/// free encoder buffer. False if the encoder had no buffer free.
fn capture(kms: &mut Kms, gpu: &mut Gpu, encoder: &mut Encoder) -> Result<bool> {
    let Some(target) = encoder.free_input() else { return Ok(false) };
    let converted = (|| {
        let fb = kms.current_fb()?.context("nothing on screen")?;
        let buf = kms.export(fb)?;
        let id = kms::buffer_id(&buf.fd);
        kms::wait_rendered(&buf.fd, Duration::from_millis(8));
        if !gpu.knows(id) {
            gpu.add_scanout(id, buf)?;
        }
        gpu.convert(id, target)
    })();
    if let Err(e) = converted {
        encoder.release(target);
        return Err(e);
    }
    Ok(true)
}

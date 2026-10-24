//! `sfrecorder --diagnose`: runs each stage of recording on its own, with
//! time limits, and prints what happened, so a failure on the headset shows
//! which stage it is in. Also saves what the GPU produced as a picture.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::encoder::{Encoder, Packet};
use crate::gpu::Gpu;
use crate::kms::{self, Kms};
use crate::lens;
use crate::openvr::{Frustum, OpenVr};

const W: u32 = 1920;
const H: u32 = 1080;
const LIMIT: Duration = Duration::from_secs(3);

macro_rules! say {
    ($($t:tt)*) => {{
        println!($($t)*);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }};
}

fn fourcc_name(f: u32) -> String {
    f.to_le_bytes().iter().map(|&b| b as char).collect()
}

pub fn run(card: &Path, out_dir: &Path) -> Result<()> {
    say!("1. Display");
    let mut kms = Kms::open(card)?;
    say!("   panels {}x{} at {:.2} Hz", kms.size.0, kms.size.1, kms.refresh_hz);
    say!("   panel helper unlocked: {}", crate::grab::helper_unlocked());
    for line in kms.describe_planes() {
        say!("   {line}");
    }
    let buf = kms.export_current()?;
    say!(
        "   on screen: {}x{} {} modifier {:#x}, pitch {}, offset {}",
        buf.width,
        buf.height,
        fourcc_name(buf.fourcc),
        buf.modifier,
        buf.pitch,
        buf.offset
    );
    let (fourcc, modifier) = (buf.fourcc, buf.modifier);
    drop(buf);

    say!("2. Lens data");
    let grid = match OpenVr::connect() {
        Ok(vr) => {
            let eye = vr.view_eye().unwrap_or(0);
            let f = vr.frustum(eye);
            say!("   eye {eye}, frustum l {:.3} r {:.3} t {:.3} b {:.3}", f.left, f.right, f.top, f.bottom);
            say!("   eye turned {:.1}° against the head (built in, levelled in 16:9)", lens::roll_degrees(&lens::head_rotation(eye)));
            for (u, v) in [(0.5, 0.5), (0.25, 0.5), (0.5, 0.25)] {
                match vr.distortion(eye, u, v) {
                    Some([r, g, b]) => say!(
                        "   panel {u:.2},{v:.2} shows image r {:.3},{:.3} g {:.3},{:.3} b {:.3},{:.3}",
                        r[0], r[1], g[0], g[1], b[0], b[1]
                    ),
                    None => say!("   panel {u:.2},{v:.2}: SteamVR gives no lens data"),
                }
            }
            match lens::framed(&vr, eye, W, H, 1.0) {
                Ok(g) => {
                    say!("   16:9 framing ok, {:.0}% of points inverted, per-channel colour {}", g.converged * 100.0, g.per_channel);
                    describe_grid(&g);
                    g
                }
                Err(e) => {
                    say!("   16:9 framing failed ({e:#}), using the middle of the eye without un-warping");
                    let g = lens::plain(W, H, eye, kms.size, f);
                    describe_grid(&g);
                    g
                }
            }
        }
        Err(e) => {
            say!("   no SteamVR ({e:#}), using Raw");
            raw()
        }
    };

    say!("3. GPU conversion into the GPU's own memory");
    let mut working: Option<Gpu> = None;
    for low in [false, true] {
        let mut gpu = match Gpu::with_priority(&grid, W, H, low) {
            Ok(g) => g,
            Err(e) => {
                say!("   Vulkan setup failed: {e:#}");
                continue;
            }
        };
        let prio = if gpu.low_priority { "low" } else { "normal" };
        say!("   [{prio} priority] {} on {}", gpu.describe_scanout(fourcc, modifier), gpu.device_name);
        match snapshot(&mut kms, &mut gpu, &out_dir.join("diagnose-frame.bmp")) {
            Ok(Some((t, shot))) => {
                say!("   [{prio} priority] 16:9 picture: {shot}");
                say!("   [{prio} priority] OK in {:.2} ms", t.as_secs_f64() * 1000.0);
                working = Some(gpu);
                break;
            }
            Ok(None) => say!("   [{prio} priority] NOT FINISHED after {} s", LIMIT.as_secs()),
            Err(e) => say!("   [{prio} priority] failed: {e:#}"),
        }
    }
    let Some(mut gpu) = working else {
        say!("Stopped: the GPU can't convert the panel image. Please also send `sudo dmesg | tail -40`.");
        return Ok(());
    };
    // Both whole panels, no lens maths: tells a framing problem (this one
    // shows the game) from a buffer the GPU reads as black (this one is
    // black too).
    gpu.set_grid(&raw())?;
    match snapshot(&mut kms, &mut gpu, &out_dir.join("diagnose-raw.bmp")) {
        Ok(Some((_, shot))) => say!("   both panels, no framing: {shot}"),
        Ok(None) => say!("   both panels, no framing: NOT FINISHED after {} s", LIMIT.as_secs()),
        Err(e) => say!("   both panels, no framing: failed: {e:#}"),
    }
    gpu.set_grid(&grid)?;

    say!("4. GPU conversion into the encoder's memory");
    let fps = kms.refresh_hz.min(90.0);
    let (mut encoder, targets) = Encoder::open(W, H, fps, 20_000_000)?;
    say!("   encoder {}: {}", encoder.path.display(), encoder.info);
    for t in targets {
        gpu.add_target(t)?;
    }
    let mut packets: Vec<Packet> = Vec::new();
    let mut converted = 0;
    for frame in 0..30u64 {
        let vblank = kms.wait_vblank()?;
        let Some(target) = encoder.free_input() else {
            encoder.packets(&mut packets)?;
            continue;
        };
        let buf = kms.export_current()?;
        let id = kms::buffer_id(&buf.fd);
        kms::wait_rendered(&buf.fd, Duration::from_millis(8));
        if !gpu.knows(id) {
            gpu.add_scanout(id, buf)?;
        }
        gpu.convert(id, target, 0)?;
        match gpu.wait(LIMIT)? {
            Some(t) => {
                if frame == 0 {
                    say!("   first frame OK in {:.2} ms", t.as_secs_f64() * 1000.0);
                }
                converted += 1;
            }
            None => {
                say!("   NOT FINISHED after {} s: the GPU can't write into the encoder's memory", LIMIT.as_secs());
                say!("Stopped. Please also send `sudo dmesg | tail -40`.");
                return Ok(());
            }
        }
        encoder.encode(target, vblank)?;
        encoder.packets(&mut packets)?;
    }
    encoder.finish(&mut packets)?;
    let keys = packets.iter().filter(|p| p.key).count();
    let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
    say!("5. Encoder: {converted} frames in, {} out ({keys} keyframes, {} KB)", packets.len(), bytes / 1024);
    say!("Done.");
    Ok(())
}

/// Where the grid reads from on the panel, as text.
fn describe_grid(g: &lens::Grid) {
    let c = g.at(W / 2, H / 2);
    let (lo, hi) = g.bounds();
    say!("   picture centre reads panel point {:.3},{:.3}; whole picture reads x {:.3}..{:.3}, y {:.3}..{:.3}", c[0], c[1], lo[0], hi[0], lo[1], hi[1]);
}

/// Converts what is on screen into the GPU's own memory and saves the
/// brightness as a picture; the time taken and a summary, if it finished.
fn snapshot(kms: &mut Kms, gpu: &mut Gpu, path: &Path) -> Result<Option<(Duration, String)>> {
    let buf = kms.export_current()?;
    let id = kms::buffer_id(&buf.fd);
    kms::wait_rendered(&buf.fd, Duration::from_millis(50));
    if !gpu.knows(id) {
        gpu.add_scanout(id, buf)?;
    }
    let t = gpu.add_host_target(W, H)?;
    gpu.convert(id, t, 0)?;
    let Some(done) = gpu.wait(LIMIT)? else { return Ok(None) };
    let luma = gpu.read_luma(t, (W * H) as usize)?;
    let mean = luma.iter().map(|&v| v as u64).sum::<u64>() as f64 / luma.len() as f64;
    let (lo, hi) = (luma.iter().min().copied().unwrap_or(0), luma.iter().max().copied().unwrap_or(0));
    let mut rgba = Vec::with_capacity(luma.len() * 4);
    for row in luma.chunks_exact(W as usize).rev() {
        for &v in row {
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
    }
    crate::mirror::write_bmp(&path.to_path_buf(), W, H, &rgba)?;
    let verdict = if hi <= 17 { "ALL BLACK" } else { "has a picture" };
    Ok(Some((done, format!("{verdict}, brightness average {mean:.1}, range {lo}..{hi} (16 = black, 235 = white), saved {}", path.display()))))
}

fn raw() -> lens::Grid {
    let f = Frustum { left: -1.0, right: 1.0, top: -1.0, bottom: 1.0 };
    lens::raw(W, H, [f, f])
}

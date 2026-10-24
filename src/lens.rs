//! Where each output pixel comes from on the panel.
//!
//! The result is a coarse grid (one point every STEP output pixels) of scanout
//! coordinates, built once at start. Per frame the shader only interpolates
//! it, so the lens maths costs nothing while recording.

use anyhow::{bail, Result};

use crate::openvr::{Frustum, OpenVr};

pub const STEP: u32 = 8;
/// The Frame scans each eye out turned 180° inside its own half of the
/// panel, while SteamVR describes the panels upright.
pub const EYES_ROTATED: bool = true;
/// Samples per side of SteamVR's distortion function, interpolated between.
const FORWARD_SAMPLES: usize = 65;
/// Below this colour fringing (in output pixels) one read per pixel is enough.
const FRINGE_PX: f64 = 0.33;

pub struct Grid {
    pub width: u32,
    pub height: u32,
    /// Separate red and blue positions are needed (else green is used for all).
    pub per_channel: bool,
    /// 3 points (r, g, b) per grid point, in scanout coordinates 0..1.
    pub points: Vec<[f32; 2]>,
    /// The recorded view, as positive tangents (left, right, up, down).
    pub view: [f64; 4],
}

impl Grid {
    fn new(out_w: u32, out_h: u32) -> Self {
        let (width, height) = (out_w.div_ceil(STEP) + 1, out_h.div_ceil(STEP) + 1);
        Self { width, height, per_channel: false, points: vec![[0.0; 2]; (width * height * 3) as usize], view: [0.0; 4] }
    }

    fn cells(&self) -> impl Iterator<Item = (usize, f64, f64)> {
        let w = self.width;
        (0..self.height).flat_map(move |j| (0..w).map(move |i| (((j * w + i) * 3) as usize, (i * STEP) as f64, (j * STEP) as f64)))
    }
}

/// A point on one eye's panel (0..1, upright as SteamVR describes it) to
/// scanout coordinates.
fn to_scanout(eye: usize, u: f64, v: f64) -> [f32; 2] {
    let (u, v) = if EYES_ROTATED { (1.0 - u, 1.0 - v) } else { (u, v) };
    [((eye as f64 + u.clamp(0.0, 1.0)) * 0.5) as f32, v.clamp(0.0, 1.0) as f32]
}

/// Raw: both panels as shown, each eye turned upright, fitted into the output.
pub fn raw(out_w: u32, out_h: u32, frustums: [Frustum; 2]) -> Grid {
    let mut g = Grid::new(out_w, out_h);
    for (idx, x, y) in g.cells().collect::<Vec<_>>() {
        let (x, y) = ((x / out_w as f64).min(1.0), (y / out_h as f64).min(1.0));
        let eye = if x < 0.5 { 0 } else { 1 };
        let p = to_scanout(eye, (x * 2.0 - eye as f64).clamp(0.0, 1.0), y);
        g.points[idx..idx + 3].fill(p);
    }
    let f = frustums[0];
    g.view = [-f.left, frustums[1].right, -f.top, f.bottom];
    g
}

/// SteamVR's distortion for one eye, sampled once and interpolated.
struct Forward {
    samples: Vec<[[f32; 2]; 3]>,
}

impl Forward {
    fn sample(vr: &OpenVr, eye: usize) -> Result<Self> {
        let n = FORWARD_SAMPLES;
        let mut samples = Vec::with_capacity(n * n);
        for j in 0..n {
            for i in 0..n {
                let (u, v) = (i as f32 / (n - 1) as f32, j as f32 / (n - 1) as f32);
                match vr.distortion(eye, u, v) {
                    Some(d) => samples.push(d),
                    None => bail!("SteamVR has no lens data for this eye"),
                }
            }
        }
        Ok(Self { samples })
    }

    /// Rendered-image position shown at panel point (u, v), for channel c.
    fn at(&self, c: usize, u: f64, v: f64) -> [f64; 2] {
        let n = FORWARD_SAMPLES;
        let fx = (u.clamp(0.0, 1.0) * (n - 1) as f64).min((n - 1) as f64 - 1e-9);
        let fy = (v.clamp(0.0, 1.0) * (n - 1) as f64).min((n - 1) as f64 - 1e-9);
        let (i, j) = (fx as usize, fy as usize);
        let (ax, ay) = (fx - i as f64, fy - j as f64);
        let s = |i: usize, j: usize| self.samples[j * n + i][c];
        let lerp = |a: [f32; 2], b: [f32; 2], t: f64| [a[0] as f64 + (b[0] - a[0]) as f64 * t, a[1] as f64 + (b[1] - a[1]) as f64 * t];
        let top = lerp(s(i, j), s(i + 1, j), ax);
        let bottom = lerp(s(i, j + 1), s(i + 1, j + 1), ax);
        [top[0] + (bottom[0] - top[0]) * ay, top[1] + (bottom[1] - top[1]) * ay]
    }

    /// The panel point that shows rendered-image point `target`, by Newton's
    /// method from `guess`. Points the lens never shows come back clamped to
    /// the panel's edge, which stretches the edge pixel instead of leaving black.
    fn invert(&self, c: usize, target: [f64; 2], guess: [f64; 2]) -> [f64; 2] {
        let mut p = guess;
        let h = 1e-4;
        for _ in 0..24 {
            let d = self.at(c, p[0], p[1]);
            let (ex, ey) = (d[0] - target[0], d[1] - target[1]);
            if ex.abs() < 1e-7 && ey.abs() < 1e-7 {
                break;
            }
            let du = self.at(c, p[0] + h, p[1]);
            let dv = self.at(c, p[0], p[1] + h);
            let (a, b) = ((du[0] - d[0]) / h, (dv[0] - d[0]) / h);
            let (cc, dd) = ((du[1] - d[1]) / h, (dv[1] - d[1]) / h);
            let det = a * dd - b * cc;
            if det.abs() < 1e-12 {
                break;
            }
            p[0] = (p[0] - (dd * ex - b * ey) / det).clamp(-0.05, 1.05);
            p[1] = (p[1] - (-cc * ex + a * ey) / det).clamp(-0.05, 1.05);
        }
        p
    }
}

/// The 16:9 (or any shape) view of one eye, framed like Steam's recorder:
/// the largest rectangle of the output's shape centred on the eye's frustum,
/// scaled by `zoom` (1.0 = that rectangle).
pub fn framed(vr: &OpenVr, eye: usize, out_w: u32, out_h: u32, zoom: f64) -> Result<Grid> {
    let f = vr.frustum(eye);
    let forward = Forward::sample(vr, eye)?;
    let (cx, cy) = ((f.left + f.right) / 2.0, (f.top + f.bottom) / 2.0);
    let (hw, hh) = ((f.right - f.left) / 2.0, (f.bottom - f.top) / 2.0);
    let aspect = out_w as f64 / out_h as f64;
    let (mut half_w, mut half_h) = if hw / hh > aspect { (hh * aspect, hh) } else { (hw, hw / aspect) };
    half_w /= zoom;
    half_h /= zoom;

    let mut g = Grid::new(out_w, out_h);
    // Largest red/blue offset from green, in panel units, and the green span.
    let (mut fringe_u, mut fringe_v): (f64, f64) = (0.0, 0.0);
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    let mut guess = [0.5, 0.5];
    for (idx, x, y) in g.cells().collect::<Vec<_>>() {
        let tx = cx + (x / out_w as f64 * 2.0 - 1.0) * half_w;
        let ty = cy + (y / out_h as f64 * 2.0 - 1.0) * half_h;
        let target = [(tx - f.left) / (f.right - f.left), (ty - f.top) / (f.bottom - f.top)];
        let mut panel = [[0.0; 2]; 3];
        for c in [1, 0, 2] {
            panel[c] = forward.invert(c, target, if c == 1 { guess } else { panel[1] });
        }
        guess = panel[1];
        for c in [0, 2] {
            fringe_u = fringe_u.max((panel[c][0] - panel[1][0]).abs());
            fringe_v = fringe_v.max((panel[c][1] - panel[1][1]).abs());
        }
        for k in 0..2 {
            lo[k] = lo[k].min(panel[1][k]);
            hi[k] = hi[k].max(panel[1][k]);
        }
        for (c, p) in panel.iter().enumerate() {
            g.points[idx + c] = to_scanout(eye, p[0], p[1]);
        }
    }
    // Panel units to output pixels: the output spans the green panel range.
    let fringe_px = (fringe_u * out_w as f64 / (hi[0] - lo[0]).max(1e-6)).max(fringe_v * out_h as f64 / (hi[1] - lo[1]).max(1e-6));
    g.per_channel = fringe_px > FRINGE_PX;
    g.view = [-(cx - half_w), cx + half_w, -(cy - half_h), cy + half_h];
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_puts_each_eye_in_its_half_upright() {
        let f = Frustum { left: -1.0, right: 1.0, top: -1.0, bottom: 1.0 };
        let g = raw(3840, 1920, [f, f]);
        // Output top-left is the left eye's top-left, which the rotated
        // scanout stores at the bottom-right of the left half.
        assert_eq!(g.points[1], [0.5, 1.0]);
        let last = g.points.len() - 2;
        assert_eq!(g.points[last], [0.5, 0.0]);
    }

    #[test]
    fn newton_inverts_a_smooth_warp() {
        // A barrel-like warp standing in for SteamVR's lens data.
        let n = FORWARD_SAMPLES;
        let mut samples = Vec::new();
        for j in 0..n {
            for i in 0..n {
                let (u, v) = (i as f64 / (n - 1) as f64, j as f64 / (n - 1) as f64);
                let (x, y) = (u - 0.5, v - 0.5);
                let k = 1.0 + 0.3 * (x * x + y * y);
                let p = [(0.5 + x * k) as f32, (0.5 + y * k) as f32];
                samples.push([p, p, p]);
            }
        }
        let fwd = Forward { samples };
        let target = [0.7, 0.35];
        let p = fwd.invert(1, target, [0.5, 0.5]);
        let back = fwd.at(1, p[0], p[1]);
        assert!((back[0] - target[0]).abs() < 1e-5 && (back[1] - target[1]).abs() < 1e-5, "{back:?}");
    }
}

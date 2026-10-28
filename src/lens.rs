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
/// Below this share of invertible points the lens data is not used.
const MIN_CONVERGED: f64 = 0.5;
/// Widest 16:9 picture, as the tangent of half its width (1.0 = 90° across).
/// The Frame's whole view is far wider; recorded, it looks like a fisheye
/// and its edges reach the black outside the lens.
const MAX_HALF_WIDTH: f64 = 1.0;
/// How close to the panel's edge (in eye-panel units) the picture may reach.
const EDGE: f64 = 0.02;

pub struct Grid {
    pub width: u32,
    pub height: u32,
    /// Separate red and blue positions are needed (else green is used for all).
    pub per_channel: bool,
    /// 3 points (r, g, b) per grid point, in scanout coordinates 0..1.
    pub points: Vec<[f32; 2]>,
    /// The recorded view, as positive tangents (left, right, up, down).
    pub view: [f64; 4],
    /// Share of grid points where SteamVR's lens data could be inverted
    /// (1.0 for Raw and for a plain crop).
    pub converged: f64,
    /// Every point inverted and lands clear of the panel's edge, so no part
    /// of the picture shows the black outside the lens.
    pub clean: bool,
}

impl Grid {
    fn new(out_w: u32, out_h: u32) -> Self {
        let (width, height) = (out_w.div_ceil(STEP) + 1, out_h.div_ceil(STEP) + 1);
        Self { width, height, per_channel: false, points: vec![[0.0; 2]; (width * height * 3) as usize], view: [0.0; 4], converged: 1.0, clean: true }
    }

    /// Scanout position (green) of an output pixel, for the diagnosis.
    pub fn at(&self, x: u32, y: u32) -> [f32; 2] {
        let (i, j) = ((x / STEP).min(self.width - 1), (y / STEP).min(self.height - 1));
        self.points[((j * self.width + i) * 3 + 1) as usize]
    }

    /// Smallest and largest scanout position (green) over the whole grid.
    pub fn bounds(&self) -> ([f32; 2], [f32; 2]) {
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for p in self.points.iter().skip(1).step_by(3) {
            for k in 0..2 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        (lo, hi)
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
    /// method from `guess`, and whether it got there. Points the lens never
    /// shows come back clamped to the panel's edge, which stretches the edge
    /// pixel instead of leaving black.
    fn invert(&self, c: usize, target: [f64; 2], guess: [f64; 2]) -> ([f64; 2], bool) {
        let mut p = guess;
        let h = 1e-4;
        for _ in 0..24 {
            let d = self.at(c, p[0], p[1]);
            let (ex, ey) = (d[0] - target[0], d[1] - target[1]);
            if !(ex.is_finite() && ey.is_finite()) {
                return (guess, false);
            }
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
        let d = self.at(c, p[0], p[1]);
        let ok = (d[0] - target[0]).abs() < 1e-3 && (d[1] - target[1]).abs() < 1e-3;
        (p, ok)
    }
}

/// The 16:9 (or any shape) view of one eye, framed like Steam's recorder:
/// the largest rectangle of the output's shape centred on the eye's frustum,
/// scaled by `zoom` (1.0 = that rectangle).
/// `height` (degrees) aims the picture higher than the built-in tilt.
pub fn framed(vr: &OpenVr, eye: usize, out_w: u32, out_h: u32, zoom: f64, height: f64) -> Result<Grid> {
    let f = vr.frustum(eye);
    if ![f.left, f.right, f.top, f.bottom].iter().all(|v| v.is_finite()) || f.right <= f.left || f.bottom <= f.top {
        bail!("SteamVR's view for this eye makes no sense ({f:?})");
    }
    let forward = Forward::sample(vr, eye)?;
    let rot = head_rotation(eye, EYE_PITCH_DEGREES + height);
    let g = fit(&forward, f, rot, eye, out_w, out_h, zoom);
    // Lens data the maths can't invert sends every pixel to the panel's
    // black edge; a recording of nothing is worse than an un-warped crop.
    let centre = g.at(out_w / 2, out_h / 2);
    if g.converged < MIN_CONVERGED || !inside_eye(eye, centre) {
        bail!(
            "SteamVR's lens data doesn't invert ({:.0}% of points, centre at {:.3},{:.3})",
            g.converged * 100.0,
            centre[0],
            centre[1]
        );
    }
    Ok(g)
}

/// `framed_with`, zoomed in until no part of the picture reaches past what
/// the lens shows.
fn fit(forward: &Forward, f: Frustum, rot: [[f64; 3]; 3], eye: usize, out_w: u32, out_h: u32, zoom: f64) -> Grid {
    let mut zoom = zoom;
    let mut g = framed_with(forward, f, rot, eye, out_w, out_h, zoom);
    for _ in 0..20 {
        if g.clean {
            break;
        }
        zoom *= 1.04;
        g = framed_with(forward, f, rot, eye, out_w, out_h, zoom);
    }
    g
}

/// True if a scanout point is well inside the given eye's half.
fn inside_eye(eye: usize, p: [f32; 2]) -> bool {
    let u = p[0] * 2.0 - eye as f32;
    (0.1..0.9).contains(&u) && (0.1..0.9).contains(&p[1])
}

/// The output's shape cut from the middle of one eye's panel as shown,
/// lens warp and all. Used when SteamVR's lens data can't be used.
pub fn plain(out_w: u32, out_h: u32, eye: usize, panel: (u32, u32), f: Frustum) -> Grid {
    let mut g = Grid::new(out_w, out_h);
    let eye_aspect = panel.0 as f64 / 2.0 / panel.1 as f64;
    let aspect = out_w as f64 / out_h as f64;
    let (sw, sh) = if aspect > eye_aspect { (1.0, eye_aspect / aspect) } else { (aspect / eye_aspect, 1.0) };
    for (idx, x, y) in g.cells().collect::<Vec<_>>() {
        let u = 0.5 + ((x / out_w as f64).min(1.0) - 0.5) * sw;
        let v = 0.5 + ((y / out_h as f64).min(1.0) - 0.5) * sh;
        g.points[idx..idx + 3].fill(to_scanout(eye, u, v));
    }
    // Roughly what that shows, ignoring the warp.
    let (cx, cy) = ((f.left + f.right) / 2.0, (f.top + f.bottom) / 2.0);
    let (hw, hh) = ((f.right - f.left) / 2.0 * sw, (f.bottom - f.top) / 2.0 * sh);
    g.view = [-(cx - hw), cx + hw, -(cy - hh), cy + hh];
    g
}

/// How each eye's picture is turned against the head on the Frame, in
/// degrees (left, right). Its panels are canted; uncorrected, the left eye's
/// recording leaned about 10° (upright edges in a recording), and with 10°
/// taken out the head-locked FPS HUD still rose about 2° to the right, so
/// the total is 12°. SteamVR's own answer (GetEyeToHeadTransform) crashes
/// on the Frame, so the value is built in. The eyes are mirror images, so the
/// right eye turns the other way.
pub const EYE_ROLL_DEGREES: [f64; 2] = [-12.0, 12.0];

/// How far each eye's picture is tilted down against the head, in degrees.
/// Two flat-mode recordings of a screen straight ahead pin it down: framed
/// with a 15.7° tilt the screen sat a little low, with none it sat well
/// high (and the HUD was cut off at the top). Both point to about 13°.
pub const EYE_PITCH_DEGREES: f64 = 13.0;

/// The eye's turn against the head, as a rotation (head = R * eye): tilted
/// `pitch` degrees down, then rolled about its own axis.
pub fn head_rotation(eye: usize, pitch: f64) -> [[f64; 3]; 3] {
    let (sr, cr) = EYE_ROLL_DEGREES[eye.min(1)].to_radians().sin_cos();
    let (sp, cp) = (-pitch).to_radians().sin_cos();
    let roll = [[cr, -sr, 0.0], [sr, cr, 0.0], [0.0, 0.0, 1.0]];
    let pitch = [[1.0, 0.0, 0.0], [0.0, cp, -sp], [0.0, sp, cp]];
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| pitch[i][k] * roll[k][j]).sum();
        }
    }
    out
}

/// The eye's roll against the head in degrees, for the log.
pub fn roll_degrees(r: &[[f64; 3]; 3]) -> f64 {
    r[1][0].atan2(r[0][0]).to_degrees()
}

/// Image tangents (x right, y down) between head and eye space.
fn to_eye(r: &[[f64; 3]; 3], h: [f64; 2]) -> [f64; 2] {
    let d = [h[0], -h[1], -1.0];
    // eye = R^T * head
    let e: Vec<f64> = (0..3).map(|j| (0..3).map(|i| r[i][j] * d[i]).sum()).collect();
    [e[0] / -e[2], -e[1] / -e[2]]
}

fn to_head(r: &[[f64; 3]; 3], t: [f64; 2]) -> [f64; 2] {
    let d = [t[0], -t[1], -1.0];
    let h: Vec<f64> = (0..3).map(|i| (0..3).map(|j| r[i][j] * d[j]).sum()).collect();
    [h[0] / -h[2], -h[1] / -h[2]]
}

/// Steam's recorder keeps the picture level with the head. On canted panels
/// the eye's image is turned, so the output is laid out in head space,
/// centred straight ahead (where the HUD and the game expect the middle,
/// not the middle of the eye's lopsided view), at most MAX_HALF_WIDTH wide,
/// and shrunk until it fits inside the eye's view.
fn framed_with(forward: &Forward, f: Frustum, rot: [[f64; 3]; 3], eye: usize, out_w: u32, out_h: u32, zoom: f64) -> Grid {
    let ahead = to_eye(&rot, [0.0, 0.0]);
    let ahead_visible = ahead[0] > f.left && ahead[0] < f.right && ahead[1] > f.top && ahead[1] < f.bottom;
    let (cx, cy, hw, hh) = if ahead_visible {
        (0.0, 0.0, (-f.left).min(f.right).min(MAX_HALF_WIDTH), (-f.top).min(f.bottom))
    } else {
        let [cx, cy] = to_head(&rot, [(f.left + f.right) / 2.0, (f.top + f.bottom) / 2.0]);
        (cx, cy, ((f.right - f.left) / 2.0).min(MAX_HALF_WIDTH), (f.bottom - f.top) / 2.0)
    };
    let aspect = out_w as f64 / out_h as f64;
    let (mut half_w, mut half_h) = if hw / hh > aspect { (hh * aspect, hh) } else { (hw, hw / aspect) };
    let inside = |hw: f64, hh: f64| {
        (0..=8).all(|k| {
            let a = k as f64 / 8.0 * 2.0 - 1.0;
            [[a, -1.0], [a, 1.0], [-1.0, a], [1.0, a]].iter().all(|&[sx, sy]| {
                let [tx, ty] = to_eye(&rot, [cx + sx * hw, cy + sy * hh]);
                tx >= f.left - 1e-6 && tx <= f.right + 1e-6 && ty >= f.top - 1e-6 && ty <= f.bottom + 1e-6
            })
        })
    };
    for _ in 0..100 {
        if inside(half_w, half_h) {
            break;
        }
        half_w *= 0.99;
        half_h *= 0.99;
    }
    half_w /= zoom;
    half_h /= zoom;

    let mut g = Grid::new(out_w, out_h);
    // Largest red/blue offset from green, in panel units, and the green span.
    let (mut fringe_u, mut fringe_v): (f64, f64) = (0.0, 0.0);
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    let mut guess = [0.5, 0.5];
    let mut good = 0usize;
    let mut clean = true;
    let cells = g.cells().collect::<Vec<_>>();
    for &(idx, x, y) in &cells {
        let [tx, ty] = to_eye(&rot, [cx + (x / out_w as f64 * 2.0 - 1.0) * half_w, cy + (y / out_h as f64 * 2.0 - 1.0) * half_h]);
        let target = [(tx - f.left) / (f.right - f.left), (ty - f.top) / (f.bottom - f.top)];
        let mut panel = [[0.0; 2]; 3];
        for c in [1, 0, 2] {
            let (p, ok) = forward.invert(c, target, if c == 1 { guess } else { panel[1] });
            panel[c] = p;
            if c == 1 && ok {
                good += 1;
                guess = p;
            }
            if c == 1 && !(ok && (EDGE..1.0 - EDGE).contains(&p[0]) && (EDGE..1.0 - EDGE).contains(&p[1])) {
                clean = false;
            }
        }
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
    g.converged = good as f64 / cells.len() as f64;
    g.clean = clean;
    g
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

    /// A barrel-like warp standing in for SteamVR's lens data.
    fn barrel() -> Forward {
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
        Forward { samples }
    }

    #[test]
    fn framing_lands_inside_the_eye() {
        let f = Frustum { left: -1.661, right: 1.217, top: -1.735, bottom: 1.171 };
        for eye in [0, 1] {
            let g = framed_with(&barrel(), f, crate::openvr::IDENTITY, eye, 1920, 1080, 1.0);
            assert!(g.converged > 0.95, "{}", g.converged);
            assert!(inside_eye(eye, g.at(960, 540)), "{:?}", g.at(960, 540));
        }
    }

    #[test]
    fn picture_is_centred_straight_ahead_and_clear_of_the_edge() {
        // The Frame's lopsided view: centring on its middle put the picture
        // up and to the side, reaching the black outside the lens.
        let f = Frustum { left: -1.661, right: 1.217, top: -1.735, bottom: 1.171 };
        let g = fit(&barrel(), f, crate::openvr::IDENTITY, 0, 1920, 1080, 1.0);
        let [l, r, u, d] = g.view;
        assert!((l - r).abs() < 1e-9 && (u - d).abs() < 1e-9, "{:?}", g.view);
        assert!(r <= MAX_HALF_WIDTH + 1e-9);
        assert!(g.clean);
        // A lens that shows less than the frustum makes it zoom in.
        let narrow = Forward {
            samples: barrel().samples.iter().map(|s| s.map(|[x, y]| [0.5 + (x - 0.5) * 0.6, 0.5 + (y - 0.5) * 0.6])).collect(),
        };
        let z = fit(&narrow, f, crate::openvr::IDENTITY, 0, 1920, 1080, 1.0);
        assert!(z.clean && z.view[1] < r, "{:?}", z.view);
    }

    #[test]
    fn built_in_roll_matches_the_frame() {
        // Uncorrected, upright edges in a left-eye recording leaned with
        // their tops to the left. Levelling must undo exactly that: an
        // upright line in head space reads such a line in the eye.
        let r = head_rotation(0, EYE_PITCH_DEGREES);
        let (top, bottom) = (to_eye(&r, [0.0, -0.5]), to_eye(&r, [0.0, 0.5]));
        let lean = (bottom[0] - top[0]).atan2(bottom[1] - top[1]).to_degrees();
        assert!((lean + EYE_ROLL_DEGREES[0]).abs() < 0.01, "{lean}");
        assert!((roll_degrees(&head_rotation(1, EYE_PITCH_DEGREES)) + roll_degrees(&r)).abs() < 1e-9);
        // Straight ahead lands above the middle of the eye's picture by the tilt.
        let ahead = to_eye(&r, [0.0, 0.0]);
        let up = EYE_PITCH_DEGREES.to_radians().tan();
        assert!((ahead[1] + up).abs() < 0.01 && ahead[0].abs() < 0.06, "{ahead:?}");
    }

    #[test]
    fn rolled_eye_is_levelled() {
        let (s, c) = 10f64.to_radians().sin_cos();
        let rot = [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]];
        let f = Frustum { left: -1.0, right: 1.0, top: -1.0, bottom: 1.0 };
        // A level head line maps to a line tilted by the roll in the eye.
        let (a, b) = (to_eye(&rot, [-0.5, 0.0]), to_eye(&rot, [0.5, 0.0]));
        let tilt = (b[1] - a[1]).atan2(b[0] - a[0]).to_degrees();
        assert!((tilt.abs() - 10.0).abs() < 0.01, "{tilt}");
        let back = to_head(&rot, to_eye(&rot, [0.3, -0.2]));
        assert!((back[0] - 0.3).abs() < 1e-9 && (back[1] + 0.2).abs() < 1e-9);
        assert!((roll_degrees(&rot) - 10.0).abs() < 1e-9);
        let level = framed_with(&barrel(), f, crate::openvr::IDENTITY, 0, 1920, 1080, 1.0);
        let rolled = framed_with(&barrel(), f, rot, 0, 1920, 1080, 1.0);
        // Turned, the picture has to be cropped tighter to stay inside the eye.
        assert!(rolled.view[1] + rolled.view[0] < level.view[1] + level.view[0]);
        assert!(rolled.converged > 0.95);
    }

    #[test]
    fn unusable_lens_data_is_noticed() {
        let n = FORWARD_SAMPLES;
        let fwd = Forward { samples: vec![[[f32::NAN; 2]; 3]; n * n] };
        let f = Frustum { left: -1.0, right: 1.0, top: -1.0, bottom: 1.0 };
        let g = framed_with(&fwd, f, crate::openvr::IDENTITY, 0, 1920, 1080, 1.0);
        assert!(g.converged < MIN_CONVERGED);
        assert!(g.points.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
    }

    #[test]
    fn plain_crop_is_the_middle_of_the_eye() {
        let f = Frustum { left: -1.0, right: 1.0, top: -1.0, bottom: 1.0 };
        let g = plain(1920, 1080, 1, (4320, 2160), f);
        let c = g.at(960, 540);
        assert!((c[0] - 0.75).abs() < 0.01 && (c[1] - 0.5).abs() < 0.01, "{c:?}");
        let (lo, hi) = g.bounds();
        assert!(lo[0] >= 0.5 && hi[0] <= 1.0, "{lo:?} {hi:?}");
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
        let (p, ok) = fwd.invert(1, target, [0.5, 0.5]);
        assert!(ok);
        let back = fwd.at(1, p[0], p[1]);
        assert!((back[0] - target[0]).abs() < 1e-5 && (back[1] - target[1]).abs() < 1e-5, "{back:?}");
    }
}

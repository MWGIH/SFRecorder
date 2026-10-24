//! The display side: which plane shows the VR compositor's output, when the
//! next vblank is, and the dmabuf behind whatever is on screen right now.
//! The compositor stays DRM master; this only looks.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use drm::control::{crtc, plane, Device as ControlDevice};
use drm::{ClientCapability, Device, VblankWaitFlags, VblankWaitTarget};

use crate::grab;

struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

pub struct Kms {
    card: Card,
    path: PathBuf,
    helper: Option<grab::Helper>,
    pipe: u32,
    crtc: crtc::Handle,
    plane: plane::Handle,
    /// Exact refresh rate, from the mode's pixel clock.
    pub refresh_hz: f64,
    /// Panel size in pixels, both eyes side by side.
    pub size: (u32, u32),
}

impl Kms {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path).with_context(|| format!("opening {}", path.display()))?;
        let card = Card(file);
        card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
        let res = card.resource_handles().context("reading the display's resources")?;
        let (pipe, crtc, mode) = res
            .crtcs()
            .iter()
            .enumerate()
            .find_map(|(i, &h)| Some((i as u32, h, card.get_crtc(h).ok()?.mode()?)))
            .context("no active display, is the headset on?")?;
        let plane = biggest_plane(&card, crtc)?;
        let (htotal, vtotal) = (mode.hsync().2 as f64, mode.vsync().2 as f64);
        let refresh_hz = if htotal > 0.0 && vtotal > 0.0 { mode.clock() as f64 * 1000.0 / (htotal * vtotal) } else { mode.vrefresh() as f64 };
        let (w, h) = mode.size();
        Ok(Self { card, path: path.to_path_buf(), helper: None, pipe, crtc, plane, refresh_hz, size: (w as u32, h as u32) })
    }

    /// Waits for the next vblank; returns its CLOCK_MONOTONIC time in µs.
    pub fn wait_vblank(&self) -> Result<u64> {
        loop {
            match self.card.wait_vblank(VblankWaitTarget::Relative(1), VblankWaitFlags::empty(), self.pipe, 0) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("waiting for vblank"),
                Ok(reply) => return Ok(reply.time().map_or_else(crate::now_us, |t| t.as_micros() as u64)),
            }
        }
    }

    /// Every plane showing something on the panels' display, one line each,
    /// marking the one recorded. If the compositor stacks planes, the
    /// recorded one could be a plain background.
    pub fn describe_planes(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(handles) = self.card.plane_handles() else { return out };
        for h in handles {
            let Ok(info) = self.card.get_plane(h) else { continue };
            if info.crtc() != Some(self.crtc) {
                continue;
            }
            let Some(fb) = info.framebuffer() else { continue };
            let zpos = self
                .card
                .get_properties(h)
                .ok()
                .and_then(|props| {
                    let (ids, values) = props.as_props_and_values();
                    ids.iter().zip(values).find_map(|(&id, &v)| {
                        (self.card.get_property(id).ok()?.name().to_bytes() == b"zpos").then_some(v)
                    })
                })
                .map_or("?".to_string(), |v| v.to_string());
            let what = match self.card.get_planar_framebuffer(fb) {
                Ok(f) => {
                    let fourcc: String = (f.pixel_format() as u32).to_le_bytes().iter().map(|&b| b as char).collect();
                    format!("{}x{} {} modifier {:#x}", f.size().0, f.size().1, fourcc, f.modifier().map(u64::from).unwrap_or(0))
                }
                Err(e) => format!("unreadable ({e})"),
            };
            let mark = if h == self.plane { " <- recorded" } else { "" };
            out.push(format!("plane {} fb {} zpos {zpos}: {what}{mark}", u32::from(h), u32::from(fb)));
        }
        out
    }

    /// Framebuffer on the compositor's plane right now.
    pub fn current_fb(&self) -> Result<Option<u32>> {
        Ok(self.card.get_plane(self.plane)?.framebuffer().map(u32::from))
    }

    /// The framebuffer as a dmabuf: directly if this process has the
    /// capability (development), through the panel helper otherwise.
    pub fn export(&mut self, fb_id: u32) -> Result<grab::Exported> {
        if self.helper.is_none() {
            if let Some(buf) = grab::export(&self.card, fb_id)? {
                return Ok(buf);
            }
            self.helper = Some(grab::Helper::start(&self.path).context("the panels are locked, run the installer to unlock them")?);
        }
        let result = self.helper.as_mut().unwrap().export(fb_id);
        if result.is_err() {
            self.helper = None;
        }
        result
    }
}

impl Kms {
    /// Whatever is on screen right now, as a dmabuf. The compositor flips
    /// buffers every frame, so the one asked for can be gone by the time the
    /// helper looks; then it simply asks again for the new one.
    pub fn export_current(&mut self) -> Result<grab::Exported> {
        let mut last = None;
        for _ in 0..4 {
            let fb = self.current_fb()?.context("nothing on screen, is SteamVR running?")?;
            match self.export(fb) {
                Ok(buf) => return Ok(buf),
                Err(e) => last = Some(e),
            }
        }
        if !grab::helper_unlocked() {
            bail!("the panels are locked (an update replaced the panel helper): run the install line in a terminal once");
        }
        Err(last.unwrap())
    }
}

fn biggest_plane(card: &Card, crtc: crtc::Handle) -> Result<plane::Handle> {
    let mut best = None;
    for handle in card.plane_handles()? {
        let Ok(info) = card.get_plane(handle) else { continue };
        if info.crtc() != Some(crtc) {
            continue;
        }
        let Some(fb) = info.framebuffer() else { continue };
        let area = card.get_framebuffer(fb).map(|f| f.size().0 as u64 * f.size().1 as u64).unwrap_or(0);
        if best.is_none_or(|(_, a)| area > a) {
            best = Some((handle, area));
        }
    }
    match best {
        Some((h, _)) => Ok(h),
        None => bail!("nothing is on screen, is SteamVR running?"),
    }
}

/// Identity of the buffer behind a dmabuf: every export of one buffer shares
/// one dma_buf inode, while framebuffer ids get reused.
pub fn buffer_id(fd: &OwnedFd) -> u64 {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
    st.st_ino
}

/// Waits until the compositor has finished drawing into the buffer.
pub fn wait_rendered(fd: &OwnedFd, timeout: Duration) {
    let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
}

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
        Ok(Self { card, path: path.to_path_buf(), helper: None, pipe, plane, refresh_hz, size: (w as u32, h as u32) })
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

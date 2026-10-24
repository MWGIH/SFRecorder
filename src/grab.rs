//! The panel helper's protocol.
//!
//! Handing out someone else's framebuffer as a dmabuf needs CAP_SYS_ADMIN. Only
//! the tiny `sfrecorder-grab` program holds it. The recorder starts it with a
//! socket on its stdin, sends framebuffer ids, and gets back a description of
//! the buffer plus its dmabuf fd (SCM_RIGHTS).

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};
use drm::control::{framebuffer, Device as ControlDevice};

pub const VERSION: u32 = 1;
const REPLY_LEN: usize = 32;

/// One framebuffer, exported.
pub struct Exported {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub pitch: u32,
    pub offset: u32,
    pub modifier: u64,
}

/// Where the installer puts the helper.
pub fn helper_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/lib/sfrecorder/sfrecorder-grab"))
}

/// Exports a framebuffer. Ok(None) means the kernel hid the buffer, i.e. we
/// lack the capability.
pub fn export<D: ControlDevice>(card: &D, fb_id: u32) -> Result<Option<Exported>> {
    let handle: framebuffer::Handle = drm::control::from_u32(fb_id).context("bad framebuffer id")?;
    let info = card.get_planar_framebuffer(handle).with_context(|| format!("reading framebuffer {fb_id}"))?;
    let buffers = info.buffers();
    let Some(bo) = buffers[0] else { return Ok(None) };
    if buffers[1..].iter().any(Option::is_some) {
        bail!("multi-plane scanout buffers aren't supported");
    }
    let fd = card.buffer_to_prime_fd(bo, libc::O_CLOEXEC as u32);
    let _ = card.close_buffer(bo);
    let (width, height) = info.size();
    Ok(Some(Exported {
        fd: fd.context("exporting the scanout buffer")?,
        width,
        height,
        fourcc: info.pixel_format() as u32,
        pitch: info.pitches()[0],
        offset: info.offsets()[0],
        modifier: info.modifier().map(u64::from).unwrap_or(0),
    }))
}

struct Card(std::fs::File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl ControlDevice for Card {}

/// The helper's side. Serves requests on stdin until the recorder hangs up.
/// It only opens /dev/dri/card*, and only exports framebuffers that are on a
/// plane right now, so it can't be used to read arbitrary GPU memory.
pub fn serve(card_path: &Path) -> Result<()> {
    use drm::Device;
    use std::os::unix::fs::FileTypeExt;
    let real = std::fs::canonicalize(card_path)?;
    let is_char = std::fs::metadata(&real).is_ok_and(|m| m.file_type().is_char_device());
    if !real.to_string_lossy().starts_with("/dev/dri/card") || !is_char {
        bail!("{} isn't a display device", card_path.display());
    }
    let card = Card(std::fs::OpenOptions::new().read(true).write(true).open(&real)?);
    card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)?;
    let mut sock = unsafe { UnixStream::from_raw_fd(0) };
    sock.write_all(&VERSION.to_le_bytes())?;
    let mut id = [0u8; 4];
    loop {
        match sock.read_exact(&mut id) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let fb_id = u32::from_le_bytes(id);
        let result = if on_screen(&card, fb_id) { export(&card, fb_id) } else { Ok(None) };
        match result {
            Ok(Some(buf)) => send(&sock, &encode(&buf), Some(buf.fd.as_raw_fd()))?,
            Ok(None) => send(&sock, &[0xff; REPLY_LEN], None)?,
            Err(e) => {
                eprintln!("sfrecorder-grab: {e:#}");
                send(&sock, &[0xff; REPLY_LEN], None)?
            }
        }
    }
}

fn on_screen(card: &Card, fb_id: u32) -> bool {
    let Ok(planes) = card.plane_handles() else { return false };
    planes.iter().any(|&p| {
        card.get_plane(p).ok().and_then(|info| info.framebuffer()).map(u32::from) == Some(fb_id)
    })
}

fn encode(b: &Exported) -> [u8; REPLY_LEN] {
    let mut out = [0u8; REPLY_LEN];
    for (i, v) in [0, b.width, b.height, b.fourcc, b.pitch, b.offset].into_iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out[24..32].copy_from_slice(&b.modifier.to_le_bytes());
    out
}

/// The recorder's side: a running helper.
pub struct Helper {
    sock: UnixStream,
    child: Child,
}

impl Helper {
    pub fn start(card_path: &Path) -> Result<Self> {
        let path = helper_path().context("HOME isn't set")?;
        let (ours, theirs) = UnixStream::pair()?;
        let child = Command::new(&path)
            .arg(card_path)
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", path.display()))?;
        let mut helper = Self { sock: ours, child };
        let mut version = [0u8; 4];
        helper.sock.read_exact(&mut version).context("the panel helper didn't answer")?;
        if u32::from_le_bytes(version) != VERSION {
            bail!("the panel helper is a different version, run the installer again");
        }
        Ok(helper)
    }

    pub fn export(&mut self, fb_id: u32) -> Result<Exported> {
        self.sock.write_all(&fb_id.to_le_bytes())?;
        let (b, fd) = recv(&self.sock)?;
        let word = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        let (Some(fd), 0) = (fd, word(0)) else { bail!("the panel helper couldn't export framebuffer {fb_id}") };
        Ok(Exported {
            fd,
            width: word(1),
            height: word(2),
            fourcc: word(3),
            pitch: word(4),
            offset: word(5),
            modifier: u64::from_le_bytes(b[24..32].try_into().unwrap()),
        })
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
        let _ = self.child.wait();
    }
}

fn send(sock: &UnixStream, bytes: &[u8; REPLY_LEN], fd: Option<RawFd>) -> Result<()> {
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: REPLY_LEN };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        }
    }
    if unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) } != REPLY_LEN as isize {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn recv(sock: &UnixStream) -> Result<([u8; REPLY_LEN], Option<OwnedFd>)> {
    let mut bytes = [0u8; REPLY_LEN];
    let mut iov = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: REPLY_LEN };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let got = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC | libc::MSG_WAITALL) };
    if got != REPLY_LEN as isize {
        bail!("the panel helper hung up");
    }
    let mut fd = None;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            fd = Some(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>())));
        }
    }
    Ok((bytes, fd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_carries_its_fd() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let buf = Exported {
            fd: OwnedFd::from(file),
            width: 4320,
            height: 2160,
            fourcc: 0x34325241,
            pitch: 17280,
            offset: 0,
            modifier: 0x0500_0000_0000_0001,
        };
        send(&a, &encode(&buf), Some(buf.fd.as_raw_fd())).unwrap();
        let (bytes, fd) = recv(&b).unwrap();
        assert!(fd.is_some());
        assert_eq!(bytes, encode(&buf));
        send(&a, &[0xff; REPLY_LEN], None).unwrap();
        assert!(recv(&b).unwrap().1.is_none());
    }
}

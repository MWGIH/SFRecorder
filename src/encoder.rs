//! The hardware HEVC encoder, through the V4L2 memory-to-memory interface.
//! Its input buffers are handed to the GPU as dmabufs, so frames go from the
//! shader straight into the encoder.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::gpu::TargetPlanes;
use crate::v4l2::*;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}
const HEVC: u32 = fourcc(b"HEVC");
const NV12: u32 = fourcc(b"NV12");
const NV12M: u32 = fourcc(b"NM12");
const OUTPUT: u32 = v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
const CAPTURE: u32 = v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
const MMAP: u32 = v4l2_memory_V4L2_MEMORY_MMAP;
/// Raw frames in flight: up to three being converted, one being encoded, one spare.
const INPUTS: u32 = 5;
const OUTPUTS: u32 = 4;
/// Room for one encoded frame.
const CODED_SIZE: u32 = 4 << 20;

// The ioctl numbers are C macros bindgen can't see.
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}
const RW: u64 = 3;
const W: u64 = 1;
const R: u64 = 2;
const QUERYCAP: u64 = ioc(R, 0, size_of::<v4l2_capability>());
const ENUM_FMT: u64 = ioc(RW, 2, size_of::<v4l2_fmtdesc>());
const S_FMT: u64 = ioc(RW, 5, size_of::<v4l2_format>());
const REQBUFS: u64 = ioc(RW, 8, size_of::<v4l2_requestbuffers>());
const QUERYBUF: u64 = ioc(RW, 9, size_of::<v4l2_buffer>());
const QBUF: u64 = ioc(RW, 15, size_of::<v4l2_buffer>());
const EXPBUF: u64 = ioc(RW, 16, size_of::<v4l2_exportbuffer>());
const DQBUF: u64 = ioc(RW, 17, size_of::<v4l2_buffer>());
const STREAMON: u64 = ioc(W, 18, size_of::<i32>());
const STREAMOFF: u64 = ioc(W, 19, size_of::<i32>());
const S_PARM: u64 = ioc(RW, 22, size_of::<v4l2_streamparm>());
const S_CTRL: u64 = ioc(RW, 28, size_of::<v4l2_control>());
const S_SELECTION: u64 = ioc(RW, 95, size_of::<v4l2_selection>());
const ENCODER_CMD: u64 = ioc(RW, 77, size_of::<v4l2_encoder_cmd>());

fn ioctl<T>(fd: &File, req: u64, arg: &mut T) -> std::io::Result<()> {
    loop {
        if unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg as *mut T) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn zeroed<T>() -> T {
    unsafe { std::mem::zeroed() }
}

pub struct Packet {
    pub data: Vec<u8>,
    pub pts_us: u64,
    pub key: bool,
}

struct Mapped {
    ptr: *mut u8,
    len: usize,
}

pub struct Encoder {
    dev: File,
    coded: Vec<Mapped>,
    free: Vec<usize>,
    luma_size: u32,
    chroma_size: u32,
    planes: u32,
    pub path: PathBuf,
    /// The input layout the driver reported, for `--diagnose`.
    pub info: String,
}

impl Encoder {
    /// Opens the first HEVC encoder and configures it; returns it with the
    /// layout of its input buffers for the GPU.
    pub fn open(width: u32, height: u32, fps: f64, bitrate: u32) -> Result<(Self, Vec<TargetPlanes>)> {
        let (dev, path) = find().context("no hardware HEVC encoder found")?;

        let mut fmt: v4l2_format = zeroed();
        fmt.type_ = CAPTURE;
        unsafe {
            fmt.fmt.pix_mp.width = width;
            fmt.fmt.pix_mp.height = height;
            fmt.fmt.pix_mp.pixelformat = HEVC;
            fmt.fmt.pix_mp.num_planes = 1;
            fmt.fmt.pix_mp.plane_fmt[0].sizeimage = CODED_SIZE;
        }
        ioctl(&dev, S_FMT, &mut fmt).context("the encoder refused HEVC at this size")?;

        let pixelformat = if formats(&dev, OUTPUT).contains(&NV12M) { NV12M } else { NV12 };
        let mut fmt: v4l2_format = zeroed();
        fmt.type_ = OUTPUT;
        fmt.fmt.pix_mp.width = width;
        fmt.fmt.pix_mp.height = height;
        fmt.fmt.pix_mp.pixelformat = pixelformat;
        fmt.fmt.pix_mp.num_planes = if pixelformat == NV12M { 2 } else { 1 };
        fmt.fmt.pix_mp.colorspace = v4l2_colorspace_V4L2_COLORSPACE_REC709;
        fmt.fmt.pix_mp.quantization = v4l2_quantization_V4L2_QUANTIZATION_LIM_RANGE as u8;
        ioctl(&dev, S_FMT, &mut fmt).context("the encoder refused NV12 input")?;
        let pix = unsafe { fmt.fmt.pix_mp };
        if pix.width < width || pix.height < height {
            let (w, h) = (pix.width, pix.height);
            bail!("the encoder can't take {width}x{height} (offers {w}x{h})");
        }
        let stride = pix.plane_fmt[0].bytesperline;
        // NV12 in one plane: chroma follows luma at the height the driver
        // reports, which may be padded past the picture.
        let scanlines = pix.height;
        if scanlines != height {
            // Only the picture gets encoded, not the padding rows.
            let mut sel = v4l2_selection { type_: v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_OUTPUT, target: V4L2_SEL_TGT_CROP, ..zeroed() };
            sel.r = v4l2_rect { left: 0, top: 0, width, height };
            let _ = ioctl(&dev, S_SELECTION, &mut sel);
        }

        let mut parm: v4l2_streamparm = zeroed();
        parm.type_ = OUTPUT;
        let (num, den) = rational(fps);
        parm.parm.output.timeperframe.numerator = num;
        parm.parm.output.timeperframe.denominator = den;
        let _ = ioctl(&dev, S_PARM, &mut parm);

        // Constant quality for motion, one keyframe a second so cuts land
        // close to where they're asked for, and no B-frames (no reordering).
        let gop = fps.round() as i32;
        for (id, value) in [
            (V4L2_CID_MPEG_VIDEO_BITRATE_MODE, v4l2_mpeg_video_bitrate_mode_V4L2_MPEG_VIDEO_BITRATE_MODE_VBR as i32),
            (V4L2_CID_MPEG_VIDEO_BITRATE, bitrate as i32),
            (V4L2_CID_MPEG_VIDEO_GOP_SIZE, gop),
            (V4L2_CID_MPEG_VIDEO_B_FRAMES, 0),
            (V4L2_CID_MPEG_VIDEO_HEVC_PROFILE, v4l2_mpeg_video_hevc_profile_V4L2_MPEG_VIDEO_HEVC_PROFILE_MAIN as i32),
            (V4L2_CID_MPEG_VIDEO_HEADER_MODE, v4l2_mpeg_video_header_mode_V4L2_MPEG_VIDEO_HEADER_MODE_JOINED_WITH_1ST_FRAME as i32),
            (V4L2_CID_MPEG_VIDEO_PREPEND_SPSPPS_TO_IDR, 1),
        ] {
            let mut c = v4l2_control { id, value };
            let _ = ioctl(&dev, S_CTRL, &mut c);
        }

        let planes = if pixelformat == NV12M { 2 } else { 1 };
        let mut req = v4l2_requestbuffers { count: INPUTS, type_: OUTPUT, memory: MMAP, ..zeroed() };
        ioctl(&dev, REQBUFS, &mut req).context("allocating encoder input")?;
        let mut targets = Vec::new();
        for index in 0..req.count {
            let export = |plane: u32| -> Result<OwnedFd> {
                let mut e = v4l2_exportbuffer { type_: OUTPUT, index, plane, flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32, ..zeroed() };
                ioctl(&dev, EXPBUF, &mut e).context("sharing an encoder buffer")?;
                Ok(unsafe { OwnedFd::from_raw_fd(e.fd) })
            };
            let luma = export(0)?;
            let (chroma, chroma_offset, chroma_stride) = if planes == 2 {
                (export(1)?, 0, pix.plane_fmt[1].bytesperline)
            } else {
                (luma.try_clone()?, stride * scanlines, stride)
            };
            targets.push(TargetPlanes { luma, luma_offset: 0, luma_stride: stride, chroma, chroma_offset, chroma_stride });
        }
        let luma_size = stride * scanlines;
        let info = format!(
            "{} input {}x{}, {} plane(s), stride {}, sizeimage {:?}, buffer {} bytes",
            if pixelformat == NV12M { "NV12M" } else { "NV12" },
            { pix.width },
            { pix.height },
            planes,
            stride,
            (0..planes as usize).map(|i| { pix.plane_fmt[i].sizeimage }).collect::<Vec<_>>(),
            targets.first().map_or(0, |t: &TargetPlanes| unsafe { libc::lseek(t.luma.as_raw_fd(), 0, libc::SEEK_END) })
        );
        let chroma_size = if planes == 2 { pix.plane_fmt[1].bytesperline * scanlines.div_ceil(2) } else { 0 };

        let mut req = v4l2_requestbuffers { count: OUTPUTS, type_: CAPTURE, memory: MMAP, ..zeroed() };
        ioctl(&dev, REQBUFS, &mut req).context("allocating encoder output")?;
        let mut coded = Vec::new();
        for index in 0..req.count {
            let mut plane: v4l2_plane = zeroed();
            let mut buf = v4l2_buffer { index, type_: CAPTURE, memory: MMAP, length: 1, ..zeroed() };
            buf.m.planes = &mut plane;
            ioctl(&dev, QUERYBUF, &mut buf)?;
            let len = plane.length as usize;
            let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, dev.as_raw_fd(), plane.m.mem_offset as i64) };
            if ptr == libc::MAP_FAILED {
                bail!("mapping encoder output: {}", std::io::Error::last_os_error());
            }
            coded.push(Mapped { ptr: ptr.cast(), len });
        }

        let mut enc = Self { dev, coded, free: (0..targets.len()).collect(), luma_size, chroma_size, planes, path, info };
        for i in 0..enc.coded.len() {
            enc.queue_coded(i as u32)?;
        }
        for t in [OUTPUT, CAPTURE] {
            let mut t = t as i32;
            ioctl(&enc.dev, STREAMON, &mut t).context("starting the encoder")?;
        }
        Ok((enc, targets))
    }

    /// An input buffer the GPU may write into, if one is free.
    pub fn free_input(&mut self) -> Option<usize> {
        self.reclaim();
        self.free.pop()
    }

    /// Gives back an input buffer that wasn't used after all.
    pub fn release(&mut self, index: usize) {
        self.free.push(index);
    }

    /// Hands a converted frame to the encoder.
    pub fn encode(&mut self, index: usize, pts_us: u64) -> Result<()> {
        let mut planes: [v4l2_plane; 2] = zeroed();
        planes[0].bytesused = self.luma_size;
        planes[1].bytesused = self.chroma_size;
        let mut buf = v4l2_buffer { index: index as u32, type_: OUTPUT, memory: MMAP, length: self.planes, ..zeroed() };
        buf.m.planes = planes.as_mut_ptr();
        buf.timestamp = timeval { tv_sec: (pts_us / 1_000_000) as _, tv_usec: (pts_us % 1_000_000) as _ };
        buf.flags = V4L2_BUF_FLAG_TIMESTAMP_COPY;
        if let Err(e) = ioctl(&self.dev, QBUF, &mut buf) {
            self.free.push(index);
            return Err(e).context("queueing a frame for encoding");
        }
        Ok(())
    }

    /// Encoded frames that are ready, without waiting.
    pub fn packets(&mut self, out: &mut Vec<Packet>) -> Result<bool> {
        loop {
            let mut plane: v4l2_plane = zeroed();
            let mut buf = v4l2_buffer { type_: CAPTURE, memory: MMAP, length: 1, ..zeroed() };
            buf.m.planes = &mut plane;
            match ioctl(&self.dev, DQBUF, &mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => return Ok(true),
                Err(e) => return Err(e).context("reading from the encoder"),
                Ok(()) => {}
            }
            let m = &self.coded[buf.index as usize];
            let start = plane.data_offset as usize;
            let end = (plane.bytesused as usize).min(m.len);
            if end > start {
                let data = unsafe { std::slice::from_raw_parts(m.ptr.add(start), end - start) }.to_vec();
                let pts_us = buf.timestamp.tv_sec as u64 * 1_000_000 + buf.timestamp.tv_usec as u64;
                out.push(Packet { data, pts_us, key: buf.flags & V4L2_BUF_FLAG_KEYFRAME != 0 });
            }
            let last = buf.flags & V4L2_BUF_FLAG_LAST != 0;
            if last {
                return Ok(true);
            }
            self.queue_coded(buf.index)?;
        }
    }

    /// Flushes the encoder, returning the last frames.
    pub fn finish(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        let mut cmd: v4l2_encoder_cmd = zeroed();
        cmd.cmd = V4L2_ENC_CMD_STOP;
        if ioctl(&self.dev, ENCODER_CMD, &mut cmd).is_err() {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let mut pfd = libc::pollfd { fd: self.dev.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut pfd, 1, 100) };
            if self.packets(out)? {
                break;
            }
        }
        Ok(())
    }

    fn reclaim(&mut self) {
        loop {
            let mut planes: [v4l2_plane; 2] = zeroed();
            let mut buf = v4l2_buffer { type_: OUTPUT, memory: MMAP, length: self.planes, ..zeroed() };
            buf.m.planes = planes.as_mut_ptr();
            if ioctl(&self.dev, DQBUF, &mut buf).is_err() {
                return;
            }
            self.free.push(buf.index as usize);
        }
    }

    fn queue_coded(&mut self, index: u32) -> Result<()> {
        let mut plane: v4l2_plane = zeroed();
        let mut buf = v4l2_buffer { index, type_: CAPTURE, memory: MMAP, length: 1, ..zeroed() };
        buf.m.planes = &mut plane;
        ioctl(&self.dev, QBUF, &mut buf).context("returning an encoder buffer")
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        for t in [OUTPUT, CAPTURE] {
            let mut t = t as i32;
            let _ = ioctl(&self.dev, STREAMOFF, &mut t);
        }
        for m in &self.coded {
            unsafe { libc::munmap(m.ptr.cast(), m.len) };
        }
    }
}

fn find() -> Option<(File, PathBuf)> {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev").ok()?.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("video"))).collect();
    nodes.sort();
    nodes.into_iter().find_map(|path| {
        let dev = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path).ok()?;
        let mut cap: v4l2_capability = zeroed();
        ioctl(&dev, QUERYCAP, &mut cap).ok()?;
        let caps = if cap.capabilities & V4L2_CAP_DEVICE_CAPS != 0 { cap.device_caps } else { cap.capabilities };
        (caps & V4L2_CAP_VIDEO_M2M_MPLANE != 0 && formats(&dev, CAPTURE).contains(&HEVC)).then_some((dev, path))
    })
}

fn formats(dev: &File, type_: u32) -> Vec<u32> {
    (0..64)
        .map_while(|index| {
            let mut d = v4l2_fmtdesc { index, type_, ..zeroed() };
            ioctl(dev, ENUM_FMT, &mut d).ok().map(|_| d.pixelformat)
        })
        .collect()
}

/// Frame time as a fraction, e.g. 90 Hz → 1/90, 59.94 → 1001/60000.
fn rational(fps: f64) -> (u32, u32) {
    let den = (fps * 1000.0).round() as u32;
    (1000, den.max(1))
}

//! SFRecorder: records what the Steam Frame's panels show, Steam overlays and
//! all, with as little cost to the game as possible.
//!
//! The pixels never touch the CPU: the panel's scanout buffer is borrowed as a
//! dmabuf, one compute shader turns it into NV12 inside the hardware encoder's
//! own input buffer, and the encoder writes HEVC.

pub mod audio;
pub mod control;
pub mod encoder;
pub mod gpu;
pub mod grab;
pub mod kms;
pub mod lens;
pub mod openvr;
pub mod writer;

#[allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, clippy::all)]
pub mod v4l2 {
    include!(concat!(env!("OUT_DIR"), "/v4l2.rs"));
}

/// CLOCK_MONOTONIC now, in microseconds. Video and audio share this clock.
pub fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

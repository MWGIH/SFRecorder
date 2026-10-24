//! Game audio (what's playing on the default output) and, if asked, the
//! microphone mixed in. PipeWire's PulseAudio interface keeps this to a
//! blocking read in one small thread.

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{bail, Result};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms per read: few wakeups, still well inside one video frame of drift.
const CHUNK_FRAMES: usize = 960;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}
#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}
const PA_SAMPLE_FLOAT32LE: c_int = 5;
const PA_STREAM_RECORD: c_int = 2;

#[link(name = "pulse-simple")]
#[link(name = "pulse")]
extern "C" {
    fn pa_simple_new(server: *const c_char, name: *const c_char, dir: c_int, dev: *const c_char, stream: *const c_char, ss: *const SampleSpec, map: *const c_void, attr: *const BufferAttr, error: *mut c_int) -> *mut c_void;
    fn pa_simple_read(s: *mut c_void, data: *mut c_void, bytes: usize, error: *mut c_int) -> c_int;
    fn pa_simple_get_latency(s: *mut c_void, error: *mut c_int) -> u64;
    fn pa_simple_free(s: *mut c_void);
}

/// Interleaved stereo f32 samples; `pts_us` is when the first one was heard.
pub struct Chunk {
    pub pts_us: u64,
    pub samples: Vec<f32>,
}

struct Stream(*mut c_void);
unsafe impl Send for Stream {}

impl Stream {
    fn open(device: &str, what: &str) -> Result<Self> {
        let spec = SampleSpec { format: PA_SAMPLE_FLOAT32LE, rate: RATE, channels: CHANNELS as u8 };
        let bytes = (CHUNK_FRAMES * CHANNELS * 4) as u32;
        let attr = BufferAttr { maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: bytes };
        let (name, dev, stream) = (CString::new("SFRecorder")?, CString::new(device)?, CString::new(what)?);
        let mut err = 0;
        let s = unsafe { pa_simple_new(std::ptr::null(), name.as_ptr(), PA_STREAM_RECORD, dev.as_ptr(), stream.as_ptr(), &spec, std::ptr::null(), &attr, &mut err) };
        if s.is_null() {
            bail!("can't open {what} (error {err})");
        }
        Ok(Self(s))
    }

    /// Fills `buf`; returns when its first sample was captured.
    fn read(&mut self, buf: &mut [f32]) -> Result<u64> {
        let mut err = 0;
        if unsafe { pa_simple_read(self.0, buf.as_mut_ptr().cast(), size_of_val(buf), &mut err) } < 0 {
            bail!("audio read failed (error {err})");
        }
        let now = crate::now_us();
        let latency = unsafe { pa_simple_get_latency(self.0, &mut err) };
        let duration = buf.len() as u64 / CHANNELS as u64 * 1_000_000 / RATE as u64;
        Ok(now.saturating_sub(latency + duration))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { pa_simple_free(self.0) };
    }
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Capture {
    /// Starts sending chunks of game audio, with the mic mixed in if `mic`.
    pub fn start(mic: bool, out: Sender<Chunk>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut game = Stream::open("@DEFAULT_MONITOR@", "game audio")?;
        let mut threads = Vec::new();
        let voice: Option<Arc<Mutex<VecDeque<f32>>>> = if mic {
            let mut stream = Stream::open("@DEFAULT_SOURCE@", "microphone")?;
            let queue = Arc::new(Mutex::new(VecDeque::new()));
            let (q, stop) = (queue.clone(), stop.clone());
            threads.push(std::thread::spawn(move || {
                let mut buf = vec![0f32; CHUNK_FRAMES * CHANNELS];
                while !stop.load(Ordering::Relaxed) && stream.read(&mut buf).is_ok() {
                    let mut q = q.lock().unwrap();
                    q.extend(buf.iter().copied());
                    // Never more than a fifth of a second behind.
                    let excess = q.len().saturating_sub(RATE as usize / 5 * CHANNELS);
                    q.drain(..excess);
                }
            }));
            Some(queue)
        } else {
            None
        };
        let s = stop.clone();
        threads.push(std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let mut samples = vec![0f32; CHUNK_FRAMES * CHANNELS];
                let Ok(pts_us) = game.read(&mut samples) else { break };
                if let Some(v) = &voice {
                    let mut q = v.lock().unwrap();
                    let n = q.len().min(CHUNK_FRAMES * CHANNELS);
                    for (s, m) in samples.iter_mut().zip(q.drain(..n)) {
                        *s = (*s + m).clamp(-1.0, 1.0);
                    }
                }
                if out.send(Chunk { pts_us, samples }).is_err() {
                    break;
                }
            }
        }));
        Ok(Self { stop, threads })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

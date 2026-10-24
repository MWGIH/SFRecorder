//! MP4 out: the encoder's HEVC as is, plus AAC from FFmpeg's encoder. The file
//! is written as `<name>.part` and only renamed once it's complete, so a
//! half-written file is never mistaken for a recording.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{bail, Context, Result};
use ffmpeg_sys_next as ff;

use crate::audio::{self, Chunk};
use crate::encoder::Packet;

const US: ff::AVRational = ff::AVRational { num: 1, den: 1_000_000 };
const AUDIO_BITRATE: i64 = 160_000;
/// Audio this far off its own clock gets padded or trimmed back into sync.
const RESYNC_SAMPLES: i64 = audio::RATE as i64 / 20;

fn check(ret: i32, what: &str) -> Result<i32> {
    if ret < 0 {
        let mut buf = [0i8; 128];
        unsafe { ff::av_strerror(ret, buf.as_mut_ptr(), buf.len()) };
        let msg = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
        bail!("{what}: {msg}");
    }
    Ok(ret)
}

pub struct Writer {
    path: PathBuf,
    part: PathBuf,
    ctx: *mut ff::AVFormatContext,
    video: *mut ff::AVStream,
    audio: *mut ff::AVStream,
    aac: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    started: bool,
    /// Planar samples waiting for a full AAC frame.
    pending: [Vec<f32>; audio::CHANNELS],
    /// Position of the next sample written, in samples since the start.
    next_sample: Option<i64>,
    last_video_pts: i64,
}

impl Writer {
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self> {
        let part = path.with_extension("mp4.part");
        let cpart = CString::new(part.to_string_lossy().as_bytes())?;
        unsafe {
            ff::av_log_set_level(ff::AV_LOG_ERROR);
            let mut ctx = ptr::null_mut();
            check(ff::avformat_alloc_output_context2(&mut ctx, ptr::null(), c"mp4".as_ptr(), cpart.as_ptr()), "setting up MP4")?;
            let mut w = Self {
                path: path.to_path_buf(),
                part,
                ctx,
                video: ptr::null_mut(),
                audio: ptr::null_mut(),
                aac: ptr::null_mut(),
                frame: ff::av_frame_alloc(),
                pkt: ff::av_packet_alloc(),
                started: false,
                pending: Default::default(),
                next_sample: None,
                last_video_pts: -1,
            };

            w.video = ff::avformat_new_stream(ctx, ptr::null());
            let par = &mut *(*w.video).codecpar;
            par.codec_type = ff::AVMediaType::AVMEDIA_TYPE_VIDEO;
            par.codec_id = ff::AVCodecID::AV_CODEC_ID_HEVC;
            par.codec_tag = u32::from_le_bytes(*b"hvc1");
            par.width = width as i32;
            par.height = height as i32;
            par.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            par.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            par.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            par.color_space = ff::AVColorSpace::AVCOL_SPC_BT709;
            (*w.video).time_base = US;

            let codec = ff::avcodec_find_encoder(ff::AVCodecID::AV_CODEC_ID_AAC);
            if codec.is_null() {
                bail!("FFmpeg has no AAC encoder");
            }
            w.aac = ff::avcodec_alloc_context3(codec);
            let c = &mut *w.aac;
            c.sample_rate = audio::RATE as i32;
            c.sample_fmt = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
            ff::av_channel_layout_default(&mut c.ch_layout, audio::CHANNELS as i32);
            c.bit_rate = AUDIO_BITRATE;
            c.time_base = ff::AVRational { num: 1, den: audio::RATE as i32 };
            c.flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            check(ff::avcodec_open2(w.aac, codec, ptr::null_mut()), "opening the AAC encoder")?;
            w.audio = ff::avformat_new_stream(ctx, ptr::null());
            check(ff::avcodec_parameters_from_context((*w.audio).codecpar, w.aac), "audio stream")?;
            (*w.audio).time_base = c.time_base;

            let f = &mut *w.frame;
            f.format = c.sample_fmt as i32;
            f.nb_samples = c.frame_size;
            f.sample_rate = c.sample_rate;
            check(ff::av_channel_layout_copy(&mut f.ch_layout, &c.ch_layout), "audio frame")?;
            check(ff::av_frame_get_buffer(w.frame, 0), "audio frame")?;

            check(ff::avio_open(&mut (*ctx).pb, cpart.as_ptr(), ff::AVIO_FLAG_WRITE), "creating the file")?;
            Ok(w)
        }
    }

    /// Writes one encoded video frame; `pts_us` counts from the recording's start.
    pub fn video(&mut self, p: &Packet, pts_us: i64) -> Result<()> {
        if !self.started {
            if !p.key {
                return Ok(());
            }
            self.start(&p.data)?;
        }
        // Timestamps must rise even if the clock hiccups.
        let pts = pts_us.max(self.last_video_pts + 1);
        self.last_video_pts = pts;
        unsafe {
            check(ff::av_new_packet(self.pkt, p.data.len() as i32), "video packet")?;
            ptr::copy_nonoverlapping(p.data.as_ptr(), (*self.pkt).data, p.data.len());
            let k = &mut *self.pkt;
            k.stream_index = (*self.video).index;
            k.pts = ff::av_rescale_q(pts, US, (*self.video).time_base);
            k.dts = k.pts;
            if p.key {
                k.flags |= ff::AV_PKT_FLAG_KEY;
            }
            check(ff::av_interleaved_write_frame(self.ctx, self.pkt), "writing video")?;
        }
        Ok(())
    }

    /// Adds audio; `pts_us` counts from the recording's start.
    pub fn audio(&mut self, chunk: &Chunk, pts_us: i64) -> Result<()> {
        if !self.started || pts_us < 0 {
            return Ok(());
        }
        let at = pts_us * audio::RATE as i64 / 1_000_000;
        let expected = *self.next_sample.get_or_insert(at) + self.pending[0].len() as i64;
        let mut skip = 0;
        if at - expected > RESYNC_SAMPLES {
            for ch in &mut self.pending {
                ch.extend(std::iter::repeat_n(0.0, (at - expected) as usize));
            }
        } else if expected - at > RESYNC_SAMPLES {
            skip = (expected - at) as usize;
        }
        for frame in chunk.samples.chunks_exact(audio::CHANNELS).skip(skip) {
            for (ch, s) in self.pending.iter_mut().zip(frame) {
                ch.push(*s);
            }
        }
        self.drain_audio(false)
    }

    /// Finishes the file and gives it its real name.
    pub fn finish(mut self) -> Result<Option<PathBuf>> {
        if !self.started {
            return Ok(None);
        }
        self.drain_audio(true)?;
        unsafe {
            check(ff::avcodec_send_frame(self.aac, ptr::null()), "flushing audio")?;
            self.write_audio_packets()?;
            check(ff::av_write_trailer(self.ctx), "finishing the file")?;
            ff::avio_closep(&mut (*self.ctx).pb);
        }
        std::fs::rename(&self.part, &self.path).context("renaming the finished recording")?;
        Ok(Some(self.path.clone()))
    }

    fn start(&mut self, keyframe: &[u8]) -> Result<()> {
        let headers = parameter_sets(keyframe);
        if headers.is_empty() {
            bail!("the encoder's first keyframe has no stream headers");
        }
        unsafe {
            let par = &mut *(*self.video).codecpar;
            par.extradata = ff::av_mallocz(headers.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize).cast();
            ptr::copy_nonoverlapping(headers.as_ptr(), par.extradata, headers.len());
            par.extradata_size = headers.len() as i32;
            check(ff::avformat_write_header(self.ctx, ptr::null_mut()), "writing the MP4 header")?;
        }
        self.started = true;
        Ok(())
    }

    fn drain_audio(&mut self, last: bool) -> Result<()> {
        let size = unsafe { (*self.aac).frame_size } as usize;
        while self.pending[0].len() >= size || (last && !self.pending[0].is_empty()) {
            let n = self.pending[0].len().min(size);
            unsafe {
                check(ff::av_frame_make_writable(self.frame), "audio frame")?;
                let f = &mut *self.frame;
                f.nb_samples = n as i32;
                for (c, ch) in self.pending.iter_mut().enumerate() {
                    ptr::copy_nonoverlapping(ch.as_ptr(), f.data[c].cast::<f32>(), n);
                    ch.drain(..n);
                }
                let pos = self.next_sample.unwrap_or(0);
                f.pts = pos;
                self.next_sample = Some(pos + n as i64);
                check(ff::avcodec_send_frame(self.aac, self.frame), "encoding audio")?;
            }
            self.write_audio_packets()?;
        }
        Ok(())
    }

    fn write_audio_packets(&mut self) -> Result<()> {
        unsafe {
            loop {
                let ret = ff::avcodec_receive_packet(self.aac, self.pkt);
                if ret == ff::AVERROR(libc::EAGAIN) || ret == ff::AVERROR_EOF {
                    return Ok(());
                }
                check(ret, "encoding audio")?;
                (*self.pkt).stream_index = (*self.audio).index;
                ff::av_packet_rescale_ts(self.pkt, (*self.aac).time_base, (*self.audio).time_base);
                check(ff::av_interleaved_write_frame(self.ctx, self.pkt), "writing audio")?;
            }
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        unsafe {
            if !(*self.ctx).pb.is_null() {
                ff::avio_closep(&mut (*self.ctx).pb);
            }
            ff::avformat_free_context(self.ctx);
            ff::avcodec_free_context(&mut self.aac);
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
        }
    }
}

/// The VPS, SPS and PPS NAL units (with start codes) from an Annex B frame.
fn parameter_sets(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nal_units(data) {
        if let Some(&first) = nal.first() {
            if matches!((first >> 1) & 0x3f, 32..=34) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(nal);
            }
        }
    }
    out
}

fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
            while end > s && data[end - 1] == 0 {
                end -= 1;
            }
            &data[s..end]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_parameter_sets_out_of_a_keyframe() {
        // VPS (32), SPS (33), PPS (34), then an IDR slice (19).
        let frame = [0, 0, 0, 1, 0x40, 1, 0xaa, 0, 0, 0, 1, 0x42, 1, 0xbb, 0, 0, 1, 0x44, 1, 0xcc, 0, 0, 0, 1, 0x26, 1, 0xdd];
        assert_eq!(parameter_sets(&frame), [0, 0, 0, 1, 0x40, 1, 0xaa, 0, 0, 0, 1, 0x42, 1, 0xbb, 0, 0, 0, 1, 0x44, 1, 0xcc]);
    }
}

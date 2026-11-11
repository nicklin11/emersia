//! Hardware decode straight to the panel's Surface via NDK `AMediaCodec`.
//!
//! The decoder renders into the `ANativeWindow` itself, so decoded pixels never
//! touch the CPU or the JVM: datagram → AEAD → reassembly → codec input buffer
//! is the only copy chain, and output buffers are released to the display the
//! moment they exist.

use std::time::Duration;

use emersia_transport::codec::Codec;
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;

use crate::session::VideoSink;
use std::sync::Arc;

/// Input buffers are sized for the worst keyframe we expect (4K HEVC at high
/// bitrate); the codec rejects larger access units rather than truncating.
const MAX_INPUT: i32 = 4 << 20;
/// Nominal dimensions. The real size comes from the in-band SPS; these only
/// size the initial allocation, and `max-*` allows adaptive changes up to 4K.
const NOMINAL: (i32, i32) = (1920, 1080);
const MAX_DIMS: (i32, i32) = (3840, 2160);
/// How long `submit` may wait for a free input buffer before giving up on a
/// frame. Rendering is drained between attempts, which is what frees them.
const INPUT_WAIT: Duration = Duration::from_millis(4);
const INPUT_ATTEMPTS: usize = 6;

pub struct SurfaceDecoder {
    window: NativeWindow,
    codec: Option<(Codec, MediaCodec)>,
    stats: Arc<crate::session::Stats>,
    /// When each queued unit (by pts) went in, to time the decoder.
    queued: std::collections::VecDeque<(u64, std::time::Instant)>,
}

impl SurfaceDecoder {
    pub fn new(window: NativeWindow, stats: Arc<crate::session::Stats>) -> Self {
        Self {
            window,
            codec: None,
            stats,
            queued: std::collections::VecDeque::new(),
        }
    }

    fn mime(codec: Codec) -> &'static str {
        match codec {
            Codec::H264 => "video/avc",
            Codec::Hevc => "video/hevc",
        }
    }

    fn open(&mut self, kind: Codec) -> Result<(), String> {
        self.codec = None; // drop (stop + delete) any previous instance first
        let mime = Self::mime(kind);
        let codec =
            MediaCodec::from_decoder_type(mime).ok_or_else(|| format!("no decoder for {mime}"))?;
        let mut format = MediaFormat::new();
        format.set_str("mime", mime);
        format.set_i32("width", NOMINAL.0);
        format.set_i32("height", NOMINAL.1);
        format.set_i32("max-width", MAX_DIMS.0);
        format.set_i32("max-height", MAX_DIMS.1);
        format.set_i32("max-input-size", MAX_INPUT);
        // Low-latency hints. Unknown keys are ignored by decoders that do not
        // support them, so the vendor keys are safe to set everywhere.
        format.set_i32("low-latency", 1); // KEY_LOW_LATENCY, API 30
        format.set_i32("priority", 0); // realtime
        format.set_i32("operating-rate", 120);
        // Qualcomm (Quest's XR2): output in decode order, no reorder queue.
        format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
        format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
        codec
            .configure(&format, Some(&self.window), MediaCodecDirection::Decoder)
            .map_err(|e| format!("configure {mime}: {e:?}"))?;
        codec.start().map_err(|e| format!("start {mime}: {e:?}"))?;
        let name = codec.name().unwrap_or_else(|_| "unknown".into());
        log::info!("decoder {name} ({mime}) started");
        self.stats.set_decoder(name);
        self.codec = Some((kind, codec));
        Ok(())
    }

    /// Release every decoded frame to the display; only the newest matters,
    /// so older ones are dropped without rendering.
    fn drain(&mut self) {
        let Some((_, codec)) = &self.codec else {
            return;
        };
        let mut newest = None;
        loop {
            match codec.dequeue_output_buffer(Duration::ZERO) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let pts = buffer.info().presentation_time_us() as u64;
                    while let Some(&(queued_pts, at)) = self.queued.front() {
                        if queued_pts > pts {
                            break;
                        }
                        self.queued.pop_front();
                        if queued_pts == pts {
                            crate::session::smooth(
                                &self.stats.decode_us,
                                at.elapsed().as_micros() as u64,
                            );
                        }
                    }
                    if let Some(older) = newest.replace(buffer) {
                        let _ = codec.release_output_buffer(older, false);
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    log::info!("decoder output format: {:?}", codec.output_format());
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Err(e) => {
                    log::warn!("dequeue output: {e:?}");
                    break;
                }
            }
        }
        if let Some(buffer) = newest {
            if let Err(e) = codec.release_output_buffer(buffer, true) {
                log::warn!("render: {e:?}");
            }
        }
    }
}

impl VideoSink for SurfaceDecoder {
    fn submit(
        &mut self,
        kind: Codec,
        au: &[u8],
        pts_us: u64,
        keyframe: bool,
    ) -> Result<(), String> {
        if self.codec.as_ref().map(|(k, _)| *k) != Some(kind) {
            if !keyframe {
                return Err("codec changed mid-stream without a keyframe".into());
            }
            self.open(kind)?;
        }
        for _ in 0..INPUT_ATTEMPTS {
            let (_, codec) = self.codec.as_ref().expect("opened above");
            match codec.dequeue_input_buffer(INPUT_WAIT) {
                Ok(DequeuedInputBufferResult::Buffer(mut input)) => {
                    let dst = input.buffer_mut();
                    if au.len() > dst.len() {
                        return Err(format!(
                            "access unit of {} bytes exceeds input buffer of {}",
                            au.len(),
                            dst.len()
                        ));
                    }
                    // SAFETY: dst has at least au.len() bytes (checked above),
                    // and MaybeUninit<u8> has u8's layout.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            au.as_ptr(),
                            dst.as_mut_ptr().cast::<u8>(),
                            au.len(),
                        );
                    }
                    codec
                        .queue_input_buffer(input, 0, au.len(), pts_us, 0)
                        .map_err(|e| format!("queue input: {e:?}"))?;
                    if self.queued.len() == 64 {
                        self.queued.pop_front();
                    }
                    self.queued.push_back((pts_us, std::time::Instant::now()));
                    return Ok(());
                }
                Ok(DequeuedInputBufferResult::TryAgainLater) => self.drain(),
                Err(e) => return Err(format!("dequeue input: {e:?}")),
            }
        }
        Err("decoder input stalled".into())
    }

    fn service(&mut self) {
        self.drain();
    }

    fn reset(&mut self) {
        // A new session restarts the stream at a keyframe; flushing drops
        // anything queued from the old one.
        if let Some((_, codec)) = &self.codec {
            if let Err(e) = codec.flush() {
                log::warn!("flush: {e:?}; reopening on next keyframe");
                self.codec = None;
            }
        }
    }
}

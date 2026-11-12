//! AAudio low-latency output fed from a [`PcmBuffer`].

use std::sync::{Arc, Mutex};

use emersia_transport::transport::control::{AUDIO_CHANNELS, AUDIO_RATE};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode,
    AudioStream, AudioStreamBuilder,
};

use crate::audio_buffer::PcmBuffer;

/// Cushion against Wi-Fi jitter, and the bound past which it is trimmed.
const TARGET_MS: u32 = 30;
const MAX_MS: u32 = 120;

pub struct AudioOut {
    buffer: Arc<Mutex<PcmBuffer>>,
    stream: Option<AudioStream>,
    failed: bool,
}

impl Default for AudioOut {
    fn default() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(PcmBuffer::new(
                AUDIO_RATE,
                AUDIO_CHANNELS,
                TARGET_MS,
                MAX_MS,
            ))),
            stream: None,
            failed: false,
        }
    }
}

impl AudioOut {
    fn open(&mut self) -> Result<(), String> {
        let buffer = self.buffer.clone();
        let stream = AudioStreamBuilder::new()
            .map_err(|e| format!("{e:?}"))?
            .direction(AudioDirection::Output)
            .sharing_mode(AudioSharingMode::Shared)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .sample_rate(AUDIO_RATE as i32)
            .channel_count(AUDIO_CHANNELS as i32)
            .format(AudioFormat::PCM_I16)
            .data_callback(Box::new(move |_, data, frames| {
                let len = frames.max(0) as usize * AUDIO_CHANNELS as usize;
                // SAFETY: AAudio hands a buffer of `frames` frames in the
                // format we requested (interleaved i16).
                let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<i16>(), len) };
                match buffer.try_lock() {
                    Ok(mut b) => b.pull(out),
                    // Never block the real-time thread; a contended lock
                    // costs one callback of silence.
                    Err(_) => out.fill(0),
                }
                AudioCallbackResult::Continue
            }))
            .open_stream()
            .map_err(|e| format!("open: {e:?}"))?;
        stream
            .request_start()
            .map_err(|e| format!("start: {e:?}"))?;
        log::info!(
            "audio out: {} Hz, {} ch, {:?}",
            stream.sample_rate(),
            stream.channel_count(),
            stream.performance_mode()
        );
        self.stream = Some(stream);
        Ok(())
    }

    /// Queue a PCM chunk; the stream opens on the first one.
    pub fn push(&mut self, pcm: &[u8]) {
        if self.failed {
            return;
        }
        if self.stream.is_none() {
            if let Err(e) = self.open() {
                log::warn!("audio disabled: {e}");
                self.failed = true;
                return;
            }
        }
        if let Ok(mut b) = self.buffer.lock() {
            b.push(pcm);
        }
    }
}

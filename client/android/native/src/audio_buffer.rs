//! Jitter buffer between the network and the audio callback.
//!
//! Audio arrives in 5 ms chunks on the network thread and is pulled by the
//! audio device's real-time callback. The buffer absorbs Wi-Fi jitter but is
//! bounded: if it grows past `max` (the network burst after a stall, or a
//! sender clock slightly faster than the DAC's), it is cut back to `target`,
//! so latency can never drift upward. Underruns play silence.

use std::collections::VecDeque;

pub struct PcmBuffer {
    samples: VecDeque<i16>,
    /// Level (in samples) to trim back to.
    target: usize,
    /// Level (in samples) that triggers a trim.
    max: usize,
    /// Whether playback has started; before that, wait until `target` is
    /// reached so the first chunks do not immediately underrun.
    primed: bool,
    pub underruns: u64,
    pub trimmed: u64,
}

impl PcmBuffer {
    /// Sizes in milliseconds at `rate` Hz with `channels` channels.
    pub fn new(rate: u32, channels: u32, target_ms: u32, max_ms: u32) -> Self {
        let per_ms = (rate / 1000 * channels) as usize;
        Self {
            samples: VecDeque::with_capacity(per_ms * max_ms as usize * 2),
            target: per_ms * target_ms as usize,
            max: per_ms * max_ms as usize,
            primed: false,
            underruns: 0,
            trimmed: 0,
        }
    }

    /// Append little-endian s16 bytes (an odd trailing byte is ignored).
    pub fn push(&mut self, bytes: &[u8]) {
        let (pairs, _) = bytes.as_chunks::<2>();
        self.samples
            .extend(pairs.iter().map(|&b| i16::from_le_bytes(b)));
        if self.samples.len() > self.max {
            let excess = self.samples.len() - self.target;
            self.samples.drain(..excess);
            self.trimmed += 1;
        }
    }

    /// Fill `out` completely: buffered audio first, silence for the rest.
    pub fn pull(&mut self, out: &mut [i16]) {
        if !self.primed {
            if self.samples.len() < self.target {
                out.fill(0);
                return;
            }
            self.primed = true;
        }
        let n = out.len().min(self.samples.len());
        for (dst, src) in out.iter_mut().zip(self.samples.drain(..n)) {
            *dst = src;
        }
        if n < out.len() {
            out[n..].fill(0);
            self.underruns += 1;
            // Re-prime so playback resumes with a cushion, not sample by sample.
            self.primed = false;
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn waits_for_target_then_plays_in_order() {
        // 1 kHz mono so 1 ms = 1 sample: target 4, max 10.
        let mut b = PcmBuffer::new(1000, 1, 4, 10);
        b.push(&bytes(&[1, 2, 3]));
        let mut out = [9i16; 2];
        b.pull(&mut out);
        assert_eq!(out, [0, 0], "not primed yet");
        b.push(&bytes(&[4]));
        b.pull(&mut out);
        assert_eq!(out, [1, 2]);
        b.pull(&mut out);
        assert_eq!(out, [3, 4]);
        assert_eq!(b.underruns, 0);
    }

    #[test]
    fn underrun_plays_silence_and_counts() {
        let mut b = PcmBuffer::new(1000, 1, 2, 10);
        b.push(&bytes(&[5, 6, 7]));
        let mut out = [9i16; 4];
        b.pull(&mut out);
        assert_eq!(out, [5, 6, 7, 0]);
        assert_eq!(b.underruns, 1);
    }

    #[test]
    fn latency_is_bounded_by_trimming_the_oldest() {
        let mut b = PcmBuffer::new(1000, 1, 4, 10);
        b.push(&bytes(&(0..12).collect::<Vec<i16>>()));
        assert_eq!(b.len(), 4, "trimmed back to target");
        assert_eq!(b.trimmed, 1);
        let mut out = [0i16; 4];
        b.pull(&mut out);
        assert_eq!(out, [8, 9, 10, 11], "newest audio kept");
    }
}

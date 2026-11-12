//! The status overlay text, with rates computed between polls.

use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Instant;

use crate::session::Stats;

/// Remembers the previous poll so rates are measured, not estimated.
#[derive(Default)]
pub struct Sampler {
    last: Mutex<Option<(Instant, u64, u64)>>,
}

impl Sampler {
    /// `(fps, Mbit/s)` since the previous call.
    fn rates(&self, frames: u64, bytes: u64) -> (f64, f64) {
        let now = Instant::now();
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        let rates = match *last {
            Some((at, f0, b0)) => {
                let dt = now.duration_since(at).as_secs_f64().max(1e-3);
                (
                    frames.saturating_sub(f0) as f64 / dt,
                    bytes.saturating_sub(b0) as f64 * 8.0 / dt / 1e6,
                )
            }
            None => (0.0, 0.0),
        };
        *last = Some((now, frames, bytes));
        rates
    }
}

pub fn render(server: &str, stats: &Stats, sampler: &Sampler) -> String {
    let submitted = stats.frames_submitted.load(Ordering::Relaxed);
    let bytes = stats.bytes_received.load(Ordering::Relaxed);
    let (fps, mbps) = sampler.rates(submitted, bytes);
    let mut text = format!(
        "{} · {}\n{:.0} fps · {:.1} Mbit/s · decoder {}\nframes {} · skipped {} · losses {} · IDR requests {} · input {} · audio {} · sessions {} · handshake {:.1} ms",
        server,
        stats.phase().label(),
        fps,
        mbps,
        stats.decoder().unwrap_or_else(|| "—".into()),
        submitted,
        stats.frames_skipped.load(Ordering::Relaxed),
        stats.losses.load(Ordering::Relaxed),
        stats.keyframe_requests.load(Ordering::Relaxed),
        stats.input_events.load(Ordering::Relaxed),
        stats.audio_chunks.load(Ordering::Relaxed),
        stats.sessions.load(Ordering::Relaxed),
        stats.handshake_us.load(Ordering::Relaxed) as f64 / 1000.0,
    );
    let ms = |us: u64| -> String {
        if us == 0 {
            "—".into()
        } else {
            format!("{:.1} ms", us as f64 / 1000.0)
        }
    };
    let rx = stats.capture_to_rx_us.load(Ordering::Relaxed);
    let dec = stats.decode_us.load(Ordering::Relaxed);
    text.push_str(&format!(
        "\nlatency: capture→received {} · decode {} · sum {} · rtt {}",
        ms(rx),
        ms(dec),
        if rx > 0 && dec > 0 {
            ms(rx + dec)
        } else {
            "—".into()
        },
        ms(stats.rtt_us.load(Ordering::Relaxed)),
    ));
    if let Some(err) = stats.last_error() {
        text.push_str("\nlast error: ");
        text.push_str(&err);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_mentions_phase_and_error() {
        let stats = Stats::default();
        stats.set_error("boom");
        let text = render("10.0.0.1:47800", &stats, &Sampler::default());
        assert!(text.starts_with("10.0.0.1:47800 · connecting"));
        assert!(text.contains("last error: boom"));
    }

    #[test]
    fn rates_are_measured_between_polls() {
        let sampler = Sampler::default();
        assert_eq!(sampler.rates(0, 0), (0.0, 0.0));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (fps, mbps) = sampler.rates(10, 125_000);
        assert!(fps > 0.0 && mbps > 0.0);
    }
}

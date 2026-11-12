//! Desktop audio to connected headsets.
//!
//! Captures the default sink's monitor through ffmpeg's PulseAudio input
//! (served by pipewire-pulse on current desktops) as 48 kHz stereo s16le and
//! sends 5 ms chunks as `AUDIO_PCM` records in the encrypted channel.
//! Uncompressed on purpose: ~1.5 Mbit/s is noise next to the video, and it
//! avoids codec latency entirely. Capture runs only while the daemon is
//! streaming to at least one connected device.

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::network::{lock, SharedEndpoint};
use crate::service::{rtp_timestamp, Service};
use crate::transport::control::{AUDIO_CHANNELS, AUDIO_CHUNK_BYTES, AUDIO_PCM, AUDIO_RATE};

const IDLE: Duration = Duration::from_millis(200);

/// The ffmpeg command that captures desktop audio.
fn capture_command() -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        // Small fragments: PulseAudio otherwise hands over ~50 ms at a time.
        .args([
            "-f",
            "pulse",
            "-fragment_size",
            &AUDIO_CHUNK_BYTES.to_string(),
        ])
        .args(["-i", "@DEFAULT_MONITOR@"])
        .args([
            "-ac",
            &AUDIO_CHANNELS.to_string(),
            "-ar",
            &AUDIO_RATE.to_string(),
        ])
        .args(["-f", "s16le", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd
}

struct Capture {
    child: Child,
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start the audio thread; it exits when the service shuts down.
pub fn spawn(endpoint: SharedEndpoint, service: Arc<Service>) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("emersia-audio".into())
        .spawn(move || run(endpoint, service))
}

fn wanted(endpoint: &SharedEndpoint, service: &Service) -> bool {
    service.is_streaming() && lock(endpoint).peer_count() > 0
}

fn run(endpoint: SharedEndpoint, service: Arc<Service>) {
    let epoch = service.stream_epoch();
    let mut warned = false;
    while !service.is_shutting_down() && !crate::signal_received() {
        if !wanted(&endpoint, &service) {
            std::thread::sleep(IDLE);
            continue;
        }
        let mut capture = match capture_command().spawn() {
            Ok(child) => Capture { child },
            Err(e) => {
                if !warned {
                    eprintln!("emersia-daemon: audio disabled, cannot start ffmpeg: {e}");
                    warned = true;
                }
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let Some(mut stdout) = capture.child.stdout.take() else {
            continue;
        };
        eprintln!("emersia-daemon: streaming desktop audio");
        let mut chunk = vec![0u8; AUDIO_CHUNK_BYTES];
        while !service.is_shutting_down() && wanted(&endpoint, &service) {
            if stdout.read_exact(&mut chunk).is_err() {
                eprintln!("emersia-daemon: audio capture ended; retrying");
                std::thread::sleep(Duration::from_secs(1));
                break;
            }
            let timestamp = rtp_timestamp(epoch, std::time::Instant::now());
            let mut ep = lock(&endpoint);
            for id in ep.peer_ids() {
                let _ = ep.send_frame(&id, timestamp, false, AUDIO_PCM, &chunk);
            }
        }
        // Dropping `capture` stops ffmpeg until audio is wanted again.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_command_asks_for_the_wire_format() {
        let cmd = capture_command();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(
            joined.contains("-ac 2 -ar 48000 -f s16le pipe:1"),
            "{joined}"
        );
        assert!(joined.contains("@DEFAULT_MONITOR@"));
    }
}

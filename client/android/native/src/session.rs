//! Platform-independent receive loop: handshake, decrypt, reassemble, gate on
//! keyframes, hand whole access units to a [`VideoSink`].
//!
//! Nothing here touches Android, so the whole path — including reconnects and
//! loss handling — is tested on the host against a real `HostEndpoint` over
//! loopback.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use emersia_transport::codec::Codec;
use emersia_transport::crypto::{device_id_bytes, DeviceIdentity};
use emersia_transport::transport::udp::ClientEndpoint;
use emersia_transport::transport::{FrameHeader, Reassembler, VIDEO_CLOCK_HZ};

/// How long to wait for the host's handshake response.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Socket read slice. Short, so the sink is serviced (decoded frames released
/// to the display) promptly even when no datagram is arriving.
const READ_SLICE: Duration = Duration::from_millis(2);
/// Silence after which the session is presumed dead and re-established. The
/// host forces a keyframe at least every 2 s and capture only stalls on a
/// fully idle screen, so 4 s of nothing means the host went away.
const STALL_TIMEOUT: Duration = Duration::from_secs(4);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(3);
/// Receive buffer requested from the kernel (see `set_recv_buffer`).
const RECV_BUFFER: usize = 4 << 20;

/// Where the session is, for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Connecting = 0,
    WaitingForKeyframe = 1,
    Streaming = 2,
    Stopped = 3,
}

impl Phase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::WaitingForKeyframe,
            2 => Self::Streaming,
            3 => Self::Stopped,
            _ => Self::Connecting,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::WaitingForKeyframe => "waiting for keyframe",
            Self::Streaming => "streaming",
            Self::Stopped => "stopped",
        }
    }
}

/// Counters shared with the UI thread. Relaxed atomics: these are reports, not
/// synchronisation.
#[derive(Default)]
pub struct Stats {
    phase: AtomicU8,
    pub sessions: AtomicU32,
    pub frames_received: AtomicU64,
    pub frames_submitted: AtomicU64,
    pub frames_skipped: AtomicU64,
    pub losses: AtomicU64,
    pub bytes_received: AtomicU64,
    pub handshake_us: AtomicU64,
    last_error: Mutex<Option<String>>,
    decoder: Mutex<Option<String>>,
}

impl Stats {
    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Relaxed))
    }

    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub fn set_error(&self, err: impl Into<String>) {
        let err = err.into();
        log::warn!("{err}");
        *self.last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(err);
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn set_decoder(&self, name: impl Into<String>) {
        *self.decoder.lock().unwrap_or_else(|p| p.into_inner()) = Some(name.into());
    }

    pub fn decoder(&self) -> Option<String> {
        self.decoder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Consumer of whole access units — MediaCodec on the headset, a recorder in
/// tests.
pub trait VideoSink {
    /// Queue one access unit (Annex-B, parameter sets inline on keyframes).
    fn submit(
        &mut self,
        codec: Codec,
        au: &[u8],
        pts_us: u64,
        keyframe: bool,
    ) -> Result<(), String>;
    /// Called whenever the loop is idle, so decoded output can be released to
    /// the display without waiting for the next datagram.
    fn service(&mut self) {}
    /// The stream is about to restart from a keyframe (new session).
    fn reset(&mut self) {}
}

/// Connection parameters.
pub struct Config {
    /// `HOST:PORT` of the daemon's stream endpoint.
    pub server: String,
    /// Pinned host static public key.
    pub host_key: [u8; 32],
}

/// Decides which whole frames reach the decoder.
///
/// H.264/HEVC P-frames reference their predecessor, so after a lost frame
/// everything up to the next keyframe would decode as smeared garbage. For a
/// desktop that is worse than a brief freeze, so frames are held back until a
/// keyframe restarts the reference chain.
#[derive(Debug, Default)]
pub struct KeyframeGate {
    expected: Option<u32>,
    synced: bool,
}

/// What the gate decided for one frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    Pass,
    /// Dropped; `lost` is true when this frame revealed a gap.
    Skip {
        lost: bool,
    },
}

impl KeyframeGate {
    /// `header` is the header of the frame's last fragment.
    pub fn admit(&mut self, header: &FrameHeader) -> Gate {
        // Each fragment consumes one sequence number after the frame's own
        // (see HostEndpoint::send_frame), so the next frame is predictable.
        let next = header
            .frame_sequence
            .wrapping_add(u32::from(header.frag_count))
            .wrapping_add(1);
        let lost = self
            .expected
            .is_some_and(|expected| expected != header.frame_sequence);
        self.expected = Some(next);
        if lost {
            self.synced = false;
        }
        if header.keyframe {
            self.synced = true;
        }
        if self.synced {
            Gate::Pass
        } else {
            Gate::Skip { lost }
        }
    }

    pub fn synced(&self) -> bool {
        self.synced
    }
}

/// Extends the 32-bit 90 kHz RTP timestamp to a monotonic microsecond clock.
#[derive(Debug, Default)]
struct PtsClock {
    last: Option<u32>,
    ticks: u64,
}

impl PtsClock {
    fn micros(&mut self, timestamp: u32) -> u64 {
        if let Some(last) = self.last {
            // Forward distance with wraparound; a backwards step (reordering
            // across frames should not happen) counts as zero.
            let delta = timestamp.wrapping_sub(last);
            if delta < 0x8000_0000 {
                self.ticks += u64::from(delta);
            }
        }
        self.last = Some(timestamp);
        self.ticks * 1_000_000 / u64::from(VIDEO_CLOCK_HZ)
    }
}

/// Run until `stop` is set: connect, stream, reconnect on silence.
pub fn run(
    config: &Config,
    identity: &DeviceIdentity,
    sink: &mut dyn VideoSink,
    stop: &AtomicBool,
    stats: &Stats,
) {
    let device_id = device_id_bytes(&identity.public_key_hex());
    let mut backoff = BACKOFF_MIN;
    while !stop.load(Ordering::Relaxed) {
        stats.set_phase(Phase::Connecting);
        match connect(config, device_id, identity, stats) {
            Ok(client) => {
                backoff = BACKOFF_MIN;
                stats.sessions.fetch_add(1, Ordering::Relaxed);
                sink.reset();
                stream(client, sink, stop, stats);
            }
            Err(err) => {
                stats.set_error(err);
                sleep_unless_stopped(backoff, stop);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
    stats.set_phase(Phase::Stopped);
}

fn connect(
    config: &Config,
    device_id: [u8; 8],
    identity: &DeviceIdentity,
    stats: &Stats,
) -> Result<ClientEndpoint, String> {
    let identity = DeviceIdentity::from_private_hex(&identity.private_key_hex())
        .map_err(|e| format!("identity: {e}"))?;
    let mut client = ClientEndpoint::new(&config.server, device_id, identity, config.host_key)
        .map_err(|e| format!("socket for {}: {e}", config.server))?;
    #[cfg(unix)]
    match client.set_recv_buffer(RECV_BUFFER) {
        Ok(granted) => log::info!("receive buffer: {} KiB", granted / 1024),
        Err(e) => log::warn!("could not enlarge receive buffer: {e}"),
    }
    let started = Instant::now();
    client
        .send_init()
        .map_err(|e| format!("handshake init to {}: {e}", config.server))?;
    let response = client.recv_raw(HANDSHAKE_TIMEOUT).map_err(|_| {
        format!(
            "no answer from {} (daemon not streaming, wrong host key, or this headset is not paired)",
            config.server
        )
    })?;
    client
        .complete(&response)
        .map_err(|e| format!("handshake rejected: {e}"))?;
    let elapsed = started.elapsed();
    stats
        .handshake_us
        .store(elapsed.as_micros() as u64, Ordering::Relaxed);
    log::info!(
        "session established with {} in {:.1} ms",
        config.server,
        elapsed.as_secs_f64() * 1000.0
    );
    Ok(client)
}

fn stream(mut client: ClientEndpoint, sink: &mut dyn VideoSink, stop: &AtomicBool, stats: &Stats) {
    stats.set_phase(Phase::WaitingForKeyframe);
    let mut reassembler = Reassembler::default();
    let mut gate = KeyframeGate::default();
    let mut clock = PtsClock::default();
    let mut last_datagram = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let (header, payload) = match client.recv_record(READ_SLICE) {
            Ok(record) => record,
            Err(_) => {
                // Timeout, or a dropped (unauthenticated/replayed) datagram.
                sink.service();
                if last_datagram.elapsed() > STALL_TIMEOUT {
                    stats.set_error("stream stalled; reconnecting");
                    return;
                }
                continue;
            }
        };
        last_datagram = Instant::now();
        stats
            .bytes_received
            .fetch_add(payload.len() as u64, Ordering::Relaxed);
        let Some(codec) = Codec::from_payload_type(header.payload_type) else {
            continue; // not video
        };
        let Some(frame) = reassembler.push(header, &payload) else {
            continue;
        };
        stats.frames_received.fetch_add(1, Ordering::Relaxed);
        let pts = clock.micros(header.timestamp);
        match gate.admit(&header) {
            Gate::Pass => {}
            Gate::Skip { lost } => {
                if lost {
                    stats.losses.fetch_add(1, Ordering::Relaxed);
                }
                stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
                stats.set_phase(Phase::WaitingForKeyframe);
                continue;
            }
        }
        match sink.submit(codec, &frame, pts, header.keyframe) {
            Ok(()) => {
                stats.frames_submitted.fetch_add(1, Ordering::Relaxed);
                stats.set_phase(Phase::Streaming);
            }
            Err(err) => {
                // A frame the decoder could not take breaks the reference
                // chain just like a lost one.
                stats.set_error(format!("decoder: {err}"));
                gate = KeyframeGate::default();
                stats.set_phase(Phase::WaitingForKeyframe);
            }
        }
        sink.service();
    }
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emersia_transport::transport::udp::HostEndpoint;
    use std::sync::Arc;

    fn header(frame_sequence: u32, frag_count: u16, keyframe: bool) -> FrameHeader {
        FrameHeader {
            version: 1,
            keyframe,
            payload_type: Codec::H264.payload_type(),
            sequence: frame_sequence + u32::from(frag_count),
            frame_sequence,
            timestamp: 0,
            payload_len: 0,
            frag_index: frag_count - 1,
            frag_count,
        }
    }

    #[test]
    fn gate_waits_for_first_keyframe() {
        let mut gate = KeyframeGate::default();
        assert_eq!(gate.admit(&header(0, 1, false)), Gate::Skip { lost: false });
        assert_eq!(gate.admit(&header(2, 3, true)), Gate::Pass);
        assert_eq!(gate.admit(&header(6, 1, false)), Gate::Pass);
    }

    #[test]
    fn gate_holds_after_loss_until_next_keyframe() {
        let mut gate = KeyframeGate::default();
        assert_eq!(gate.admit(&header(0, 1, true)), Gate::Pass);
        // Frame at sequence 2 (1 fragment) lost; the next one is 4.
        assert_eq!(gate.admit(&header(4, 1, false)), Gate::Skip { lost: true });
        assert_eq!(gate.admit(&header(6, 1, false)), Gate::Skip { lost: false });
        assert!(!gate.synced());
        assert_eq!(gate.admit(&header(8, 2, true)), Gate::Pass);
        assert_eq!(gate.admit(&header(11, 1, false)), Gate::Pass);
    }

    #[test]
    fn gate_handles_sequence_wrap() {
        let mut gate = KeyframeGate::default();
        assert_eq!(gate.admit(&header(u32::MAX - 1, 1, true)), Gate::Pass);
        // MAX-1 + 1 fragment + 1 = 0 after wrapping.
        assert_eq!(gate.admit(&header(0, 1, false)), Gate::Pass);
    }

    #[test]
    fn pts_clock_is_monotonic_across_wrap() {
        let mut clock = PtsClock::default();
        assert_eq!(clock.micros(u32::MAX - 899), 0);
        // 1800 ticks later, across the wrap: 20 ms.
        assert_eq!(clock.micros(900), 20_000);
    }

    #[derive(Default)]
    struct Recorder {
        frames: Vec<(Vec<u8>, bool)>,
        resets: usize,
    }

    impl VideoSink for Recorder {
        fn submit(&mut self, _: Codec, au: &[u8], _: u64, keyframe: bool) -> Result<(), String> {
            self.frames.push((au.to_vec(), keyframe));
            Ok(())
        }
        fn reset(&mut self) {
            self.resets += 1;
        }
    }

    /// End to end over loopback against the real host endpoint: handshake,
    /// a fragmented keyframe, a delta frame — and nothing before the keyframe.
    #[test]
    fn receives_frames_from_a_real_host_endpoint() {
        let host_identity = DeviceIdentity::generate().unwrap();
        let host_key = host_identity.public_key_bytes();
        let mut host = HostEndpoint::bind("127.0.0.1:0", host_identity).unwrap();
        let server = host.local_addr().unwrap().to_string();

        let device = DeviceIdentity::generate().unwrap();
        let device_pub = device.public_key_bytes();
        let device_id = device_id_bytes(&device.public_key_hex());

        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let client = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::spawn(move || {
                let mut sink = Recorder::default();
                let config = Config { server, host_key };
                run(&config, &device, &mut sink, &stop, &stats);
                tx.send(sink).unwrap();
            })
        };

        // Pump the handshake through.
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.peer_count() == 0 {
            assert!(Instant::now() < deadline, "handshake did not complete");
            host.pump(16, |id, pk| id == &device_id && pk == &device_pub)
                .unwrap();
        }
        let pt = Codec::H264.payload_type();
        let keyframe = vec![0xAB; 5000]; // spans several datagrams
        host.send_frame(&device_id, 0, false, pt, b"delta-before-key")
            .unwrap();
        host.send_frame(&device_id, 3000, true, pt, &keyframe)
            .unwrap();
        host.send_frame(&device_id, 6000, false, pt, b"delta")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while stats.frames_submitted.load(Ordering::Relaxed) < 2 {
            assert!(Instant::now() < deadline, "frames did not arrive");
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        client.join().unwrap();
        let sink = rx.recv().unwrap();

        assert_eq!(sink.resets, 1);
        assert_eq!(
            sink.frames,
            vec![(keyframe, true), (b"delta".to_vec(), false)],
            "pre-keyframe delta must be held back"
        );
        assert_eq!(stats.frames_skipped.load(Ordering::Relaxed), 1);
        assert_eq!(stats.losses.load(Ordering::Relaxed), 0);
        assert_eq!(stats.phase(), Phase::Stopped);
    }

    #[test]
    fn unreachable_host_reports_an_error_and_stops_cleanly() {
        // Bound but silent: the handshake times out.
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let config = Config {
            server: silent.local_addr().unwrap().to_string(),
            host_key: [9; 32],
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let worker = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::spawn(move || {
                let device = DeviceIdentity::generate().unwrap();
                run(&config, &device, &mut Recorder::default(), &stop, &stats);
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while stats.last_error().is_none() {
            assert!(Instant::now() < deadline, "no error surfaced");
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(stats.last_error().unwrap().contains("no answer"));
        assert_eq!(stats.phase(), Phase::Stopped);
    }
}

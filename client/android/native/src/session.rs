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
use emersia_transport::input::{self, InputEvent};
use emersia_transport::transport::udp::ClientEndpoint;
use emersia_transport::transport::{control, FrameHeader, Reassembler, VIDEO_CLOCK_HZ};
use std::sync::mpsc::Receiver;

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
/// Minimum spacing of keyframe requests while out of sync. The host rate-limits
/// too; this just keeps a loss burst from becoming a request burst.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(300);
/// How often the client probes the host clock.
const PING_INTERVAL: Duration = Duration::from_secs(1);
/// Clock samples kept; the lowest-RTT one wins (its offset error is smallest).
const CLOCK_SAMPLES: usize = 8;
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
    pub keyframe_requests: AtomicU64,
    pub input_events: AtomicU64,
    /// Smoothed host capture → frame fully received on the headset, µs.
    /// Zero until the clock offset is known.
    pub capture_to_rx_us: AtomicU64,
    /// Smoothed decoder latency (queued → output ready), µs.
    pub decode_us: AtomicU64,
    /// Best recent round-trip time to the host, µs.
    pub rtt_us: AtomicU64,
    last_error: Mutex<Option<String>>,
    decoder: Mutex<Option<String>>,
    /// The address currently in use (changes when the host moves).
    server: Mutex<String>,
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

    pub fn set_server(&self, server: &str) {
        *self.server.lock().unwrap_or_else(|p| p.into_inner()) = server.to_string();
    }

    pub fn server(&self) -> String {
        self.server
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
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

    /// Account for a non-video record (e.g. PONG) in the sequence space, so
    /// it is not mistaken for a lost frame. Returns whether it revealed a gap.
    pub fn observe_other(&mut self, header: &FrameHeader) -> bool {
        let lost = self
            .expected
            .is_some_and(|expected| expected != header.frame_sequence);
        self.expected = Some(
            header
                .frame_sequence
                .wrapping_add(u32::from(header.frag_count))
                .wrapping_add(1),
        );
        if lost {
            self.synced = false;
        }
        lost
    }
}

/// Exponentially smoothed value (1/8 weight), stored as µs in an atomic.
pub fn smooth(slot: &AtomicU64, sample_us: u64) {
    let old = slot.load(Ordering::Relaxed);
    let new = if old == 0 {
        sample_us
    } else {
        (old * 7 + sample_us) / 8
    };
    slot.store(new.max(1), Ordering::Relaxed);
}

/// Host ↔ device clock offset from ping/pong pairs (NTP-style).
#[derive(Debug, Default)]
pub struct ClockSync {
    /// `(rtt_us, host_minus_device_us)` of recent probes.
    samples: std::collections::VecDeque<(u64, i64)>,
}

impl ClockSync {
    /// Record a reply: probe sent at `sent_us`, answered with `host_us`,
    /// received at `now_us` (device clock).
    pub fn sample(&mut self, sent_us: u64, host_us: u64, now_us: u64) -> Option<u64> {
        let rtt = now_us.checked_sub(sent_us)?;
        // The host read its clock roughly half way through the round trip.
        let offset = host_us as i64 - (sent_us + rtt / 2) as i64;
        if self.samples.len() == CLOCK_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((rtt, offset));
        Some(rtt)
    }

    /// `host - device` in µs, from the lowest-RTT recent sample.
    pub fn offset(&self) -> Option<i64> {
        self.samples.iter().min_by_key(|(rtt, _)| *rtt).map(|s| s.1)
    }

    pub fn best_rtt(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.0).min()
    }
}

/// Unwraps the 32-bit 90 kHz timestamp into absolute host microseconds since
/// the host's stream epoch (the clock PONG replies use).
#[derive(Debug, Default)]
struct HostClock {
    last: Option<u32>,
    wraps: u64,
}

impl HostClock {
    fn micros(&mut self, timestamp: u32) -> u64 {
        if let Some(last) = self.last {
            // A large backwards jump is a wrap; small ones are reordering.
            if timestamp < last && last - timestamp > 0x8000_0000 {
                self.wraps += 1;
            }
        }
        self.last = Some(timestamp);
        ((self.wraps << 32) + u64::from(timestamp)) * 1_000_000 / u64::from(VIDEO_CLOCK_HZ)
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
    input: Option<&Receiver<InputEvent>>,
    stop: &AtomicBool,
    stats: &Stats,
) {
    run_following(config, identity, sink, input, None, stop, stats)
}

/// The newest presence announcement from `host_key` waiting on `discovery`,
/// as the address to try. Older or foreign announcements are discarded.
pub fn follow_presence(discovery: &std::net::UdpSocket, host_key: &[u8; 32]) -> Option<String> {
    let mut buf = [0u8; 64];
    let mut found = None;
    while let Ok((n, from)) = discovery.recv_from(&mut buf) {
        if let Some((key, port)) = emersia_transport::pairing::parse_presence(&buf[..n]) {
            if &key == host_key {
                found = Some(std::net::SocketAddr::new(from.ip(), port).to_string());
            }
        }
    }
    found
}

/// [`run`], plus: when the pinned address stops answering, follow the
/// pinned host key's presence announcements (on `discovery`) to its new
/// address. The handshake still authenticates against the pinned key, so a
/// forged announcement cannot redirect the stream anywhere useful.
pub fn run_following(
    config: &Config,
    identity: &DeviceIdentity,
    sink: &mut dyn VideoSink,
    input: Option<&Receiver<InputEvent>>,
    discovery: Option<&std::net::UdpSocket>,
    stop: &AtomicBool,
    stats: &Stats,
) {
    let device_id = device_id_bytes(&identity.public_key_hex());
    let mut backoff = BACKOFF_MIN;
    let mut config = Config {
        server: config.server.clone(),
        host_key: config.host_key,
    };
    stats.set_server(&config.server);
    if let Some(d) = discovery {
        let _ = d.set_nonblocking(true);
    }
    while !stop.load(Ordering::Relaxed) {
        stats.set_phase(Phase::Connecting);
        match connect(&config, device_id, identity, stats) {
            Ok(client) => {
                backoff = BACKOFF_MIN;
                stats.sessions.fetch_add(1, Ordering::Relaxed);
                sink.reset();
                // Input queued while disconnected is stale; never replay it.
                if let Some(rx) = input {
                    while rx.try_recv().is_ok() {}
                }
                stream(client, sink, input, stop, stats);
            }
            Err(err) => {
                stats.set_error(err);
                sleep_unless_stopped(backoff, stop);
                backoff = (backoff * 2).min(BACKOFF_MAX);
                if let Some(moved) = discovery.and_then(|d| follow_presence(d, &config.host_key)) {
                    if moved != config.server {
                        log::info!("host moved: {} -> {moved}", config.server);
                        config.server = moved;
                        stats.set_server(&config.server);
                        backoff = BACKOFF_MIN;
                    }
                }
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

/// Drain queued input into as few records as possible. Consecutive pointer
/// moves collapse to the newest, since only the final position matters.
pub fn batch_input(rx: &Receiver<InputEvent>) -> Vec<Vec<InputEvent>> {
    let mut batches = Vec::new();
    let mut current: Vec<InputEvent> = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let (Some(InputEvent::Pointer { .. }), InputEvent::Pointer { .. }) =
            (current.last(), event)
        {
            *current.last_mut().expect("checked") = event;
            continue;
        }
        if current.len() == input::MAX_EVENTS {
            batches.push(std::mem::take(&mut current));
        }
        current.push(event);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

fn send_input(client: &mut ClientEndpoint, rx: &Receiver<InputEvent>, stats: &Stats) {
    for batch in batch_input(rx) {
        let Ok(payload) = input::encode(&batch) else {
            continue; // only invalid key codes fail; drop the batch
        };
        match client.send_control(control::INPUT, &payload) {
            Ok(()) => {
                stats
                    .input_events
                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
            }
            Err(e) => log::warn!("input: {e}"),
        }
    }
}

fn stream(
    mut client: ClientEndpoint,
    sink: &mut dyn VideoSink,
    input: Option<&Receiver<InputEvent>>,
    stop: &AtomicBool,
    stats: &Stats,
) {
    stats.set_phase(Phase::WaitingForKeyframe);
    let mut reassembler = Reassembler::default();
    let mut gate = KeyframeGate::default();
    let mut clock = PtsClock::default();
    let mut last_datagram = Instant::now();
    let mut last_request: Option<Instant> = None;
    let epoch = Instant::now();
    let now_us = || epoch.elapsed().as_micros() as u64;
    let mut clock_sync = ClockSync::default();
    let mut host_clock = HostClock::default();
    let mut last_ping: Option<Instant> = None;
    // Ask for an IDR, at most once per interval.
    let mut request_keyframe = |client: &mut ClientEndpoint| {
        if last_request.is_some_and(|t| t.elapsed() < KEYFRAME_REQUEST_INTERVAL) {
            return;
        }
        last_request = Some(Instant::now());
        match client.send_control(control::KEYFRAME_REQUEST, &[]) {
            Ok(()) => {
                stats.keyframe_requests.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => log::warn!("keyframe request: {e}"),
        }
    };

    while !stop.load(Ordering::Relaxed) {
        // At most one READ_SLICE (2 ms) between an input event and its send.
        if let Some(rx) = input {
            send_input(&mut client, rx, stats);
        }
        if last_ping.is_none_or(|t| t.elapsed() >= PING_INTERVAL) {
            last_ping = Some(Instant::now());
            let _ = client.send_control(control::PING, &now_us().to_be_bytes());
            log::debug!(
                "latency capture→rx {} µs, decode {} µs, rtt {} µs, frames {}",
                stats.capture_to_rx_us.load(Ordering::Relaxed),
                stats.decode_us.load(Ordering::Relaxed),
                stats.rtt_us.load(Ordering::Relaxed),
                stats.frames_submitted.load(Ordering::Relaxed),
            );
        }
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
            if header.payload_type == control::PONG {
                if let Some((sent, host_us)) = control::parse_pong(&payload) {
                    if clock_sync.sample(sent, host_us, now_us()).is_some() {
                        if let Some(rtt) = clock_sync.best_rtt() {
                            stats.rtt_us.store(rtt.max(1), Ordering::Relaxed);
                        }
                    }
                }
            }
            if gate.observe_other(&header) {
                stats.losses.fetch_add(1, Ordering::Relaxed);
            }
            continue; // not video
        };
        let Some(frame) = reassembler.push(header, &payload) else {
            continue;
        };
        stats.frames_received.fetch_add(1, Ordering::Relaxed);
        let pts = clock.micros(header.timestamp);
        let captured_host_us = host_clock.micros(header.timestamp);
        if let Some(offset) = clock_sync.offset() {
            // Capture time on the device clock, then how long ago that was.
            let captured_device_us = captured_host_us as i64 - offset;
            let latency = now_us() as i64 - captured_device_us;
            if (0..10_000_000).contains(&latency) {
                smooth(&stats.capture_to_rx_us, latency as u64);
            }
        }
        match gate.admit(&header) {
            Gate::Pass => {}
            Gate::Skip { lost } => {
                if lost {
                    stats.losses.fetch_add(1, Ordering::Relaxed);
                }
                stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
                stats.set_phase(Phase::WaitingForKeyframe);
                request_keyframe(&mut client);
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
                request_keyframe(&mut client);
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
    fn input_batches_coalesce_pointer_moves_only() {
        let (tx, rx) = std::sync::mpsc::channel();
        use emersia_transport::input::Button;
        for e in [
            InputEvent::Pointer { x: 1, y: 1 },
            InputEvent::Pointer { x: 2, y: 2 },
            InputEvent::Button {
                button: Button::Left,
                down: true,
            },
            InputEvent::Pointer { x: 3, y: 3 },
            InputEvent::Pointer { x: 4, y: 4 },
            InputEvent::Button {
                button: Button::Left,
                down: false,
            },
        ] {
            tx.send(e).unwrap();
        }
        assert_eq!(
            batch_input(&rx),
            vec![vec![
                InputEvent::Pointer { x: 2, y: 2 },
                InputEvent::Button {
                    button: Button::Left,
                    down: true
                },
                InputEvent::Pointer { x: 4, y: 4 },
                InputEvent::Button {
                    button: Button::Left,
                    down: false
                },
            ]]
        );
        // Long runs of non-pointer events split into bounded records.
        for _ in 0..(input::MAX_EVENTS + 5) {
            tx.send(InputEvent::Scroll { dx: 0, dy: 120 }).unwrap();
        }
        let batches = batch_input(&rx);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), input::MAX_EVENTS);
        assert_eq!(batches[1].len(), 5);
    }

    #[test]
    fn clock_sync_prefers_the_lowest_rtt_sample() {
        let mut sync = ClockSync::default();
        // Host clock is device + 1_000_000 µs. Symmetric 10 ms RTT.
        assert_eq!(sync.sample(0, 1_005_000, 10_000), Some(10_000));
        assert_eq!(sync.offset(), Some(1_000_000));
        // A slow, asymmetric probe (reply delayed) has a worse estimate...
        sync.sample(100_000, 1_102_000, 190_000);
        // ...but the 10 ms one still wins.
        assert_eq!(sync.offset(), Some(1_000_000));
        assert_eq!(sync.best_rtt(), Some(10_000));
        // A reply from before the probe is ignored.
        assert_eq!(sync.sample(500, 0, 100), None);
    }

    #[test]
    fn host_clock_unwraps_timestamps() {
        let mut c = HostClock::default();
        assert_eq!(c.micros(90_000), 1_000_000);
        let before_wrap = c.micros(u32::MAX);
        let after_wrap = c.micros(89); // 90 ticks later
        assert_eq!(after_wrap - before_wrap, 1_000);
    }

    #[test]
    fn gate_counts_non_video_records_in_the_sequence() {
        let mut gate = KeyframeGate::default();
        assert_eq!(gate.admit(&header(0, 1, true)), Gate::Pass);
        // A PONG takes sequence 2; the next video frame at 4 is not a loss.
        assert!(!gate.observe_other(&header(2, 1, false)));
        assert_eq!(gate.admit(&header(4, 1, false)), Gate::Pass);
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
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        let client = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::spawn(move || {
                let mut sink = Recorder::default();
                let config = Config { server, host_key };
                run(&config, &device, &mut sink, Some(&input_rx), &stop, &stats);
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
        // Input from the "UI thread" reaches the host as one decoded record.
        let click = [
            InputEvent::Pointer { x: 100, y: 200 },
            InputEvent::Button {
                button: emersia_transport::input::Button::Left,
                down: true,
            },
        ];
        for e in click {
            input_tx.send(e).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut records = Vec::new();
        let mut control_seen = Vec::new();
        while Instant::now() < deadline {
            host.pump(16, |_, _| false).unwrap();
            let batch = host.take_control();
            records.extend(
                batch
                    .iter()
                    .filter(|r| r.payload_type == control::INPUT)
                    .cloned(),
            );
            control_seen.extend(batch);
            if !records.is_empty() {
                break;
            }
        }
        assert_eq!(records.len(), 1);
        assert_eq!(input::decode(&records[0].payload).unwrap(), click.to_vec());

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
        // The skipped pre-keyframe delta made the client ask for an IDR.
        assert_eq!(stats.keyframe_requests.load(Ordering::Relaxed), 1);
        let requests: Vec<_> = control_seen
            .iter()
            .filter(|r| r.payload_type == control::KEYFRAME_REQUEST)
            .collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].device_id, device_id);
        assert_eq!(stats.losses.load(Ordering::Relaxed), 0);
        assert_eq!(stats.phase(), Phase::Stopped);
    }

    /// The pinned address is dead; a presence announcement from the pinned
    /// key leads the client to the host's new address.
    #[test]
    fn follows_the_pinned_host_to_a_new_address() {
        let host_identity = DeviceIdentity::generate().unwrap();
        let host_key = host_identity.public_key_bytes();
        let mut host = HostEndpoint::bind("127.0.0.1:0", host_identity).unwrap();
        let new_port = host.local_addr().unwrap().port();
        let dead = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let discovery = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let discovery_addr = discovery.local_addr().unwrap();
        let announcer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        // A foreign key's announcement must be ignored.
        let _ = announcer.send_to(
            &emersia_transport::pairing::presence(&[1; 32], 9),
            discovery_addr,
        );
        announcer
            .send_to(
                &emersia_transport::pairing::presence(&host_key, new_port),
                discovery_addr,
            )
            .unwrap();

        let device = DeviceIdentity::generate().unwrap();
        let device_pub = device.public_key_bytes();
        let device_id = device_id_bytes(&device.public_key_hex());
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let worker = {
            let (stop, stats) = (stop.clone(), stats.clone());
            let server = dead.local_addr().unwrap().to_string();
            std::thread::spawn(move || {
                let config = Config { server, host_key };
                run_following(
                    &config,
                    &device,
                    &mut Recorder::default(),
                    None,
                    Some(&discovery),
                    &stop,
                    &stats,
                );
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.peer_count() == 0 {
            assert!(
                Instant::now() < deadline,
                "client never reached the moved host"
            );
            host.pump(16, |id, pk| id == &device_id && pk == &device_pub)
                .unwrap();
        }
        assert_eq!(stats.server(), format!("127.0.0.1:{new_port}"));
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
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
                run(
                    &config,
                    &device,
                    &mut Recorder::default(),
                    None,
                    &stop,
                    &stats,
                );
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

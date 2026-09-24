//! Daemon service state machine and capture engine.
//!
//! The engine (main thread) owns the Wayland connection and runs a capture
//! loop; the control server runs on its own thread and mutates the shared
//! [`Service`] state. `start`/`stop` flip a flag the engine observes, so no
//! Wayland object ever crosses a thread boundary.
//!
//! M1.2 scope: this captures and measures frames. Encoding, transport and
//! uinput land in later subtasks of #3.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use emersia_protocol::{
    Command, DeviceInfo, ErrorCode, PairAction, Request, Response, PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::json;
use wayland_client::Connection;

use crate::capture::{self, Backend, BackendPref};
use crate::control::{err_response, ok_response};
use crate::pairing::{PairingDb, PairingError};
use crate::transport::udp::HostEndpoint;
use crate::transport::VIDEO_CLOCK_HZ;

/// Decode a hex device id back to bytes.
fn decode_device_id(hex_str: &str) -> Option<[u8; 8]> {
    if hex_str.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Default capture rate when `start` does not specify one.
const DEFAULT_FPS: u32 = 30;

/// Engine idle tick — how often a stopped engine checks for new commands.
const IDLE_TICK: Duration = Duration::from_millis(20);

/// A capturable output as reported by `screens`.
#[derive(Debug, Clone, Serialize)]
pub struct ScreenInfo {
    pub kind: &'static str,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub transform: String,
}

/// Live counters, updated by the engine after every captured frame.
#[derive(Debug, Default, Clone)]
struct Stats {
    frames: u64,
    last_latency_ms: f64,
    avg_latency_ms: f64,
    measured_fps: f64,
}

/// Mutable service state, guarded by one mutex.
#[derive(Debug, Default)]
struct Inner {
    streaming: bool,
    fps_target: u32,
    selected: Option<String>,
    outputs: Vec<ScreenInfo>,
    backend: Option<&'static str>,
    stats: Stats,
    last_error: Option<String>,
    /// Devices currently connected to the streaming endpoint.
    stream_peers: usize,
    /// Devices revoked since the engine last drained; the engine disconnects
    /// them, which is how "revoke kills an active session" is honoured.
    revoked_pending: Vec<[u8; 8]>,
    /// Encrypted packets handed to the transport since start.
    encoded_packets: u64,
    /// The ffmpeg encoder currently in use, for status reporting.
    encoder_name: Option<String>,
    /// True when the encoder in use is the software fallback.
    encoder_software: bool,
    /// Frames handed to the encoder.
    encoder_frames_in: u64,
    /// Access units the encoder has produced.
    encoder_frames_out: u64,
}

/// Shared control-plane state. Cheap to clone via `Arc`; the Wayland machinery
/// stays on the engine thread.
///
/// **Locking rule:** never hold both mutexes at once. Read or mutate one,
/// drop it, then take the other. `status_payload` shows the safe shape.
///
/// The two locks are only ever needed together to build a response. Acquiring
/// them in different orders in two code paths is a deadlock, not a race: each
/// holds one and waits for the other. An earlier version of this file did
/// exactly that and wedged `status` against `revoke` (see issue #26).
#[derive(Debug)]
pub struct Service {
    inner: Mutex<Inner>,
    shutdown: AtomicBool,
    /// File-backed device trust store, behind its own lock so a disk write
    /// never blocks the status path.
    pairing: Mutex<PairingDb>,
    /// The daemon's own long-term identity (ADR 0006). Devices need this to
    /// pin the host they are talking to.
    host_public_key: String,
}

impl Service {
    pub fn new(pairing: PairingDb, host_public_key_hex: &str) -> Self {
        Self {
            inner: Mutex::new(Inner {
                fps_target: DEFAULT_FPS,
                ..Inner::default()
            }),
            shutdown: AtomicBool::new(false),
            pairing: Mutex::new(pairing),
            host_public_key: host_public_key_hex.to_string(),
        }
    }

    /// A service with no Wayland discovery, for socket-layer tests.
    #[cfg(test)]
    pub fn new_for_tests() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("temp dir for test trust store");
        let db = PairingDb::load(&dir.path().join("pairing.json")).expect("fresh store");
        let host = crate::crypto::HostIdentity::load_or_create(&dir.path().join("host.key"))
            .expect("host identity");
        let host_hex = host.public_key_hex();
        // The store writes to this path for the life of the test process.
        std::mem::forget(dir);
        Arc::new(Self::new(db, &host_hex))
    }

    pub(crate) fn lock_pairing(&self) -> std::sync::MutexGuard<'_, PairingDb> {
        self.pairing.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record that `count` encrypted packets were shipped.
    pub fn record_encoded(&self, count: u32) {
        let mut inner = self.lock();
        inner.encoded_packets += u64::from(count);
    }

    /// Report the encoder in use and its frame counters.
    pub fn set_encoder(
        &self,
        name: Option<String>,
        software: bool,
        frames_in: u64,
        frames_out: u64,
    ) {
        let mut inner = self.lock();
        inner.encoder_name = name;
        inner.encoder_software = software;
        inner.encoder_frames_in = frames_in;
        inner.encoder_frames_out = frames_out;
    }

    /// The frame rate capture is actually achieving.
    pub fn measured_fps(&self) -> f64 {
        self.lock().stats.measured_fps
    }

    /// Record how many devices are connected to the streaming endpoint.
    pub fn set_stream_peers(&self, count: usize) {
        self.lock().stream_peers = count;
    }

    /// Take device ids revoked since the last call, so the engine can drop
    /// their live sessions.
    pub fn take_revoked(&self) -> Vec<[u8; 8]> {
        std::mem::take(&mut self.lock().revoked_pending)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a prior thread panicked mid-update; the state
        // is plain data with no invariants spanning fields, so recovering is
        // safer than taking the whole daemon down.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Populate outputs/backend discovered by the engine at startup.
    pub fn publish_discovery(&self, outputs: Vec<ScreenInfo>, backend: Option<&'static str>) {
        let mut inner = self.lock();
        inner.outputs = outputs;
        inner.backend = backend;
    }

    pub fn is_streaming(&self) -> bool {
        self.lock().streaming
    }

    pub fn fps_target(&self) -> u32 {
        self.lock().fps_target.max(1)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Record one successfully captured frame.
    pub fn record_frame(&self, latency: Duration, since_last: Duration) {
        let mut inner = self.lock();
        let ms = latency.as_secs_f64() * 1000.0;
        inner.stats.frames += 1;
        inner.stats.last_latency_ms = ms;
        inner.stats.avg_latency_ms = if inner.stats.frames == 1 {
            ms
        } else {
            // Running mean so a long session cannot drift.
            let n = inner.stats.frames as f64;
            (inner.stats.avg_latency_ms * (n - 1.0) + ms) / n
        };
        inner.stats.measured_fps = if since_last.is_zero() {
            0.0
        } else {
            1.0 / since_last.as_secs_f64()
        };
        inner.last_error = None;
    }

    /// Record a capture failure and drop out of streaming.
    pub fn record_error(&self, message: String) {
        let mut inner = self.lock();
        inner.streaming = false;
        inner.last_error = Some(message);
    }

    /// Handle one control request and produce its response.
    pub fn handle(&self, req: Request) -> Response {
        let id = req.id.clone();
        match req.command {
            Command::Status => ok_response(&id, self.status_payload()),
            Command::Screens => ok_response(&id, json!({ "screens": self.screens_payload() })),
            Command::Start(args) => self.handle_start(&id, args),
            Command::Stop => {
                let was = {
                    let mut inner = self.lock();
                    let was = inner.streaming;
                    inner.streaming = false;
                    was
                };
                if was {
                    ok_response(&id, json!({ "streaming": false }))
                } else {
                    err_response(&id, ErrorCode::Busy, "not streaming; nothing to stop")
                }
            }
            Command::Events => {
                // Follow-mode push is deferred; send the current snapshot so
                // `emersia events` is useful today.
                ok_response(
                    &id,
                    json!({ "snapshot": self.status_payload(), "following": false }),
                )
            }
            Command::Unknown => err_response(&id, ErrorCode::UnknownCmd, "unknown command"),
            Command::Pair(args) => self.handle_pair(&id, args),
            Command::Devices => {
                let devices: Vec<DeviceInfo> = self
                    .lock_pairing()
                    .devices()
                    .iter()
                    .map(|d| DeviceInfo {
                        id: d.id.clone(),
                        name: d.name.clone(),
                        paired_at: d.paired_at,
                        revoked: d.revoked,
                    })
                    .collect();
                let active = devices.iter().filter(|d| !d.revoked).count();
                ok_response(&id, json!({ "devices": devices, "active": active }))
            }
            Command::Revoke(args) => {
                // Scope the pairing guard so it is released before `inner` is
                // taken; see `status_payload` for the lock order.
                let outcome = self.lock_pairing().revoke(&args.device);
                match outcome {
                    Ok(device) => {
                        // Hand the id to the engine so the live session dies now.
                        if let Some(bytes) = decode_device_id(&device.id) {
                            self.lock().revoked_pending.push(bytes);
                        }
                        ok_response(&id, json!({ "revoked": true, "device": device.id }))
                    }
                    Err(e) => err_response(&id, pairing_error_code(&e), e.to_string()),
                }
            }
            Command::Select(args) => {
                if args.targets.is_empty() {
                    let mut inner = self.lock();
                    inner.streaming = false;
                    inner.selected = None;
                    ok_response(&id, json!({ "targets": [] }))
                } else {
                    err_response(
                        &id,
                        ErrorCode::InvalidArgs,
                        "multi-target select is not implemented yet; use start --output",
                    )
                }
            }
        }
    }

    /// `pair new` mints a short-lived code; `pair accept <code>` registers a
    /// device identity against it.
    fn handle_pair(&self, id: &str, args: emersia_protocol::PairArgs) -> Response {
        let mut db = self.lock_pairing();
        match args.action {
            PairAction::New => match db.new_code() {
                Ok(code) => ok_response(
                    id,
                    json!({
                        "code": code.to_string(),
                        "expires_in_s": crate::pairing::CODE_TTL.as_secs(),
                    }),
                ),
                Err(e) => err_response(id, pairing_error_code(&e), e.to_string()),
            },
            PairAction::Accept => {
                let (Some(code), Some(name), Some(public_key)) =
                    (args.code, args.name, args.public_key)
                else {
                    return err_response(
                        id,
                        ErrorCode::InvalidArgs,
                        "pair accept needs a code, a device name and a public key",
                    );
                };
                match db.accept_code(&code, &name, &public_key) {
                    Ok(device) => ok_response(
                        id,
                        json!({ "paired": true, "device": device.id, "name": device.name }),
                    ),
                    Err(e) => err_response(id, pairing_error_code(&e), e.to_string()),
                }
            }
        }
    }

    fn handle_start(&self, id: &str, args: emersia_protocol::StartArgs) -> Response {
        let mut inner = self.lock();
        if inner.streaming {
            return err_response(id, ErrorCode::Busy, "already streaming");
        }
        // Validate the requested output before claiming success.
        if let Some(want) = &args.output {
            let known = inner.outputs.iter().any(|o| &o.name == want);
            if !known {
                let names: Vec<&str> = inner.outputs.iter().map(|o| o.name.as_str()).collect();
                return err_response(
                    id,
                    ErrorCode::NotFound,
                    format!("no output named {want:?}; available: {}", names.join(", ")),
                );
            }
        }
        if let Some(fps) = args.fps {
            if fps == 0 || fps > 240 {
                return err_response(
                    id,
                    ErrorCode::InvalidArgs,
                    format!("fps must be 1..=240, got {fps}"),
                );
            }
            inner.fps_target = fps;
        }
        if args.output.is_some() {
            inner.selected = args.output.clone();
        }
        if inner.selected.is_none() {
            inner.selected = inner.outputs.first().map(|o| o.name.clone());
        }
        if inner.selected.is_none() {
            return err_response(
                id,
                ErrorCode::NoCaptureBackend,
                "no outputs detected on this compositor",
            );
        }
        inner.streaming = true;
        inner.last_error = None;
        let selected = inner.selected.clone().unwrap_or_default();
        ok_response(
            id,
            json!({ "streaming": true, "output": selected, "fps": inner.fps_target }),
        )
    }

    fn screens_payload(&self) -> Vec<ScreenInfo> {
        self.lock().outputs.clone()
    }

    fn status_payload(&self) -> serde_json::Value {
        // Never hold both locks: take the pairing read, drop it, then lock
        // `inner`. See the locking rule on `Service`.
        let paired_devices = self.lock_pairing().active_devices().len();
        let inner = self.lock();
        json!({
            "streaming": inner.streaming,
            "backend": inner.backend,
            "selected_output": inner.selected,
            "fps_target": inner.fps_target,
            "frames": inner.stats.frames,
            "latency_ms": round2(inner.stats.last_latency_ms),
            "avg_latency_ms": round2(inner.stats.avg_latency_ms),
            "measured_fps": round2(inner.stats.measured_fps),
            "connected_devices": inner.stream_peers,
            "paired_devices": paired_devices,
            "encoder": inner.encoder_name,
            "encoder_software": inner.encoder_software,
            "encoder_frames_in": inner.encoder_frames_in,
            "encoder_frames_out": inner.encoder_frames_out,
            "encoded_packets": inner.encoded_packets,
            "last_error": inner.last_error,
            "protocol_version": PROTOCOL_VERSION,
            "host_public_key": self.host_public_key,
        })
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Map a pairing failure onto a stable wire error token.
fn pairing_error_code(err: &PairingError) -> ErrorCode {
    use PairingError as P;
    match err {
        P::WrongCode | P::NoPendingCode | P::TooManyAttempts => ErrorCode::NotPaired,
        P::NotFound(_) => ErrorCode::NotFound,
        P::Invalid(_) => ErrorCode::InvalidArgs,
        // I/O and corruption are both server-side trust failures: refuse loudly.
        P::Io(_) | P::Corrupt(_) | P::NoConfigDir => ErrorCode::Internal,
    }
}

/// Everything the engine needs to capture, discovered once at startup.
pub struct Engine {
    queue: wayland_client::EventQueue<capture::State>,
    state: capture::State,
    /// Streaming endpoint; bound even when idle so a device can pair and wait.
    transport: Option<HostEndpoint>,
    /// Video encoder; started on the first streamed frame so the capture size
    /// and the encoder configuration are known before anything is spawned.
    encoder: Option<crate::encoder::Encoder>,
    /// Encoder selection requested by the user.
    encoder_choice: crate::encoder::EncoderChoice,
    /// Bitrate requested for the stream.
    bitrate: u32,
    /// The codec in force, once an encoder exists.
    codec: crate::codec::Codec,
    /// Rolling estimate of the capture rate actually being achieved.
    measured_fps: f64,
    output: wayland_client::protocol::wl_output::WlOutput,
    transform: wayland_client::protocol::wl_output::Transform,
    backend: Backend,
    pub backend_name: &'static str,
    pub screens: Vec<ScreenInfo>,
}

impl Engine {
    /// Connect to Wayland, enumerate outputs, and pick a backend/output.
    /// Bind the streaming endpoint, if a port was requested.
    ///
    /// `identity` must be the host's persisted long-term identity: devices pin
    /// that exact public key, and a different one would fail verification.
    pub fn with_stream_port(
        mut self,
        port: Option<u16>,
        identity: crate::crypto::DeviceIdentity,
    ) -> anyhow::Result<Self> {
        let Some(port) = port else {
            self.transport = None;
            return Ok(self);
        };
        self.transport = Some(
            HostEndpoint::bind(&format!("0.0.0.0:{port}"), identity)
                .map_err(|e| anyhow::anyhow!("could not bind UDP port {port}: {e}"))?,
        );
        Ok(self)
    }

    /// The streaming endpoint, once bound.
    pub fn transport(&self) -> Option<&HostEndpoint> {
        self.transport.as_ref()
    }

    /// Choose the encoder family and bitrate. Does not start anything: the
    /// encoder is created on the first frame, when the capture geometry is
    /// known and an odd resolution can be rejected with a clear message.
    pub fn with_encoder(
        mut self,
        choice: crate::encoder::EncoderChoice,
        bitrate: Option<u32>,
    ) -> Self {
        self.encoder_choice = choice;
        if let Some(b) = bitrate {
            self.bitrate = b;
        }
        self
    }

    /// Record how long the last capture took, to estimate the real frame rate.
    fn note_capture(&mut self, since_last: Duration) {
        let instant = since_last.as_secs_f64();
        if instant <= 0.0 {
            return;
        }
        let sample = 1.0 / instant;
        // Light smoothing: enough to stop one slow frame from reconfiguring
        // the encoder, not so much that a real change is missed.
        self.measured_fps = if self.measured_fps <= 0.0 {
            sample
        } else {
            self.measured_fps * 0.8 + sample * 0.2
        };
    }

    /// The encoder in use, if one has been started.
    #[allow(dead_code)]
    pub fn encoder(&self) -> Option<&crate::encoder::Encoder> {
        self.encoder.as_ref()
    }

    /// Start the encoder for a given capture geometry.
    ///
    /// `target_fps` is what the user asked for; `measured_fps` is what capture
    /// is actually delivering. The encoder is told the **measured** rate,
    /// because its clock is derived from the declared input rate: telling it
    /// 60 fps while frames arrive at 2.5 makes every duration it computes 24x
    /// too long, including the keyframe interval.
    fn ensure_encoder(
        &mut self,
        width: u32,
        height: u32,
        target_fps: u32,
        measured_fps: f64,
    ) -> Result<bool, crate::encoder::EncoderError> {
        // Report a sane rate: never zero, never above what was asked for.
        let effective = if measured_fps >= 1.0 {
            (measured_fps.round() as u32).clamp(1, target_fps.max(1))
        } else {
            target_fps.max(1)
        };
        if let Some(enc) = &self.encoder {
            // A geometry change means the running encoder is wrong for the
            // new frames; restart rather than feed it mismatched bytes.
            if enc.config().width != width || enc.config().height != height {
                self.encoder = None;
            } else if (enc.config().fps as i64 - effective as i64).abs()
                > (effective as i64 / 4).max(1)
            {
                // The delivered rate moved far enough that the encoder's
                // timing is now wrong. Restarting also forces a keyframe,
                // which is convenient rather than incidental.
                self.encoder = None;
            } else {
                return Ok(true);
            }
        }
        let config = crate::encoder::EncoderConfig {
            choice: self.encoder_choice,
            codec: self.codec,
            bitrate: self.bitrate,
            // A 2-second GOP, counted in frames at the rate we are actually
            // achieving, so it stays 2 seconds whatever that rate is.
            gop: effective.saturating_mul(2).max(1),
            fps: effective,
            width,
            height,
        };
        let encoder = crate::encoder::Encoder::start(config)?;
        self.codec = encoder.candidate().codec;
        self.encoder = Some(encoder);
        Ok(true)
    }

    pub fn discover(pref: BackendPref) -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env()
            .context("failed to connect to a Wayland compositor (is WAYLAND_DISPLAY set?)")?;
        let mut queue = conn.new_event_queue::<capture::State>();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = capture::State::new();

        queue
            .roundtrip(&mut state)
            .context("failed while enumerating Wayland globals")?;
        queue
            .roundtrip(&mut state)
            .context("failed while reading output descriptions")?;

        let labels = state.output_labels();
        let has_ext = state.ext_source_mgr.is_some() && state.ext_copy_mgr.is_some();
        let has_wlr = state.wlr_mgr.is_some();
        let backend = capture::choose_backend(pref, has_ext, has_wlr)?;

        let index = capture::select_output(&labels, None)?;
        let output = state.outputs[index].proxy.clone();
        let transform = state.outputs[index].transform;
        let screens = state
            .outputs
            .iter()
            .enumerate()
            .map(|(i, info)| ScreenInfo {
                kind: "output",
                name: labels[i].clone(),
                width: info.width,
                height: info.height,
                transform: format!("{:?}", info.transform).to_lowercase(),
            })
            .collect();

        Ok(Self {
            queue,
            state,
            transport: None,
            encoder: None,
            encoder_choice: crate::encoder::EncoderChoice::Auto,
            bitrate: crate::encoder::EncoderConfig::default().bitrate,
            codec: crate::codec::Codec::H264,
            measured_fps: 0.0,
            output,
            transform,
            backend,
            backend_name: backend.protocol(),
            screens,
        })
    }

    /// Capture exactly one frame with the selected backend.
    pub fn capture_once(&mut self) -> anyhow::Result<crate::frame::Frame> {
        match self.backend {
            Backend::Ext => capture::ext::capture(&mut self.queue, &mut self.state, &self.output),
            Backend::Wlr => capture::wlr::capture(
                &mut self.queue,
                &mut self.state,
                &self.output,
                self.transform,
            ),
        }
    }
}

/// Run the engine until shutdown: capture while streaming, idle otherwise.
///
/// While a device is connected the captured frame is shipped over the
/// encrypted transport. Frames are sent as a downscaled RGBA preview: the
/// encoder stage has not landed yet, so a full 1080p RGBA frame would be ~8 MB
/// per frame and is not something to put on a wire. The preview proves the
/// path end to end; the encoder replaces it in M1.5.
pub fn run_engine(mut engine: Engine, service: Arc<Service>) {
    service.publish_discovery(engine.screens.clone(), Some(engine.backend_name));
    let mut last_frame = Instant::now();
    let mut timestamp: u32 = 0;
    let mut last_peers = 0usize;

    while !service.is_shutting_down() && !crate::signal_received() {
        // Always service the transport so a device can pair and connect even
        // when we are not capturing.
        if let Some(transport) = engine.transport.as_mut() {
            // A revoked device loses its session immediately.
            for id in service.take_revoked() {
                if transport.disconnect(&id) {
                    eprintln!(
                        "emersia-daemon: revoked device {} disconnected from the stream",
                        hex::encode(id)
                    );
                }
            }
            // The authorizer reads the trust store at the moment of the
            // decision rather than from a snapshot taken at the top of the
            // loop.
            //
            // A snapshot is stale for up to a whole engine iteration (a few
            // hundred ms), and a device that pairs and immediately connects
            // lands squarely in that window: it was refused as unpaired, and
            // because a client sends `init` only once, the connection failed
            // until the user retried. The lock is taken per handshake, not per
            // frame, so the cost is irrelevant next to the correctness.
            let mut allowed = |id: &[u8; 8], pubkey: &[u8; 32]| {
                let got_id = hex::encode(id);
                let got_key = hex::encode(pubkey);
                service.lock_pairing().devices().iter().any(|d| {
                    d.id == got_id && d.public_key.eq_ignore_ascii_case(&got_key) && !d.revoked
                })
            };
            if let Err(e) = transport.pump(16, &mut allowed) {
                eprintln!("emersia-daemon: stream pump error: {e}");
            }
            let peers = transport.peer_count();
            if peers > last_peers {
                // A device just connected. It has no prior frames, so it cannot
                // decode anything until a keyframe arrives. Dropping the
                // encoder makes the next frame an IDR, which is the only way to
                // force one over a raw ffmpeg pipe — there is no control
                // channel. Costs one encoder restart per connection.
                if last_peers > 0 || engine.encoder.is_some() {
                    eprintln!(
                        "emersia-daemon: new device connected, restarting encoder for a keyframe"
                    );
                }
                engine.encoder = None;
            }
            last_peers = peers;
            service.set_stream_peers(peers);
        }

        if !service.is_streaming() {
            std::thread::sleep(IDLE_TICK);
            last_frame = Instant::now();
            continue;
        }
        let started = Instant::now();
        match engine.capture_once() {
            Ok(frame) => {
                if engine
                    .transport
                    .as_ref()
                    .is_some_and(|t| t.peer_count() > 0)
                {
                    // Start or restart the encoder for this geometry. A failure
                    // is reported but does not stop the daemon: the user may be
                    // able to fix it by choosing another encoder from the CLI.
                    match engine.ensure_encoder(
                        frame.width,
                        frame.height,
                        service.fps_target().max(1),
                        service.measured_fps().max(engine.measured_fps),
                    ) {
                        Ok(true) => {
                            // 90 kHz presentation clock, per ADR 0003.
                            timestamp = timestamp.wrapping_add(
                                ((VIDEO_CLOCK_HZ as u64 / service.fps_target().max(1) as u64)
                                    as u32)
                                    .max(1),
                            );
                            if let Some(enc) = engine.encoder.as_mut() {
                                if let Err(e) = enc.encode(&frame.rgba) {
                                    service.record_error(format!("encode: {e}"));
                                    eprintln!("emersia-daemon: encode failed: {e}");
                                    engine.encoder = None;
                                }
                            }
                            // Ship every access unit that is ready. Each packet
                            // carries the parameter sets in-band (ADR 0003), so
                            // a device joining mid-stream can decode at once.
                            let ids = engine
                                .transport
                                .as_ref()
                                .map(|transport| transport.peer_ids())
                                .unwrap_or_default();
                            let mut shipped = 0u32;
                            while let Some(au) = engine
                                .encoder
                                .as_mut()
                                .and_then(crate::encoder::Encoder::next_access_unit)
                            {
                                let pt = au.codec_payload_type();
                                let keyframe = au.keyframe;
                                // Prefix the parameter sets so a delta packet is
                                // still self-describing.
                                let payload = engine
                                    .encoder
                                    .as_ref()
                                    .map(|e| e.payload_for(&au))
                                    .unwrap_or_else(|| au.annexb.clone());
                                for id in &ids {
                                    match engine.transport.as_mut().map(|t| {
                                        t.send_frame(id, timestamp, keyframe, pt, &payload)
                                    }) {
                                        Some(Ok(_)) => shipped += 1,
                                        Some(Err(e)) => {
                                            eprintln!(
                                                "emersia-daemon: send failed: {e} (id {})",
                                                hex::encode(id)
                                            );
                                        }
                                        None => eprintln!("emersia-daemon: no transport"),
                                    }
                                }
                            }
                            if shipped > 0 {
                                service.record_encoded(shipped);
                            }
                            if let Some(enc) = engine.encoder.as_ref() {
                                service.set_encoder(
                                    Some(enc.candidate().name.clone()),
                                    enc.is_software(),
                                    enc.frames_in(),
                                    enc.frames_out(),
                                );
                            }
                        }
                        Ok(false) => {}
                        Err(e) => {
                            // A missing ffmpeg is a setup problem, not a
                            // stream problem, and it is worth saying so plainly
                            // rather than logging it once per frame.
                            if e.ffmpeg_missing() {
                                service.record_error(
                                    "streaming needs the ffmpeg binary, which was not found on PATH"
                                        .to_string(),
                                );
                                eprintln!(
                                    "emersia-daemon: streaming requires ffmpeg on PATH. \
                                     Install it (e.g. `apt install ffmpeg`, `pacman -S ffmpeg`) \
                                     and restart the daemon; capture and the control socket keep \
                                     working without it."
                                );
                                service.request_shutdown();
                            } else {
                                service.record_error(format!("encoder: {e}"));
                                eprintln!("emersia-daemon: encoder unavailable: {e}");
                            }
                        }
                    }
                }
                // Pace to the requested rate; drop frames if capture is slow.
                // Sample timings before sleeping so pacing is not reported as
                // capture or frame-processing latency.
                let frame_latency = started.elapsed();
                let since_last = last_frame.elapsed();
                let interval = Duration::from_secs_f64(1.0 / service.fps_target() as f64);
                let elapsed = started.elapsed();
                if elapsed < interval {
                    std::thread::sleep(interval - elapsed);
                }
                service.record_frame(frame_latency, since_last);
                engine.note_capture(since_last);
                last_frame = Instant::now();
            }
            Err(err) => {
                service.record_error(format!("{err:#}"));
                eprintln!("emersia-daemon: capture failed: {err:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emersia_protocol::{Command, ResponseErr, ResponseOk, StartArgs};

    /// A service backed by a throwaway trust store, with fake outputs.
    fn service_with_outputs() -> Service {
        let dir = tempfile::tempdir().expect("temp trust store");
        let db = PairingDb::load(&dir.path().join("pairing.json")).expect("fresh store");
        let host = crate::crypto::HostIdentity::load_or_create(&dir.path().join("host.key"))
            .expect("host key");
        let host_hex = host.public_key_hex();
        let service = Service::new(db, &host_hex);
        std::mem::forget(dir);
        service.publish_discovery(
            vec![
                ScreenInfo {
                    kind: "output",
                    name: "DP-3".into(),
                    width: 1920,
                    height: 1080,
                    transform: "normal".into(),
                },
                ScreenInfo {
                    kind: "output",
                    name: "HDMI-A-1".into(),
                    width: 1920,
                    height: 1080,
                    transform: "normal".into(),
                },
            ],
            Some("wlr-screencopy"),
        );
        service
    }

    fn req(id: &str, command: Command) -> Request {
        Request {
            v: PROTOCOL_VERSION,
            id: id.into(),
            command,
        }
    }

    fn ok_payload(res: Response) -> serde_json::Value {
        match res {
            Response::Ok(o) => o.ok,
            Response::Err(e) => panic!("expected ok, got {:?}", e.error),
        }
    }

    fn err_code(res: Response) -> ErrorCode {
        match res {
            Response::Err(e) => e.error.code,
            Response::Ok(o) => panic!("expected err, got {o:?}"),
        }
    }

    #[test]
    fn status_reports_discovery() {
        let s = service_with_outputs();
        let p = ok_payload(s.handle(req("1", Command::Status)));
        assert_eq!(p["backend"], "wlr-screencopy");
        assert_eq!(p["streaming"], false);
        assert_eq!(p["frames"], 0);
    }

    #[test]
    fn screens_lists_outputs() {
        let s = service_with_outputs();
        let p = ok_payload(s.handle(req("1", Command::Screens)));
        let arr = p["screens"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "DP-3");
    }

    #[test]
    fn start_then_stop_transitions_streaming() {
        let s = service_with_outputs();
        let p = ok_payload(s.handle(req("1", Command::Start(StartArgs::default()))));
        assert_eq!(p["streaming"], true);
        assert_eq!(p["output"], "DP-3", "defaults to first output");
        assert!(s.is_streaming());

        // Starting twice is busy.
        assert_eq!(
            err_code(s.handle(req("2", Command::Start(StartArgs::default())))),
            ErrorCode::Busy
        );

        ok_payload(s.handle(req("3", Command::Stop)));
        assert!(!s.is_streaming());
        // Stopping when idle is also reported, not silently ignored.
        assert_eq!(err_code(s.handle(req("4", Command::Stop))), ErrorCode::Busy);
    }

    #[test]
    fn start_rejects_unknown_output_and_bad_fps() {
        let s = service_with_outputs();
        let res = s.handle(req(
            "1",
            Command::Start(StartArgs {
                output: Some("NOPE".into()),
                fps: None,
            }),
        ));
        assert_eq!(err_code(res), ErrorCode::NotFound);

        let res = s.handle(req(
            "2",
            Command::Start(StartArgs {
                output: None,
                fps: Some(0),
            }),
        ));
        assert_eq!(err_code(res), ErrorCode::InvalidArgs);
        assert!(!s.is_streaming());
    }

    #[test]
    fn start_selects_named_output() {
        let s = service_with_outputs();
        let p = ok_payload(s.handle(req(
            "1",
            Command::Start(StartArgs {
                output: Some("HDMI-A-1".into()),
                fps: Some(60),
            }),
        )));
        assert_eq!(p["output"], "HDMI-A-1");
        assert_eq!(p["fps"], 60);
        assert_eq!(s.fps_target(), 60);
    }

    #[test]
    fn unknown_command_is_rejected_cleanly() {
        let s = service_with_outputs();
        assert_eq!(
            err_code(s.handle(req("1", Command::Unknown))),
            ErrorCode::UnknownCmd
        );
    }

    #[test]
    fn pair_new_mints_a_code() {
        let s = service_with_outputs();
        let p = ok_payload(s.handle(req(
            "1",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::New,
                code: None,
                name: None,
                public_key: None,
            }),
        )));
        let code = p["code"].as_str().expect("code in response");
        assert_eq!(code.len(), 7, "formatted as XXX-XXX");
        assert!(p["expires_in_s"].as_u64().unwrap() > 0);
    }

    #[test]
    fn pair_accept_registers_then_devices_lists() {
        let s = service_with_outputs();
        let minted = ok_payload(s.handle(req(
            "1",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::New,
                code: None,
                name: None,
                public_key: None,
            }),
        )));
        let code = minted["code"].as_str().unwrap().to_string();
        let identity = crate::crypto::DeviceIdentity::generate().unwrap();
        let paired = ok_payload(s.handle(req(
            "2",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::Accept,
                code: Some(code),
                name: Some("Quest 3".into()),
                public_key: Some(identity.public_key_hex()),
            }),
        )));
        assert_eq!(paired["paired"], true);
        assert_eq!(paired["name"], "Quest 3");

        let listed = ok_payload(s.handle(req("3", Command::Devices)));
        assert_eq!(listed["active"], 1);
        let devices = listed["devices"].as_array().unwrap();
        assert_eq!(devices[0]["name"], "Quest 3");
        assert_eq!(devices[0]["revoked"], false);
    }

    /// A device that pairs and connects immediately must be accepted.
    ///
    /// The engine used to copy the device table once per loop iteration and
    /// answer handshakes from that copy, so a device paired within the
    /// iteration's window was refused as unpaired. Since a client sends its
    /// `init` exactly once, the connection then failed until the user retried.
    /// The authorizer now reads the trust store at the moment it decides.
    #[test]
    fn a_device_paired_after_the_authorizer_existed_is_accepted() {
        let s = service_with_outputs();
        // Take the authorizer's view *before* the device exists, the way a
        // long-lived closure would have.
        let existing = s.lock_pairing().devices();
        assert!(existing.is_empty(), "nothing paired yet");

        let minted = ok_payload(s.handle(req(
            "1",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::New,
                code: None,
                name: None,
                public_key: None,
            }),
        )));
        let code = minted["code"].as_str().unwrap().to_string();
        let identity = crate::crypto::DeviceIdentity::generate().unwrap();
        let paired = ok_payload(s.handle(req(
            "2",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::Accept,
                code: Some(code),
                name: Some("Quest 3".into()),
                public_key: Some(identity.public_key_hex()),
            }),
        )));
        let id = paired["device"].as_str().unwrap();

        // The decision reads current state, not the pre-pairing copy.
        let db = s.lock_pairing();
        let authorized = db.devices().iter().any(|d| {
            d.id == id
                && d.public_key
                    .eq_ignore_ascii_case(&identity.public_key_hex())
                && !d.revoked
        });
        assert!(
            authorized,
            "a device paired after the authorizer was created must be accepted"
        );
    }

    /// A revoked device must be refused even if it presents valid credentials.
    #[test]
    fn a_revoked_device_is_refused_even_with_the_right_key() {
        let s = service_with_outputs();
        let minted = ok_payload(s.handle(req(
            "1",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::New,
                code: None,
                name: None,
                public_key: None,
            }),
        )));
        let code = minted["code"].as_str().unwrap().to_string();
        let identity = crate::crypto::DeviceIdentity::generate().unwrap();
        let paired = ok_payload(s.handle(req(
            "2",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::Accept,
                code: Some(code),
                name: Some("Quest 3".into()),
                public_key: Some(identity.public_key_hex()),
            }),
        )));
        let id = paired["device"].as_str().unwrap().to_string();
        ok_payload(s.handle(req(
            "3",
            Command::Revoke(emersia_protocol::RevokeArgs { device: id.clone() }),
        )));

        let db = s.lock_pairing();
        let authorized = db.devices().iter().any(|d| {
            d.id == id
                && d.public_key
                    .eq_ignore_ascii_case(&identity.public_key_hex())
                && !d.revoked
        });
        assert!(!authorized, "a revoked device must never be authorized");
    }

    #[test]
    fn pair_accept_with_a_wrong_code_is_not_paired() {
        let s = service_with_outputs();
        let _ = s.handle(req(
            "1",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::New,
                code: None,
                name: None,
                public_key: None,
            }),
        ));
        let identity = crate::crypto::DeviceIdentity::generate().unwrap();
        let res = s.handle(req(
            "2",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::Accept,
                code: Some("000000".into()),
                name: Some("Quest".into()),
                public_key: Some(identity.public_key_hex()),
            }),
        ));
        assert_eq!(err_code(res), ErrorCode::NotPaired);
    }

    #[test]
    fn pair_accept_requires_all_arguments() {
        let s = service_with_outputs();
        let res = s.handle(req(
            "1",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::Accept,
                code: Some("123456".into()),
                name: None,
                public_key: None,
            }),
        ));
        assert_eq!(err_code(res), ErrorCode::InvalidArgs);
    }

    #[test]
    fn revoke_removes_a_paired_device() {
        let s = service_with_outputs();
        let minted = ok_payload(s.handle(req(
            "1",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::New,
                code: None,
                name: None,
                public_key: None,
            }),
        )));
        let code = minted["code"].as_str().unwrap().to_string();
        let identity = crate::crypto::DeviceIdentity::generate().unwrap();
        let paired = ok_payload(s.handle(req(
            "2",
            Command::Pair(emersia_protocol::PairArgs {
                action: emersia_protocol::PairAction::Accept,
                code: Some(code),
                name: Some("Quest".into()),
                public_key: Some(identity.public_key_hex()),
            }),
        )));
        let device_id = paired["device"].as_str().unwrap().to_string();

        let revoked = ok_payload(s.handle(req(
            "3",
            Command::Revoke(emersia_protocol::RevokeArgs { device: device_id }),
        )));
        assert_eq!(revoked["revoked"], true);
        let listed = ok_payload(s.handle(req("4", Command::Devices)));
        assert_eq!(listed["active"], 0, "revoked devices are not active");
    }

    #[test]
    fn revoking_an_unknown_device_is_not_found() {
        let s = service_with_outputs();
        let res = s.handle(req(
            "1",
            Command::Revoke(emersia_protocol::RevokeArgs {
                device: "nosuch".into(),
            }),
        ));
        assert_eq!(err_code(res), ErrorCode::NotFound);
    }

    #[test]
    fn record_frame_updates_running_stats() {
        let s = service_with_outputs();
        s.record_frame(Duration::from_millis(5), Duration::from_millis(33));
        s.record_frame(Duration::from_millis(20), Duration::from_millis(33));
        let p = ok_payload(s.handle(req("1", Command::Status)));
        assert_eq!(p["frames"], 2);
        assert_eq!(p["latency_ms"], 20.0);
        assert_eq!(p["avg_latency_ms"], 12.5);
    }

    #[test]
    fn record_error_stops_streaming() {
        let s = service_with_outputs();
        ok_payload(s.handle(req("1", Command::Start(StartArgs::default()))));
        s.record_error("backend died".into());
        assert!(!s.is_streaming());
        let p = ok_payload(s.handle(req("2", Command::Status)));
        assert_eq!(p["last_error"], "backend died");
    }

    #[test]
    fn error_response_shape_is_wire_ready() {
        let s = service_with_outputs();
        let res = s.handle(req("abc", Command::Unknown));
        match res {
            Response::Err(ResponseErr { v, id, error }) => {
                assert_eq!(v, PROTOCOL_VERSION);
                assert_eq!(id, "abc");
                assert_eq!(error.code, ErrorCode::UnknownCmd);
            }
            _ => panic!("expected error response"),
        }
    }

    #[test]
    fn ok_response_shape_is_wire_ready() {
        let s = service_with_outputs();
        let res = s.handle(req("xyz", Command::Status));
        match res {
            Response::Ok(ResponseOk { v, id, .. }) => {
                assert_eq!(v, PROTOCOL_VERSION);
                assert_eq!(id, "xyz");
            }
            _ => panic!("expected ok response"),
        }
    }
}

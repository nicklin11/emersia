//! Video encoding: runtime encoder selection and an encode pipeline.
//!
//! # Why the encoder runs as a separate process
//!
//! The `ffmpeg` CLI is driven as a child process rather than linking
//! `libavcodec`. That is a deliberate trade, recorded in ADR 0003:
//!
//! - **For:** one code path reaches VAAPI, NVENC, AMF and QSV. Each vendor's
//!   encoder is selected by name, with no per-vendor Rust binding to write,
//!   maintain or cross-compile. Encoder support then tracks the ffmpeg build
//!   the user already has, which is the same build their other tools use.
//! - **Against:** a process boundary per stream, and no in-process API control.
//!   Frame delivery is via pipes; a bounded writer queue keeps a stalled
//!   encoder from blocking the capture loop, while dedicated drain threads keep
//!   the child's other pipes from filling.
//!
//! Hardware encode is the default. Software (`libx264`) exists as `--encoder
//! sw` for debugging and as a last-resort fallback, and is labelled as such
//! wherever it is selected: a software stream on a machine that can encode in
//! hardware is a bug the user should see, not a silent downgrade.
//!
//! # Capability is probed, not assumed
//!
//! `vainfo` lists VA profiles and entry points, but that is not the same as
//! "this encoder will accept these parameters". On the dev machine's Navi 22,
//! `VAProfileH264High` reports `VAEntrypointEncSlice` and *not*
//! `VAEntrypointEncPicture` — so ffmpeg's `-low_power 1` (which asks for
//! EncPicture) fails with "No usable encoding entrypoint", while the default
//! slice-mode path works. Reading `vainfo` would have given the wrong answer.
//! Selection therefore *runs* each candidate encoder on one small frame and
//! keeps the first that produces output.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::time::{Duration, Instant};

use crate::codec::{AccessUnit, AnnexBParser, Codec, CodecError};

/// Keep a small diagnostic tail rather than allowing ffmpeg's stderr pipe to
/// fill while nobody reads it.
const STDERR_TAIL_LINES: usize = 32;
const STDERR_TAIL_BYTES: usize = 16 * 1024;
const STDERR_MAX_LINE_BYTES: usize = 4 * 1024;

/// A frame may be waiting for the writer, and the writer may be blocked in the
/// OS pipe. Keep only one additional frame so `encode` can return immediately.
const STDIN_QUEUE_CAPACITY: usize = 1;

/// Do not let child teardown block the capture loop indefinitely. A kill has
/// already been requested; helper threads are detached if the child remains
/// unkillable long enough to hit this bound.
const TEARDOWN_TIMEOUT: Duration = Duration::from_millis(250);

/// A bounded rolling view of the encoder's most recent stderr lines.
#[derive(Debug, Default)]
struct StderrTail {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrTail {
    fn push_line(&mut self, line: &str) {
        let mut line = line.to_owned();
        if line.len() > STDERR_MAX_LINE_BYTES {
            let mut end = STDERR_MAX_LINE_BYTES;
            while end > 0 && !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push_str("...");
        }

        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.lines.len() > STDERR_TAIL_LINES || self.bytes > STDERR_TAIL_BYTES {
            let Some(old) = self.lines.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.len() + 1);
        }
    }

    fn text(&self) -> String {
        self.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// Read stderr continuously and keep only its bounded tail.
///
/// Reading a fixed-size chunk keeps both the retained data and the per-line
/// scratch buffer bounded even if ffmpeg emits a line without a newline.
fn drain_stderr(mut stderr: ChildStderr, tail: std::sync::Arc<std::sync::Mutex<StderrTail>>) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let mut truncated = false;
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &byte in &buf[..n] {
                    if byte == b'\n' {
                        push_stderr_line(&tail, &line, truncated);
                        line.clear();
                        truncated = false;
                    } else if line.len() < STDERR_MAX_LINE_BYTES {
                        line.push(byte);
                    } else {
                        truncated = true;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if !line.is_empty() || truncated {
        push_stderr_line(&tail, &line, truncated);
    }
}

fn push_stderr_line(
    tail: &std::sync::Arc<std::sync::Mutex<StderrTail>>,
    line: &[u8],
    truncated: bool,
) {
    let mut text = String::from_utf8_lossy(line).into_owned();
    if truncated {
        text.push_str("...");
    }
    if text.ends_with('\r') {
        text.pop();
    }
    tail.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_line(&text);
}

/// The result of offering a frame to the bounded writer queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueResult {
    Queued,
    Dropped,
    Closed,
}

fn try_queue_frame(tx: &std::sync::mpsc::SyncSender<Vec<u8>>, frame: Vec<u8>) -> QueueResult {
    match tx.try_send(frame) {
        Ok(()) => QueueResult::Queued,
        Err(std::sync::mpsc::TrySendError::Full(_)) => QueueResult::Dropped,
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => QueueResult::Closed,
    }
}

/// Which encoder family to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderChoice {
    /// Pick the best available hardware encoder, fall back to software.
    Auto,
    /// Force VAAPI (Intel/AMD).
    Vaapi,
    /// Force NVENC (NVIDIA).
    Nvenc,
    /// Force AMF (AMD, Windows/Linux).
    Amf,
    /// Force QSV (Intel).
    Qsv,
    /// Force software x264. Debug fallback, per ADR 0003.
    Software,
}

impl EncoderChoice {
    /// Parse the value accepted by `--encoder`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "vaapi" => Some(Self::Vaapi),
            "nvenc" => Some(Self::Nvenc),
            "amf" => Some(Self::Amf),
            "qsv" => Some(Self::Qsv),
            "sw" | "software" | "libx264" => Some(Self::Software),
            _ => None,
        }
    }

    /// The ffmpeg encoder names this choice might use, best first.
    fn candidates(self, codec: Codec) -> Vec<Candidate> {
        let hw = |name: &str| Candidate::new(name.to_string(), codec, false);
        let sw = || Candidate::new("libx264".to_string(), Codec::H264, true);
        match (self, codec) {
            (Self::Software, _) => vec![sw()],
            (Self::Vaapi, c) => vec![hw(vaapi_encoder(c))],
            (Self::Nvenc, c) => vec![hw(nvenc_encoder(c))],
            (Self::Amf, c) => vec![hw(amf_encoder(c))],
            (Self::Qsv, c) => vec![hw(qsv_encoder(c))],
            // Probe in the order a real machine is most likely to satisfy:
            // VAAPI (Intel and AMD), then NVENC, then AMF, then QSV.
            (Self::Auto, c) => vec![
                hw(vaapi_encoder(c)),
                hw(nvenc_encoder(c)),
                hw(amf_encoder(c)),
                hw(qsv_encoder(c)),
                sw(),
            ],
        }
    }
}

/// ffmpeg encoder name for a codec on VAAPI.
fn vaapi_encoder(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264_vaapi",
        Codec::Hevc => "hevc_vaapi",
    }
}

fn nvenc_encoder(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264_nvenc",
        Codec::Hevc => "hevc_nvenc",
    }
}

fn amf_encoder(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264_amf",
        Codec::Hevc => "hevc_amf",
    }
}

fn qsv_encoder(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264_qsv",
        Codec::Hevc => "hevc_qsv",
    }
}

/// One concrete encoder to try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// ffmpeg `-c:v` value.
    pub name: String,
    pub codec: Codec,
    /// True for `libx264`.
    pub software: bool,
}

impl Candidate {
    fn new(name: String, codec: Codec, software: bool) -> Self {
        Self {
            name,
            codec,
            software,
        }
    }

    /// True if this candidate needs a VAAPI device (Mesa/Intel/AMD).
    fn needs_vaapi_device(&self) -> bool {
        self.name.ends_with("_vaapi")
    }
}

/// Encoder configuration.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub choice: EncoderChoice,
    pub codec: Codec,
    /// Target bitrate in bits per second.
    pub bitrate: u32,
    /// Keyframe interval in frames.
    pub gop: u32,
    /// Frames per second fed to the encoder.
    pub fps: u32,
    /// Width of the frames that will be fed in.
    pub width: u32,
    /// Height of the frames that will be fed in.
    pub height: u32,
}

impl Default for EncoderConfig {
    /// 1080p60 H.264 at 30 Mbit/s with a 2.5 s GOP — the starting point
    /// inherited from libworkspaceVR (ADR 0003), to be re-measured.
    fn default() -> Self {
        Self {
            choice: EncoderChoice::Auto,
            codec: Codec::H264,
            bitrate: 30_000_000,
            gop: 150,
            fps: 60,
            width: 1920,
            height: 1080,
        }
    }
}

/// Anything that can go wrong starting or running an encoder.
#[derive(Debug)]
pub enum EncoderError {
    /// No requested encoder could be made to work.
    NoEncoder {
        /// What the user asked for.
        requested: EncoderChoice,
        /// Every candidate that was tried, with why it failed.
        attempts: Vec<(String, String)>,
    },
    /// The child process died.
    ChildFailed { encoder: String, status: String },
    /// Writing or queueing a frame to the encoder failed.
    Write(std::io::Error),
    /// Bitstream parsing failed.
    Codec(CodecError),
    /// The configuration cannot produce a stream (e.g. odd dimensions).
    BadConfig(String),
    /// A specific candidate was rejected, with the reason.
    Rejected(String),
}

impl std::fmt::Display for EncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEncoder {
                requested,
                attempts,
            } => {
                write!(f, "no usable encoder for {requested:?}: ")?;
                if attempts.is_empty() {
                    write!(f, "nothing was tried")
                } else {
                    let mut first = true;
                    for (name, why) in attempts {
                        if !first {
                            write!(f, "; ")?;
                        }
                        first = false;
                        write!(f, "{name}: {why}")?;
                    }
                    Ok(())
                }
            }
            Self::ChildFailed { encoder, status } => {
                write!(f, "encoder {encoder} failed: {status}")
            }
            Self::Write(e) => write!(f, "could not write frame to encoder: {e}"),
            Self::Codec(e) => write!(f, "encoder output could not be parsed: {e}"),
            Self::BadConfig(why) => write!(f, "invalid encoder configuration: {why}"),
            Self::Rejected(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for EncoderError {}

impl EncoderError {
    /// True when the failure is just "there is no ffmpeg on this machine".
    ///
    /// Tests use this to skip rather than fail, so the suite still runs on a
    /// bare machine. It deliberately matches only the spawn failure and not
    /// any encoder error: "libx264 started but produced nothing" is a real
    /// failure and must not be skipped away.
    pub fn ffmpeg_missing(&self) -> bool {
        let text = self.to_string();
        text.contains("could not start ffmpeg")
    }
}

impl From<CodecError> for EncoderError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

/// A running encoder: frames in, access units out.
///
/// A few members here are for consumers this milestone has not built yet — the
/// blocking receive, the keyframe request and its status. They are part of the
/// encoder's contract with the Quest client, so they are kept and documented
/// rather than deleted and reinvented later.
///
/// # Threading
///
/// ffmpeg is fed over pipes, and a pipe read on the main thread deadlocks: the
/// encoder does not necessarily emit anything for each frame we write (it
/// buffers, and B-frame reordering would reorder output anyway), so a blocking
/// read on the writer's thread waits for output that only a later write can
/// cause. The three pipes are therefore driven from separate threads:
///
/// - a bounded input queue feeds a writer thread that owns stdin;
/// - a reader thread drains stdout, parses Annex-B, and hands access units back
///   over a channel;
/// - a stderr thread continuously drains diagnostics into a bounded tail.
///
/// `encode` never blocks on output or on the input pipe. It offers the frame to
/// the bounded queue and returns whatever access unit has already arrived. If
/// the writer cannot keep up, the new frame is dropped rather than stalling the
/// capture loop.
pub struct Encoder {
    child: Child,
    /// Sender for RGBA frames. `None` closes the queue during shutdown.
    stdin: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    /// Writer thread owning the child stdin pipe.
    writer: Option<std::thread::JoinHandle<()>>,
    /// Error reported by the writer after its pipe failed.
    writer_error: std::sync::Arc<std::sync::Mutex<Option<std::io::Error>>>,
    /// Shared bounded stderr diagnostics.
    stderr_tail: std::sync::Arc<std::sync::Mutex<StderrTail>>,
    /// Stderr drain thread.
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    /// Access units from the reader thread.
    rx: std::sync::mpsc::Receiver<ReaderMessage>,
    /// Units the reader has produced. `encode` compares this against
    /// `frames_out` to report readiness without consuming anything: taking a
    /// unit here would discard it, because the caller drains them separately.
    produced: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Keeps the reader thread alive; dropped when the encoder is dropped.
    reader: Option<std::thread::JoinHandle<()>>,
    /// Set when the reader thread reported the encoder had stopped.
    finished: bool,
    /// Set by `request_keyframe`; the GOP is the only mechanism available over
    /// a raw pipe, so this records intent rather than acting on it.
    keyframe_requested: bool,
    /// Latest parameter sets reported by the reader thread, so the sending
    /// side can prefix packets without sharing the parser itself.
    parameter_sets: Vec<u8>,
    candidate: Candidate,
    config: EncoderConfig,
    /// Rolling measurement of how long a frame takes to hand to the encoder.
    last_encode: Option<Duration>,
    frames_in: u64,
    frames_dropped: u64,
    frames_out: u64,
}

/// What the reader thread sends back.
enum ReaderMessage {
    /// A complete access unit, plus the parameter sets in force for it.
    Unit(Box<AccessUnit>, Vec<u8>),
    /// The encoder's output ended.
    End(Result<(), String>),
}

/// The outcome of pushing one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeOutcome {
    /// The frame produced no complete access unit (still in the encoder's
    /// lookahead, or a duplicate it dropped).
    Pending,
    /// At least one access unit is ready.
    Encoded,
}

impl std::fmt::Debug for Encoder {
    /// Never prints the child handle or key material, just what is running.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("candidate", &self.candidate)
            .field("frames_in", &self.frames_in)
            .field("frames_dropped", &self.frames_dropped)
            .field("frames_out", &self.frames_out)
            .field("finished", &self.finished)
            .finish()
    }
}

#[allow(dead_code)] // see the note on `Encoder`
impl Encoder {
    /// Start an encoder, probing candidates until one works.
    pub fn start(config: EncoderConfig) -> Result<Self, EncoderError> {
        Self::validate(&config)?;
        let candidates = config.choice.candidates(config.codec);
        let mut attempts = Vec::new();
        for candidate in candidates {
            match Self::spawn(&config, candidate.clone()) {
                Ok(encoder) => return Ok(encoder),
                Err(why) => attempts.push((candidate.name.clone(), why)),
            }
        }
        Err(EncoderError::NoEncoder {
            requested: config.choice,
            attempts,
        })
    }

    /// Start one specific candidate, without probing.
    ///
    /// Used by tests that need a known encoder rather than whatever probing
    /// selects, and available for a future `--encoder-name` escape hatch.
    pub fn start_exact(config: EncoderConfig, candidate: Candidate) -> Result<Self, EncoderError> {
        Self::validate(&config)?;
        Self::spawn(&config, candidate).map_err(EncoderError::Rejected)
    }

    /// Reject configurations that cannot produce a valid stream.
    fn validate(config: &EncoderConfig) -> Result<(), EncoderError> {
        if config.width == 0 || config.height == 0 {
            return Err(EncoderError::BadConfig(
                "width and height must be non-zero".into(),
            ));
        }
        if config.width % 2 != 0 || config.height % 2 != 0 {
            // 4:2:0 chroma subsampling needs even dimensions; ffmpeg would
            // silently crop, silently changing the stream.
            return Err(EncoderError::BadConfig(format!(
                "{}x{} is not even in both dimensions; 4:2:0 requires it",
                config.width, config.height
            )));
        }
        if config.fps == 0 {
            return Err(EncoderError::BadConfig("fps must be non-zero".into()));
        }
        Ok(())
    }

    /// Launch the ffmpeg child for one candidate, plus its reader thread.
    fn spawn(config: &EncoderConfig, candidate: Candidate) -> Result<Self, String> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);

        if candidate.needs_vaapi_device() {
            // A render node must be named explicitly, or hwupload has no
            // device to upload into.
            let device = std::env::var("EMERSIA_VAAPI_DEVICE")
                .unwrap_or_else(|_| "/dev/dri/renderD128".to_string());
            if !std::path::Path::new(&device).exists() {
                return Err(format!("no VAAPI device at {device}"));
            }
            cmd.args(["-vaapi_device", &device]);
        }

        // Input: raw RGBA frames on stdin.
        cmd.args([
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-s",
            &format!("{}x{}", config.width, config.height),
            "-r",
            &config.fps.to_string(),
            "-i",
            "pipe:0",
        ]);

        if candidate.needs_vaapi_device() {
            // Upload first, then format: `format` applies to the surface.
            cmd.args(["-vf", "format=nv12,hwupload"]);
        } else {
            // Software and the other vendors take system memory; convert here
            // so the encoder never sees RGBA.
            cmd.args(["-pix_fmt", "yuv420p"]);
        }

        cmd.args(["-c:v", &candidate.name]);
        cmd.args([
            "-profile:v",
            match candidate.codec {
                Codec::H264 => "high",
                Codec::Hevc => "main",
            },
        ]);
        cmd.args(["-b:v", &format!("{}k", config.bitrate / 1000)]);
        // Keyframes on a *time* interval, not a frame count.
        //
        // `-g` counts frames, so a GOP of 120 frames is 2 s at the target rate
        // but 48 s when the compositor only delivers 2.5 fps — which is exactly
        // what wlr-screencopy does on an idle screen. A device that joined
        // then waited most of a minute for its first decodable frame. Forcing
        // on elapsed stream time keeps the interval meaning the same thing
        // whatever rate the frames actually arrive at.
        let seconds = (config.gop as f64 / config.fps as f64).max(0.5);
        cmd.args([
            "-force_key_frames",
            &format!("expr:gte(t,n_forced*{seconds:.2})"),
        ]);
        // No B-frames. They buy compression efficiency at the cost of a
        // reorder buffer, and on the VAAPI path that buffer swallowed every
        // frame we fed: the encoder emitted nothing until its input closed.
        // For a low-latency stream that is exactly backwards, so the codec
        // ladder starts without them.
        cmd.args(["-bf", "0"]);
        if candidate.software {
            cmd.args(["-preset", "veryfast"]);
        }

        // Annex-B out, so the bitstream can be parsed into NAL units. The
        // muxer name follows the codec.
        cmd.args(["-f", candidate.codec.name(), "pipe:1"]);
        cmd.stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start ffmpeg: {e}"))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Stderr is a finite OS pipe, not an optional diagnostic. Drain it
        // continuously so ffmpeg cannot wedge itself by waiting for a reader.
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(StderrTail::default()));
        let stderr_tail_by_reader = std::sync::Arc::clone(&stderr_tail);
        let stderr_reader = std::thread::spawn(move || {
            drain_stderr(stderr, stderr_tail_by_reader);
        });

        // A synchronous bounded queue keeps the engine thread out of the child
        // stdin pipe. One queued frame is enough to smooth a scheduling hiccup
        // without retaining a large backlog of RGBA buffers.
        let (stdin_tx, stdin_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(STDIN_QUEUE_CAPACITY);
        let writer_error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let writer_error_by_thread = std::sync::Arc::clone(&writer_error);
        let writer = std::thread::spawn(move || {
            let mut stdin = stdin;
            while let Ok(frame) = stdin_rx.recv() {
                if let Err(e) = stdin.write_all(&frame) {
                    let mut slot = writer_error_by_thread
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if slot.is_none() {
                        *slot = Some(e);
                    }
                    break;
                }
            }
        });

        // The reader thread owns stdout for the life of the encoder.
        let codec = candidate.codec;
        let (tx, rx) = std::sync::mpsc::channel();
        let produced = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let produced_by_reader = std::sync::Arc::clone(&produced);
        let reader = std::thread::spawn(move || {
            let mut parser = AnnexBParser::new(codec);
            let mut stdout = stdout;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => {
                        // EOF: flush whatever the parser is holding.
                        let tail = parser.finish();
                        let _ = match tail {
                            Ok(units) => {
                                let sets = parser.parameter_sets().to_vec();
                                for au in units {
                                    if tx
                                        .send(ReaderMessage::Unit(Box::new(au), sets.clone()))
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                tx.send(ReaderMessage::End(Ok(())))
                            }
                            Err(e) => tx.send(ReaderMessage::End(Err(e.to_string()))),
                        };
                        return;
                    }
                    Ok(n) => match parser.push(&buf[..n]) {
                        Ok(units) => {
                            let sets = parser.parameter_sets().to_vec();
                            for au in units {
                                produced_by_reader
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if tx
                                    .send(ReaderMessage::Unit(Box::new(au), sets.clone()))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(ReaderMessage::End(Err(e.to_string())));
                            return;
                        }
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let _ = tx.send(ReaderMessage::End(Err(e.to_string())));
                        return;
                    }
                }
            }
        });

        let mut encoder = Self {
            child,
            stdin: Some(stdin_tx),
            writer: Some(writer),
            writer_error,
            stderr_tail,
            stderr_reader: Some(stderr_reader),
            rx,
            produced,
            reader: Some(reader),
            finished: false,
            keyframe_requested: false,
            parameter_sets: Vec::new(),
            candidate,
            config: config.clone(),
            last_encode: None,
            frames_in: 0,
            frames_dropped: 0,
            frames_out: 0,
        };

        // The encoder may fail at startup (bad profile, no device) without
        // having produced anything. Give it a moment to exit so the reason is
        // reported now, while probing, rather than on the first frame.
        std::thread::sleep(Duration::from_millis(30));
        match encoder.child.try_wait() {
            Ok(Some(status)) => {
                // The process is gone, so its stderr pipe is closed. Joining the
                // drain thread makes the complete tail available to the probe.
                if let Some(stderr_reader) = encoder.stderr_reader.take() {
                    let _ = stderr_reader.join();
                }
                let why = format!("exited immediately with {status}");
                Err(encoder.with_stderr(why))
            }
            Ok(None) => Ok(encoder),
            Err(e) => {
                let why = format!("could not query the encoder process: {e}");
                Err(encoder.with_stderr(why))
            }
        }
    }

    fn writer_has_failed(&self) -> bool {
        self.writer_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    fn take_writer_error_opt(&self) -> Option<std::io::Error> {
        self.writer_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn take_writer_error(&self) -> std::io::Error {
        self.take_writer_error_opt().unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "encoder stdin writer stopped",
            )
        })
    }

    fn writer_failure(&mut self) -> EncoderError {
        let error = self.take_writer_error();
        let exit_status = self.child.try_wait().ok().flatten();
        if exit_status.is_some() {
            self.join_stderr_bounded();
        }
        let status = exit_status
            .map(|status| format!("encoder exited with {status}"))
            .unwrap_or_else(|| "encoder child did not exit".to_string());
        EncoderError::ChildFailed {
            encoder: self.candidate.name.clone(),
            status: self.with_stderr(format!("stdin writer failed: {error}; {status}")),
        }
    }

    fn with_stderr(&self, status: String) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .text();
        if tail.is_empty() {
            status
        } else {
            format!("{status}; ffmpeg stderr:\n{tail}")
        }
    }

    fn join_stderr_bounded(&mut self) {
        let Some(handle) = self.stderr_reader.take() else {
            return;
        };
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }

    /// Convert an unexpected end of stdout into a terminal encoder error.
    fn output_ended(&mut self) -> Result<Option<AccessUnit>, EncoderError> {
        self.finished = true;
        self.stdin.take();
        let status = self
            .child
            .try_wait()
            .ok()
            .flatten()
            .map(|status| format!("encoder exited with {status}"))
            .unwrap_or_else(|| "encoder output ended before finish".to_string());
        Err(EncoderError::ChildFailed {
            encoder: self.candidate.name.clone(),
            status: self.with_stderr(status),
        })
    }

    /// Close all three pipes and bound teardown so a stuck child cannot freeze
    /// the capture loop. A completed helper is joined; an unfinished helper is
    /// detached after SIGKILL has been requested.
    fn stop_child_and_join(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        for handle in [
            self.reader.take(),
            self.writer.take(),
            self.stderr_reader.take(),
        ]
        .into_iter()
        .flatten()
        {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }

    /// The encoder actually selected.
    pub fn candidate(&self) -> &Candidate {
        &self.candidate
    }

    /// True if this is a software encode (the debug fallback).
    pub fn is_software(&self) -> bool {
        self.candidate.software
    }

    /// Configuration in force.
    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Frames accepted by the bounded writer queue. A frame can still be
    /// waiting for the writer when this counter is read.
    pub fn frames_in(&self) -> u64 {
        self.frames_in
    }

    /// Frames dropped because the bounded input queue was full.
    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped
    }

    /// Access units consumed from the reader by the caller.
    pub fn frames_out(&self) -> u64 {
        self.frames_out
    }

    /// How long the most recent frame took to enqueue.
    pub fn last_encode_time(&self) -> Option<Duration> {
        self.last_encode
    }

    /// Feed one RGBA frame.
    ///
    /// Returns `Encoded` when an access unit became available. The frame is
    /// offered to a bounded queue with [`std::sync::mpsc::SyncSender::try_send`],
    /// so a slow encoder drops this frame instead of blocking the capture loop.
    pub fn encode(&mut self, rgba: &[u8]) -> Result<EncodeOutcome, EncoderError> {
        let expected = (self.config.width as usize)
            .saturating_mul(self.config.height as usize)
            .saturating_mul(4);
        if rgba.len() != expected {
            return Err(EncoderError::BadConfig(format!(
                "frame is {} bytes, expected {expected} for {}x{} RGBA",
                rgba.len(),
                self.config.width,
                self.config.height
            )));
        }

        if self.finished {
            return Err(EncoderError::Rejected("encoder is finished".into()));
        }
        if self.writer_has_failed() {
            self.stdin.take();
            return Err(self.writer_failure());
        }

        let started = Instant::now();
        let queued = {
            let stdin = self
                .stdin
                .as_ref()
                .ok_or_else(|| EncoderError::Rejected("encoder is finished".into()))?;
            match try_queue_frame(stdin, rgba.to_vec()) {
                QueueResult::Queued => true,
                QueueResult::Dropped => {
                    self.frames_dropped += 1;
                    false
                }
                QueueResult::Closed => {
                    self.stdin.take();
                    return Err(self.writer_failure());
                }
            }
        };
        if queued {
            self.frames_in += 1;
        }
        self.last_encode = Some(started.elapsed());

        // Report whether the reader has anything ready, without taking it.
        // Draining here would discard the access units: the caller collects
        // them with `next_access_unit`, and a unit consumed here would never
        // reach the transport.
        let produced = self.produced.load(std::sync::atomic::Ordering::Relaxed);
        Ok(if produced > self.frames_out {
            EncodeOutcome::Encoded
        } else {
            EncodeOutcome::Pending
        })
    }

    /// Take the next complete access unit if one is already available.
    ///
    /// This compatibility wrapper keeps the original non-fallible API. The
    /// streaming loop uses [`Self::next_access_unit_result`] so a reader or
    /// child failure is reported instead of silently becoming an empty poll.
    pub fn next_access_unit(&mut self) -> Option<AccessUnit> {
        self.next_access_unit_result().ok().flatten()
    }

    /// Poll for an access unit and surface reader/child failures.
    pub fn next_access_unit_result(&mut self) -> Result<Option<AccessUnit>, EncoderError> {
        match self.rx.try_recv() {
            Ok(ReaderMessage::Unit(au, sets)) => {
                self.frames_out += 1;
                self.parameter_sets = sets;
                if au.keyframe {
                    self.keyframe_requested = false;
                }
                Ok(Some(*au))
            }
            Ok(ReaderMessage::End(Err(why))) => {
                self.finished = true;
                self.stdin.take();
                Err(EncoderError::ChildFailed {
                    encoder: self.candidate.name.clone(),
                    status: self.with_stderr(why),
                })
            }
            Ok(ReaderMessage::End(Ok(()))) => self.output_ended(),
            // Empty channel: nothing ready yet.
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.finished = true;
                self.stdin.take();
                Err(EncoderError::ChildFailed {
                    encoder: self.candidate.name.clone(),
                    status: self.with_stderr("encoder output reader stopped unexpectedly".into()),
                })
            }
        }
    }

    /// Take the next access unit, waiting up to `timeout`.
    ///
    /// Used by the streaming path, where a short wait is better than shipping
    /// nothing for this tick.
    pub fn next_access_unit_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<AccessUnit>, EncoderError> {
        match self.rx.recv_timeout(timeout) {
            Ok(ReaderMessage::Unit(au, sets)) => {
                self.frames_out += 1;
                self.parameter_sets = sets;
                if au.keyframe {
                    self.keyframe_requested = false;
                }
                Ok(Some(*au))
            }
            Ok(ReaderMessage::End(Ok(()))) => self.output_ended(),
            Ok(ReaderMessage::End(Err(why))) => {
                self.finished = true;
                self.stdin.take();
                let status = self.with_stderr(why);
                Err(EncoderError::ChildFailed {
                    encoder: self.candidate.name.clone(),
                    status,
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            // The reader thread ended without a message: treat as stream end.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.finished = true;
                self.stdin.take();
                Err(EncoderError::ChildFailed {
                    encoder: self.candidate.name.clone(),
                    status: self.with_stderr("encoder output reader stopped unexpectedly".into()),
                })
            }
        }
    }

    /// The wire payload for one access unit: the parameter sets currently in
    /// force, then the picture (ADR 0003 — config repeated in every packet).
    pub fn payload_for(&self, au: &AccessUnit) -> Vec<u8> {
        // A keyframe access unit already starts with the current parameter
        // sets. Do not duplicate them on the wire; delta pictures still need
        // the carried-forward prefix.
        if au.keyframe && au.annexb.starts_with(&self.parameter_sets) {
            return au.annexb.clone();
        }
        let mut out = Vec::with_capacity(self.parameter_sets.len() + au.annexb.len());
        out.extend_from_slice(&self.parameter_sets);
        out.extend_from_slice(&au.annexb);
        out
    }

    /// The parameter sets the encoder has emitted so far.
    pub fn parameter_sets(&self) -> &[u8] {
        &self.parameter_sets
    }

    /// Ask for a keyframe on the next frame fed.
    ///
    /// ffmpeg exposes no control channel for this over a raw pipe, so the
    /// request is recorded and served by the caller's next periodic keyframe.
    /// Kept as an explicit no-op rather than a silent lie: callers can see
    /// that the request did not take effect and fall back to waiting for the
    /// GOP.
    pub fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    /// True if a keyframe was requested but not yet delivered.
    ///
    /// A raw ffmpeg pipe has no control channel, so this stays true until the
    /// next periodic keyframe arrives. Exposed so a caller can tell the
    /// difference between "asked and waiting" and "already happened".
    pub fn keyframe_pending(&self) -> bool {
        self.keyframe_requested
    }

    /// Finish the stream: close stdin, drain the rest, and return the tail.
    pub fn finish(mut self) -> Result<Vec<AccessUnit>, EncoderError> {
        // Closing stdin tells ffmpeg the input ended; it flushes and exits.
        self.stdin.take();
        let mut out = Vec::new();
        let mut reader_error = None;
        let mut stream_ended = false;
        loop {
            match self.rx.recv_timeout(Duration::from_secs(2)) {
                Ok(ReaderMessage::Unit(au, sets)) => {
                    self.parameter_sets = sets;
                    out.push(*au)
                }
                Ok(ReaderMessage::End(Ok(()))) => {
                    stream_ended = true;
                    break;
                }
                Ok(ReaderMessage::End(Err(why))) => {
                    reader_error = Some(why);
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    reader_error = Some("timed out waiting for encoder output".to_string());
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    reader_error = Some("encoder output reader stopped unexpectedly".to_string());
                    break;
                }
            }
        }

        // Capture the child's status before the intentional kill. A clean
        // stdout EOF can race a writer that is still unwinding from EPIPE;
        // an unsuccessful child status remains a real failure, while an EPIPE
        // caused only by our shutdown is ignored.
        let status_before_stop = self.child.try_wait().ok().flatten();
        let writer_error_before_stop = self.writer_has_failed().then(|| self.take_writer_error());
        self.stop_child_and_join();
        let status_after_stop = self.child.try_wait().ok().flatten();
        let child_status = status_before_stop.or(status_after_stop);
        let writer_error_after_stop = self.take_writer_error_opt();

        if let Some(why) = reader_error {
            let mut status = self.with_stderr(why);
            if let Some(e) = writer_error_before_stop.or(writer_error_after_stop) {
                status.push_str(&format!("; stdin writer: {e}"));
            }
            return Err(EncoderError::ChildFailed {
                encoder: self.candidate.name.clone(),
                status,
            });
        }
        if let Some(status) = child_status {
            if !status.success() {
                let mut why = self.with_stderr(format!("encoder exited with {status}"));
                if let Some(e) = writer_error_before_stop.or(writer_error_after_stop) {
                    why.push_str(&format!("; stdin writer: {e}"));
                }
                return Err(EncoderError::ChildFailed {
                    encoder: self.candidate.name.clone(),
                    status: why,
                });
            }
        }
        if !stream_ended {
            if let Some(e) = writer_error_before_stop.or(writer_error_after_stop) {
                return Err(EncoderError::Write(e));
            }
        }
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Make sure ffmpeg is not left running if the encoder is dropped
        // without an explicit finish. Killing and reaping the child first
        // closes all three pipes, so every helper thread can be joined.
        self.stop_child_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_keeps_bounded_last_lines() {
        let mut tail = StderrTail::default();
        for i in 0..(STDERR_TAIL_LINES + 7) {
            tail.push_line(&format!("encoder line {i}"));
        }

        let text = tail.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), STDERR_TAIL_LINES);
        assert_eq!(lines[0], "encoder line 7");
        assert_eq!(
            lines[lines.len() - 1],
            format!("encoder line {}", STDERR_TAIL_LINES + 6)
        );
        assert!(!text.contains("encoder line 0\n"));
        assert!(tail.bytes <= STDERR_TAIL_BYTES);
    }

    #[test]
    fn bounded_stdin_queue_drops_instead_of_waiting() {
        let (tx, rx) = std::sync::mpsc::sync_channel(STDIN_QUEUE_CAPACITY);
        assert_eq!(try_queue_frame(&tx, vec![1]), QueueResult::Queued);
        // No receiver is running, so this queue is full. `try_send` must report
        // a drop immediately rather than waiting for the writer to make room.
        assert_eq!(try_queue_frame(&tx, vec![2]), QueueResult::Dropped);
        assert_eq!(rx.try_recv(), Ok(vec![1]));

        drop(rx);
        assert_eq!(try_queue_frame(&tx, vec![3]), QueueResult::Closed);
    }

    #[test]
    fn encoder_choice_parses() {
        assert_eq!(EncoderChoice::parse("auto"), Some(EncoderChoice::Auto));
        assert_eq!(EncoderChoice::parse("vaapi"), Some(EncoderChoice::Vaapi));
        assert_eq!(EncoderChoice::parse("nvenc"), Some(EncoderChoice::Nvenc));
        assert_eq!(EncoderChoice::parse("amf"), Some(EncoderChoice::Amf));
        assert_eq!(EncoderChoice::parse("qsv"), Some(EncoderChoice::Qsv));
        assert_eq!(EncoderChoice::parse("sw"), Some(EncoderChoice::Software));
        assert_eq!(
            EncoderChoice::parse("software"),
            Some(EncoderChoice::Software)
        );
        assert_eq!(EncoderChoice::parse("magic"), None);
    }

    #[test]
    fn auto_probes_hardware_before_software() {
        let candidates = EncoderChoice::Auto.candidates(Codec::H264);
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names[0], "h264_vaapi", "VAAPI is probed first");
        assert_eq!(names[1], "h264_nvenc");
        assert_eq!(names[2], "h264_amf");
        assert_eq!(names[3], "h264_qsv");
        assert_eq!(names[4], "libx264", "software is the last resort");
        assert!(!candidates[..4].iter().any(|c| c.software));
    }

    #[test]
    fn forced_choice_never_falls_back_to_software() {
        for choice in [
            EncoderChoice::Vaapi,
            EncoderChoice::Nvenc,
            EncoderChoice::Amf,
            EncoderChoice::Qsv,
        ] {
            let candidates = choice.candidates(Codec::H264);
            assert!(
                !candidates.iter().any(|c| c.software),
                "{choice:?} must not silently fall back to software"
            );
            assert_eq!(candidates.len(), 1, "{choice:?} tries exactly one encoder");
        }
    }

    #[test]
    fn hevc_candidates_use_hevc_encoders() {
        let names: Vec<String> = EncoderChoice::Auto
            .candidates(Codec::Hevc)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(names.iter().any(|n| n == "hevc_vaapi"));
        assert!(names.iter().any(|n| n == "hevc_nvenc"));
        assert_eq!(names.last().unwrap(), "libx264", "x264 is H.264 only");
        // Software fallback for HEVC still has to be H.264, so the caller must
        // see the mismatch rather than get a broken stream.
    }

    #[test]
    fn odd_dimensions_are_rejected_before_spawning() {
        let mut cfg = EncoderConfig {
            choice: EncoderChoice::Software,
            width: 641,
            height: 480,
            ..Default::default()
        };
        cfg.width = 641;
        let err = Encoder::start(cfg).unwrap_err();
        assert!(
            matches!(err, EncoderError::BadConfig(_)),
            "odd width must be caught before launching a process"
        );
    }

    #[test]
    fn zero_fps_is_rejected() {
        let cfg = EncoderConfig {
            choice: EncoderChoice::Software,
            fps: 0,
            ..Default::default()
        };
        assert!(matches!(
            Encoder::start(cfg).unwrap_err(),
            EncoderError::BadConfig(_)
        ));
    }

    #[test]
    fn wrong_sized_frame_is_rejected() {
        let cfg = EncoderConfig {
            choice: EncoderChoice::Software,
            width: 64,
            height: 64,
            ..Default::default()
        };
        let mut enc = match Encoder::start(cfg) {
            Ok(e) => e,
            // No ffmpeg in this environment: the check we care about is pure.
            Err(_) => return,
        };
        let too_short = vec![0u8; 64 * 64];
        let err = enc.encode(&too_short).unwrap_err();
        assert!(matches!(err, EncoderError::BadConfig(_)), "{err}");
    }

    /// Encode a short synthetic clip with software x264, if available.
    ///
    /// Skipped rather than failed when ffmpeg is missing, so the suite still
    /// runs on a bare machine — but on any machine with ffmpeg this is the test
    /// that proves the whole pipeline works.
    #[test]
    fn software_encoder_produces_decodable_access_units() {
        let (w, h, n) = (128u32, 64u32, 12u32);
        let cfg = EncoderConfig {
            choice: EncoderChoice::Software,
            codec: Codec::H264,
            bitrate: 1_000_000,
            gop: 6,
            fps: 12,
            width: w,
            height: h,
        };
        let mut enc = match Encoder::start(cfg) {
            Ok(e) => e,
            Err(e) if e.ffmpeg_missing() => {
                eprintln!("skipping: {e}");
                return;
            }
            Err(e) => panic!("software encoder should be available: {e}"),
        };
        assert!(enc.is_software(), "libx264 is the software fallback");

        let mut produced = 0u64;
        for i in 0..n {
            // A moving gradient so the encoder has something to compress.
            let mut frame = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let p = ((x + y + i * 4) % 256) as u8;
                    let o = ((y * w + x) * 4) as usize;
                    frame[o] = p;
                    frame[o + 1] = p;
                    frame[o + 2] = p;
                    frame[o + 3] = 255;
                }
            }
            enc.encode(&frame).expect("frame should be accepted");
            while let Some(au) = enc.next_access_unit() {
                assert!(!au.annexb.is_empty(), "access unit carries bytes");
                assert!(
                    au.annexb.windows(4).any(|w| w == [0, 0, 0, 1]),
                    "Annex-B start code present"
                );
                produced += 1;
            }
        }
        let frames_in = enc.frames_in();
        let tail = enc.finish().expect("flush should succeed");
        produced += tail.len() as u64;

        assert!(frames_in > 0, "at least one frame should be accepted");
        assert_eq!(
            produced, frames_in,
            "every accepted frame produces exactly one access unit"
        );
        assert!(frames_in <= n as u64, "the bounded queue may drop frames");
    }

    /// Feeds a realistic 1080p60 workload and reports what came out.
    ///
    /// The unit tests use a tiny 128x64 clip, which some encoders handle on a
    /// different path than real geometry. This one exercises the size and rate
    /// the daemon actually runs at.
    #[test]
    #[ignore = "requires a GPU and ffmpeg"]
    fn report_1080p60_throughput() {
        let (w, h, n) = (1920u32, 1080u32, 60u32);
        let cfg = EncoderConfig {
            choice: EncoderChoice::Auto,
            codec: Codec::H264,
            bitrate: 20_000_000,
            gop: 120,
            fps: 60,
            width: w,
            height: h,
        };
        let mut enc = match Encoder::start(cfg) {
            Ok(e) => e,
            Err(e) => {
                println!("no encoder: {e}");
                return;
            }
        };
        println!("selected: {}", enc.candidate().name);
        let frame = vec![64u8; (w * h * 4) as usize];
        let t0 = std::time::Instant::now();
        let mut units = 0usize;
        let mut bytes = 0usize;
        for _ in 0..n {
            enc.encode(&frame).unwrap();
            while let Some(au) = enc.next_access_unit() {
                units += 1;
                bytes += au.annexb.len();
            }
        }
        let tail = enc.finish().unwrap();
        units += tail.len();
        bytes += tail.iter().map(|a| a.annexb.len()).sum::<usize>();
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "{n} frames in {dt:.2}s -> {units} access units, {} KiB, {:.2} fps, {:.1} Mbit/s",
            bytes / 1024,
            units as f64 / dt,
            bytes as f64 * 8.0 / dt / 1e6,
        );
        assert!(units > 0, "the encoder must produce output for 60 frames");
    }

    /// Feeds frames at a *slow* rate, as the compositor actually delivers them.
    ///
    /// The engine test above pushes 60 frames as fast as it can, which is not
    /// what happens in production: wlr-screencopy only produces a frame on the
    /// next repaint, so on an idle screen the real rate is a few fps while the
    /// encoder is told 60. This checks whether the encoder still emits under
    /// that mismatch, which is the case that actually occurs.
    #[test]
    #[ignore = "requires a GPU and ffmpeg"]
    fn report_slow_feed_rate() {
        for gap_ms in [0u64, 100, 400] {
            let (w, h, n) = (1920u32, 1080u32, 12u32);
            let cfg = EncoderConfig {
                choice: EncoderChoice::Auto,
                codec: Codec::H264,
                bitrate: 20_000_000,
                gop: 120,
                fps: 60,
                width: w,
                height: h,
            };
            let mut enc = match Encoder::start(cfg) {
                Ok(e) => e,
                Err(e) => {
                    println!("gap {gap_ms}ms: no encoder: {e}");
                    continue;
                }
            };
            let frame = vec![64u8; (w * h * 4) as usize];
            let mut units = 0usize;
            for _ in 0..n {
                enc.encode(&frame).unwrap();
                while enc.next_access_unit().is_some() {
                    units += 1;
                }
                if gap_ms > 0 {
                    std::thread::sleep(Duration::from_millis(gap_ms));
                }
            }
            let tail = enc.finish().unwrap();
            println!(
                "gap {gap_ms}ms: {n} fed -> {} units during, {} in tail",
                units,
                tail.len()
            );
        }
    }

    /// Reports which encoder this machine actually selects. Informational, but
    /// it is the check that proves runtime probing works rather than assuming.
    #[test]
    #[ignore = "requires a GPU and ffmpeg; run explicitly to inspect selection"]
    fn report_selection_on_this_machine() {
        for choice in [
            EncoderChoice::Auto,
            EncoderChoice::Vaapi,
            EncoderChoice::Software,
        ] {
            let cfg = EncoderConfig {
                choice,
                width: 1920,
                height: 1080,
                fps: 60,
                ..Default::default()
            };
            match Encoder::start(cfg) {
                Ok(e) => println!(
                    "{choice:?} -> {} ({})",
                    e.candidate().name,
                    if e.is_software() {
                        "software"
                    } else {
                        "hardware"
                    }
                ),
                Err(e) => println!("{choice:?} -> FAILED: {e}"),
            }
        }
    }

    /// Regression: `encode` must not consume the access units it reports.
    ///
    /// An earlier version drained the reader channel inside `encode` to decide
    /// whether output was ready, and threw the units away. The caller then
    /// drained again and found nothing, so a live stream produced zero packets
    /// while the counters cheerfully reported frames encoded. Readiness is now
    /// reported from a separate counter.
    #[test]
    fn encode_does_not_consume_access_units() {
        let (w, h, n) = (128u32, 64u32, 8u32);
        let cfg = EncoderConfig {
            choice: EncoderChoice::Software,
            codec: Codec::H264,
            bitrate: 1_000_000,
            gop: 2,
            fps: 8,
            width: w,
            height: h,
        };
        let mut enc = match Encoder::start(cfg) {
            Ok(e) => e,
            Err(e) if e.ffmpeg_missing() => return,
            Err(e) => panic!("{e}"),
        };
        let frame = vec![77u8; (w * h * 4) as usize];
        for _ in 0..n {
            enc.encode(&frame).unwrap();
        }
        let frames_in = enc.frames_in();
        // Everything the encoder produced must still be collectable.
        let mut collected = 0u64;
        while enc.next_access_unit().is_some() {
            collected += 1;
        }
        let collected = collected + enc.finish().unwrap().len() as u64;
        assert_eq!(
            collected, frames_in,
            "no accepted access unit may be lost between the encoder and the caller"
        );
    }

    #[test]
    fn keyframe_appears_at_gop_boundary() {
        let (w, h, n) = (128u32, 64u32, 12u32);
        let cfg = EncoderConfig {
            choice: EncoderChoice::Software,
            bitrate: 1_000_000,
            gop: 6,
            fps: 12,
            width: w,
            height: h,
            ..Default::default()
        };
        let mut enc = match Encoder::start(cfg) {
            Ok(e) => e,
            Err(e) if e.ffmpeg_missing() => return,
            Err(e) => panic!("{e}"),
        };
        let mut keyframes = 0;
        let mut units = 0;
        let mut frame = vec![0u8; (w * h * 4) as usize];
        for i in 0..n {
            // A moving gradient: a static frame lets the encoder emit skip
            // slices, which would make the keyframe count meaningless.
            for p in frame.iter_mut() {
                *p = p.wrapping_add(1).wrapping_add(i as u8);
            }
            // Keep this end-to-end test paced by accepted frames, not wall-clock
            // sleeps. The production queue intentionally drops bursts, so waiting
            // for the next accepted frame makes the GOP assertion deterministic.
            while enc.frames_in() < u64::from(i + 1) {
                enc.encode(&frame).unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
            while let Some(au) = enc.next_access_unit() {
                if au.keyframe {
                    keyframes += 1;
                }
                units += 1;
            }
        }
        let frames_in = enc.frames_in();
        // Count the tail too: for a short clip the encoder may hold everything
        // back until stdin closes, so keyframes can arrive at finish.
        for au in enc.finish().unwrap() {
            if au.keyframe {
                keyframes += 1;
            }
            units += 1;
        }
        assert_eq!(
            units, frames_in as usize,
            "one access unit per accepted frame"
        );
        assert!(
            keyframes >= 2,
            "a 6-frame GOP over accepted frames must contain at least two keyframes, got {keyframes}"
        );
    }
}

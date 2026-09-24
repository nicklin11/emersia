//! Headless test receiver for the Emersia stream (`emersia-daemon receive`).
//!
//! Performs the device side of the handshake, then decrypts records and reports
//! what arrived. This is the harness the M1 acceptance criteria call for: it
//! needs no headset, no display, and no Wayland session, so transport can be
//! exercised end to end from CI.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::crypto::{parse_public_key_hex, DeviceIdentity};
use crate::transport::handshake::DEVICE_ID_LEN;
use crate::transport::udp::ClientEndpoint;
use crate::transport::Reassembler;
use crate::transport::PAYLOAD_TYPE_RAW;

pub const USAGE: &str = "\
emersia-daemon receive — headless test receiver

Usage:
  emersia-daemon receive --server HOST:PORT --device-id HEX --host-key HEX [options]

Required:
  --server HOST:PORT   daemon streaming endpoint
  --device-id HEX      paired device id (16 hex chars, from `emersia devices`)
  --host-key HEX       pinned host public key (from `emersia status`)
  --private-key HEX    device private key from `emersia-daemon keygen`
                       (required: a fresh identity would be refused as unpaired)

Options:
  --frames N           stop after N records (default 10)
  --timeout SEC        per-read timeout in seconds (default 5)
  --save DIR           write each payload to DIR/frame-NNNN.raw
  --fps N              expected rate, used to report drift (informational)
  -h, --help           print this help
";

pub enum Fail {
    Usage(String),
    Runtime(String),
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::Runtime(e.to_string())
    }
}

struct Opts {
    server: String,
    device_id: String,
    host_key: String,
    private_key: String,
    frames: usize,
    timeout: Duration,
    save: Option<PathBuf>,
    fps: Option<u32>,
}

fn parse_device_id(hex_str: &str) -> Result<[u8; DEVICE_ID_LEN], Fail> {
    if hex_str.len() != DEVICE_ID_LEN * 2 {
        return Err(Fail::Usage(format!(
            "--device-id must be {} hex characters",
            DEVICE_ID_LEN * 2
        )));
    }
    let mut out = [0u8; DEVICE_ID_LEN];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16)
            .map_err(|_| Fail::Usage(format!("--device-id is not hex: {hex_str}")))?;
    }
    Ok(out)
}

/// Run the receiver; the caller maps errors onto exit codes.
pub fn run(args: &[String]) -> Result<(), Fail> {
    let args = args.to_vec();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    let mut server = None;
    let mut device_id = None;
    let mut host_key = None;
    let mut private_key = None;
    let mut frames = 10usize;
    let mut timeout = Duration::from_secs(5);
    let mut save = None;
    let mut fps = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<String, Fail> {
            it.next()
                .cloned()
                .ok_or_else(|| Fail::Usage(format!("missing value for {name}")))
        };
        match arg.as_str() {
            "--server" => server = Some(value("--server")?),
            "--device-id" => device_id = Some(value("--device-id")?),
            "--host-key" => host_key = Some(value("--host-key")?),
            "--private-key" => private_key = Some(value("--private-key")?),
            "--frames" => {
                frames = value("--frames")?
                    .parse()
                    .map_err(|_| Fail::Usage("--frames must be a number".into()))?
            }
            "--timeout" => {
                let secs: f64 = value("--timeout")?
                    .parse()
                    .map_err(|_| Fail::Usage("--timeout must be a number".into()))?;
                timeout = Duration::from_secs_f64(secs);
            }
            "--save" => save = Some(PathBuf::from(value("--save")?)),
            "--fps" => {
                fps = Some(
                    value("--fps")?
                        .parse()
                        .map_err(|_| Fail::Usage("--fps must be a number".into()))?,
                )
            }
            other => return Err(Fail::Usage(format!("unknown argument: {other}"))),
        }
    }

    let opts = Opts {
        server: server.ok_or_else(|| Fail::Usage("--server is required".into()))?,
        device_id: device_id.ok_or_else(|| Fail::Usage("--device-id is required".into()))?,
        host_key: host_key.ok_or_else(|| Fail::Usage("--host-key is required".into()))?,
        private_key: private_key.ok_or_else(|| {
            Fail::Usage(
                "--private-key is required (generate one with `emersia-daemon keygen`)".into(),
            )
        })?,
        frames,
        timeout,
        save,
        fps,
    };

    let device_id = parse_device_id(&opts.device_id)?;
    let host_static = parse_public_key_hex(&opts.host_key)
        .map_err(|e| Fail::Usage(format!("--host-key: {e}")))?;

    // The identity must be the one that was paired, otherwise the host's
    // default-deny check rejects the handshake — which is the point of it.
    let identity = DeviceIdentity::from_private_hex(&opts.private_key)
        .map_err(|e| Fail::Usage(format!("--private-key: {e}")))?;

    let mut client = ClientEndpoint::new(&opts.server, device_id, identity, host_static)?;
    println!("connecting to {}", opts.server);
    let handshake_started = Instant::now();
    client
        .send_init()
        .map_err(|e| Fail::Runtime(e.to_string()))?;

    // Wait for the host's response, then complete.
    let response = client.recv_raw(opts.timeout).map_err(|_| {
        Fail::Runtime(
            "no handshake response (is the daemon streaming and this device paired?)".into(),
        )
    })?;
    client
        .complete(&response)
        .map_err(|e| Fail::Runtime(format!("handshake rejected: {e}")))?;
    let handshake_ms = handshake_started.elapsed().as_secs_f64() * 1000.0;
    println!("handshake complete in {handshake_ms:.1} ms (forward-secret session established)");

    if let Some(dir) = &opts.save {
        std::fs::create_dir_all(dir)
            .map_err(|e| Fail::Runtime(format!("cannot create {}: {e}", dir.display())))?;
    }

    let started = Instant::now();
    let mut frames = 0usize;
    let mut datagrams = 0usize;
    let mut bytes = 0usize;
    let mut last_sequence = None;
    let mut max_gap = 0u32;
    let mut reassembler = Reassembler::default();

    while frames < opts.frames {
        let (header, payload) = match client.recv_record(opts.timeout) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("emersia-daemon receive: timed out after {} frames", frames);
                break;
            }
        };
        if header.payload_type != PAYLOAD_TYPE_RAW {
            // Unknown codec: skip rather than mis-report.
            continue;
        }
        if let Some(prev) = last_sequence {
            let gap = header.sequence.wrapping_sub(prev).saturating_sub(1);
            max_gap = max_gap.max(gap);
        }
        last_sequence = Some(header.sequence);
        datagrams += 1;
        bytes += payload.len();

        // A frame may span many datagrams; only count whole frames.
        let Some(whole) = reassembler.push(header, &payload) else {
            continue;
        };
        frames += 1;

        if let Some(dir) = &opts.save {
            let path = dir.join(format!("frame-{frames:04}.raw"));
            std::fs::write(&path, &whole)
                .map_err(|e| Fail::Runtime(format!("cannot write {}: {e}", path.display())))?;
        }
    }

    let elapsed = started.elapsed();
    let measured = if elapsed.as_secs_f64() > 0.0 {
        frames as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    println!("--- summary ---");
    println!("handshake: {handshake_ms:.1} ms");
    println!("frames:    {frames}");
    println!(
        "per frame: {} datagrams, {} KiB",
        datagrams / frames.max(1),
        bytes / frames.max(1) / 1024
    );
    println!("datagrams: {datagrams}");
    println!(
        "bytes:     {bytes} ({:.1} MiB)",
        bytes as f64 / (1024.0 * 1024.0)
    );
    println!("elapsed:   {:.2} s", elapsed.as_secs_f64());
    println!("measured:  {measured:.1} fps");
    println!("max gap:   {max_gap} datagram(s)");
    if let Some(want) = opts.fps {
        println!("target:    {want} fps");
    }

    if frames == 0 {
        Err(Fail::Runtime("no frames received".into()))
    } else {
        Ok(())
    }
}

impl Fail {
    /// True when the failure is a usage problem (exit 2) rather than runtime.
    pub fn is_usage(&self) -> bool {
        matches!(self, Self::Usage(_))
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Usage(m) | Self::Runtime(m) => m,
        }
    }
}

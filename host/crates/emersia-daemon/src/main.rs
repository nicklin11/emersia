//! `emersia-daemon` — Emersia host daemon.
//!
//! Two modes:
//!
//! - `serve` (default): run the control socket and capture engine. Detects
//!   outputs over `$WAYLAND_DISPLAY`, then serves JSON-lines requests on
//!   `$XDG_RUNTIME_DIR/emersia/control.sock` with `SO_PEERCRED` same-UID auth.
//! - `capture`: the M1.1 one-shot probe — grab a single frame, write it to
//!   disk as a PNG, exit. Kept for QA and for compositors that will not hold a
//!   long-lived session.
//!
//! M1.2 scope: no encoding, no transport, no input yet (see #3).

mod capture;
mod control;
mod crypto;
mod frame;
mod pairing;
mod service;
mod shm;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use capture::{BackendPref, State};
use wayland_client::Connection;

/// Crate version, e.g. `0.0.1`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the one-shot `capture` mode writes its frame unless `--out` says
/// otherwise.
const DEFAULT_OUT: &str = "/tmp/emersia-test-frame.png";

/// Exit code for a completed run.
const EXIT_OK: i32 = 0;
/// Exit code for a runtime failure (no compositor, capture error, I/O error).
const EXIT_FAILURE: i32 = 1;
/// Exit code for invalid command-line usage.
const EXIT_USAGE: i32 = 2;

/// Set by the SIGINT/SIGTERM handler so shutdown is graceful.
static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// Options for `capture` mode.
struct CaptureOpts {
    out: PathBuf,
    output: Option<String>,
    backend: BackendPref,
}

/// Options for `serve` mode.
struct ServeOpts {
    socket: Option<PathBuf>,
    backend: BackendPref,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = match parse_args(&args) {
        // `--help` / `--version` already printed and asked us to stop.
        Ok(None) => return,
        Ok(Some(mode)) => mode,
        Err(msg) => {
            eprintln!("emersia-daemon: {msg}");
            eprintln!("Try 'emersia-daemon --help' for more information.");
            std::process::exit(EXIT_USAGE);
        }
    };
    let result = match mode {
        Mode::Serve(opts) => run_serve(&opts),
        Mode::Capture(opts) => run_capture(&opts),
    };
    if let Err(err) = result {
        eprintln!("emersia-daemon: {err:#}");
        std::process::exit(EXIT_FAILURE);
    }
}

enum Mode {
    Serve(ServeOpts),
    Capture(CaptureOpts),
}

/// Parse the command line. `Ok(None)` means help/version was printed;
/// `Err` is a usage error.
fn parse_args(args: &[String]) -> Result<Option<Mode>, String> {
    let mut rest = args.iter().skip(1);
    let first = rest.next().map(String::as_str);

    match first {
        Some("--version") | Some("-V") => {
            println!("emersia-daemon {VERSION}");
            return Ok(None);
        }
        // No subcommand prints help rather than silently starting a daemon.
        Some("--help") | Some("-h") | None => {
            print_help();
            return Ok(None);
        }
        _ => {}
    }

    let (mode, mut rest): (&str, _) = match first {
        Some("serve") => ("serve", rest),
        Some("capture") => ("capture", rest),
        Some(other) => return Err(format!("unknown argument: {other}")),
        None => unreachable!("no-argument case returns above"),
    };

    let mut out = PathBuf::from(DEFAULT_OUT);
    let mut output = None;
    let mut socket = None;
    let mut backend = BackendPref::Auto;

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--out" => {
                let value = rest
                    .next()
                    .ok_or_else(|| "missing value for --out".to_string())?;
                out = PathBuf::from(value);
            }
            "--output" => {
                let value = rest
                    .next()
                    .ok_or_else(|| "missing value for --output".to_string())?;
                output = Some(value.clone());
            }
            "--backend" => {
                let value = rest
                    .next()
                    .ok_or_else(|| "missing value for --backend".to_string())?;
                backend = parse_backend(value)?;
            }
            "--socket" => {
                let value = rest
                    .next()
                    .ok_or_else(|| "missing value for --socket".to_string())?;
                socket = Some(PathBuf::from(value));
            }
            other => return Err(format!("unknown argument for '{mode}': {other}")),
        }
    }

    let built = match mode {
        "serve" => Mode::Serve(ServeOpts { socket, backend }),
        _ => Mode::Capture(CaptureOpts {
            out,
            output,
            backend,
        }),
    };
    Ok(Some(built))
}

/// Parse `--backend auto|ext|wlr`.
fn parse_backend(value: &str) -> Result<BackendPref, String> {
    match value {
        "auto" => Ok(BackendPref::Auto),
        "ext" => Ok(BackendPref::Ext),
        "wlr" => Ok(BackendPref::Wlr),
        other => Err(format!(
            "invalid value for --backend: {other:?} (expected auto, ext or wlr)"
        )),
    }
}

fn print_help() {
    println!("emersia-daemon {VERSION} — Emersia host daemon");
    println!("\nUsage: emersia-daemon [serve|capture] [OPTIONS]\n");
    println!("Modes:");
    println!("  serve     run the control socket and capture engine (default)");
    println!("  capture   grab one frame, write a PNG, exit (QA / one-shot)");
    println!("\nOptions:");
    println!("  --socket PATH        control socket path [default: $XDG_RUNTIME_DIR/emersia/control.sock]");
    println!(
        "  --output NAME        output to capture (wl_output name, e.g. DP-1) [default: first]"
    );
    println!("  --backend WHICH      capture backend: auto, ext, wlr [default: auto]");
    println!("  --out PATH           capture-mode PNG path [default: {DEFAULT_OUT}]");
    println!("  -V, --version        print version and exit");
    println!("  -h, --help           print this help and exit");
    println!("\nExit codes: {EXIT_OK} success, {EXIT_FAILURE} runtime failure, {EXIT_USAGE} usage error.");
}

// ---------------------------------------------------------------------------
// serve mode
// ---------------------------------------------------------------------------

fn run_serve(opts: &ServeOpts) -> anyhow::Result<()> {
    install_signal_handlers();

    let socket_path = match &opts.socket {
        Some(p) => p.clone(),
        None => emersia_protocol::default_socket_path()
            .context("failed to determine the control socket path")?,
    };

    // The engine owns the Wayland connection; discovery validates that a
    // compositor and a usable backend exist before we bind anything.
    let engine =
        service::Engine::discover(opts.backend).context("capture engine discovery failed")?;

    // Load the trust store before anything can be controlled. A corrupt or
    // unreadable database is fatal: starting with an empty trust store would
    // silently mean "trust everything" (ADR 0005).
    let pairing_path = pairing::default_db_path()
        .context("failed to determine the pairing database path (is XDG_CONFIG_HOME set?)")?;
    let pairing_db = pairing::PairingDb::load(&pairing_path)
        .map_err(|e| anyhow::anyhow!("pairing database at {}: {e}", pairing_path.display()))?;
    println!("pairing database: {}", pairing_path.display());

    // The daemon's own long-term identity, generated on first run (ADR 0006).
    let host_key_path = pairing_path.with_file_name("host.key");
    let host_identity = crypto::HostIdentity::load_or_create(&host_key_path)
        .map_err(|e| anyhow::anyhow!("host identity at {}: {e}", host_key_path.display()))?;
    println!("host identity: {}", host_identity.public_key_hex());

    let svc = Arc::new(service::Service::new(pairing_db, &host_identity));
    let server = control::ControlServer::bind(&socket_path)?;

    println!("emersia-daemon {VERSION} — serving control socket");
    println!("control socket: {}", server.path().display());
    println!("capture backend: {}", engine.backend_name);
    for screen in &engine.screens {
        println!(
            "  output: {} {}x{}",
            screen.name, screen.width, screen.height
        );
    }
    println!("ready — control it with the `emersia` CLI");

    let serve_svc = Arc::clone(&svc);
    let server_thread =
        std::thread::spawn(move || -> anyhow::Result<()> { server.serve(serve_svc) });

    service::run_engine(engine, Arc::clone(&svc));

    // Engine exited (signal): stop the accept loop and clean up the socket.
    svc.request_shutdown();
    match server_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("emersia-daemon: control server: {err:#}"),
        Err(_) => eprintln!("emersia-daemon: control server thread panicked"),
    }
    println!("emersia-daemon: shut down cleanly");
    Ok(())
}

/// Graceful Ctrl-C / SIGTERM: flip a flag the engine and accept loop poll.
fn install_signal_handlers() {
    extern "C" fn on_signal(_sig: libc::c_int) {
        SIGNALLED.store(true, Ordering::SeqCst);
    }
    // SAFETY: installing a signal handler that only stores into a static
    // AtomicBool — async-signal-safe, no allocation, no locks.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// The service and control server poll this; kept separate from `Service`'s own
/// flag so the engine loop can observe the raw signal.
pub fn signal_received() -> bool {
    SIGNALLED.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// capture mode (the M1.1 one-shot probe)
// ---------------------------------------------------------------------------

fn run_capture(opts: &CaptureOpts) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .context("failed to connect to a Wayland compositor (is WAYLAND_DISPLAY set?)")?;

    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    // Kept alive for the connection's lifetime; events route through `qh`.
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::new();

    // 1st roundtrip: registry globals (+ our binds, flushed mid-loop).
    queue
        .roundtrip(&mut state)
        .context("failed while enumerating Wayland globals")?;
    // 2nd roundtrip: output geometry/mode/name events for those binds.
    queue
        .roundtrip(&mut state)
        .context("failed while reading output descriptions")?;

    let socket = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    println!("emersia-daemon {VERSION} — Wayland capture probe (M1.1)");
    println!("connected to Wayland socket {socket}");

    let labels = state.output_labels();
    println!("detected {} output(s):", labels.len());
    for (i, info) in state.outputs.iter().enumerate() {
        let model = if info.make.is_empty() {
            String::new()
        } else {
            format!("  {} {}", info.make, info.model)
        };
        println!(
            "  [{}] {} {}x{}  transform={:?}  scale={}{}",
            i, labels[i], info.width, info.height, info.transform, info.scale, model
        );
    }

    let has_ext = state.ext_source_mgr.is_some() && state.ext_copy_mgr.is_some();
    let has_wlr = state.wlr_mgr.is_some();
    println!(
        "backends: ext-image-copy-capture={}, wlr-screencopy={}",
        availability(has_ext),
        availability(has_wlr)
    );

    let index = capture::select_output(&labels, opts.output.as_deref())?;
    let backend = capture::choose_backend(opts.backend, has_ext, has_wlr)?;
    let output = state.outputs[index].proxy.clone();
    let transform = state.outputs[index].transform;
    println!(
        "selected output: {} [{} backend]",
        labels[index],
        backend.protocol()
    );

    let started = Instant::now();
    let frame = match backend {
        capture::Backend::Ext => capture::ext::capture(&mut queue, &mut state, &output)?,
        capture::Backend::Wlr => capture::wlr::capture(&mut queue, &mut state, &output, transform)?,
    };
    let elapsed = started.elapsed();

    frame.save(&opts.out).with_context(|| {
        format!(
            "capture succeeded but the frame could not be written to {}",
            opts.out.display()
        )
    })?;
    let written = std::fs::metadata(&opts.out).map(|m| m.len()).unwrap_or(0);

    println!(
        "captured {}x{} RGBA in {:.1} ms via {}",
        frame.width,
        frame.height,
        elapsed.as_secs_f64() * 1000.0,
        backend.protocol()
    );
    println!("wrote {} ({written} bytes)", opts.out.display());
    Ok(())
}

fn availability(available: bool) -> &'static str {
    if available {
        "available"
    } else {
        "missing"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("emersia-daemon".to_string())
            .chain(list.iter().map(|s| s.to_string()))
            .collect()
    }

    fn parse_ok(list: &[&str]) -> Mode {
        match parse_args(&args(list)).unwrap() {
            Some(mode) => mode,
            None => panic!("expected a mode for {list:?}"),
        }
    }

    fn parse_err(list: &[&str]) -> String {
        match parse_args(&args(list)) {
            Err(msg) => msg,
            _ => panic!("expected a usage error for {list:?}"),
        }
    }

    #[test]
    fn version_is_semver() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH, got {VERSION}");
        for p in parts {
            assert!(
                p.chars().all(|c| c.is_ascii_digit()),
                "non-numeric part in {VERSION}"
            );
        }
    }

    #[test]
    fn no_subcommand_prints_help() {
        // Starting a daemon must be explicit; bare invocation is not a footgun.
        assert!(matches!(parse_args(&args(&[])), Ok(None)));
    }

    #[test]
    fn serve_is_the_explicit_default_mode() {
        match parse_ok(&["serve"]) {
            Mode::Serve(opts) => {
                assert!(opts.socket.is_none());
                assert_eq!(opts.backend, BackendPref::Auto);
            }
            _ => panic!("expected serve mode"),
        }
    }

    #[test]
    fn explicit_modes_parse() {
        assert!(matches!(parse_ok(&["serve"]), Mode::Serve(_)));
        match parse_ok(&["capture"]) {
            Mode::Capture(opts) => assert_eq!(opts.out, PathBuf::from(DEFAULT_OUT)),
            _ => panic!("expected capture mode"),
        }
    }

    #[test]
    fn serve_options_parse() {
        match parse_ok(&["serve", "--socket", "/tmp/x.sock", "--backend", "wlr"]) {
            Mode::Serve(opts) => {
                assert_eq!(opts.socket, Some(PathBuf::from("/tmp/x.sock")));
                assert_eq!(opts.backend, BackendPref::Wlr);
            }
            _ => panic!("expected serve mode"),
        }
    }

    #[test]
    fn capture_options_parse() {
        match parse_ok(&[
            "capture",
            "--out",
            "/tmp/f.png",
            "--output",
            "DP-3",
            "--backend",
            "ext",
        ]) {
            Mode::Capture(opts) => {
                assert_eq!(opts.out, PathBuf::from("/tmp/f.png"));
                assert_eq!(opts.output.as_deref(), Some("DP-3"));
                assert_eq!(opts.backend, BackendPref::Ext);
            }
            _ => panic!("expected capture mode"),
        }
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(parse_err(&["--nope"]).contains("--nope"));
        assert!(parse_err(&["serve", "--nope"]).contains("--nope"));
        // Flags are scoped to their subcommand.
        assert!(parse_err(&["capture", "--out"]).contains("missing value"));
        assert!(parse_err(&["serve", "--socket"]).contains("missing value"));
        assert!(parse_err(&["capture", "--backend"]).contains("missing value"));
        assert!(parse_err(&["capture", "--backend", "vulkan"]).contains("vulkan"));
    }

    #[test]
    fn help_and_version_exit_early() {
        assert!(matches!(parse_args(&args(&["--help"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["-h"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["--version"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["-V"])), Ok(None)));
    }
}

//! `emersia-daemon` — Emersia host daemon.
//!
//! M1.1 (Wayland capture baseline): connect to the compositor over
//! `$WAYLAND_DISPLAY`, detect the connected outputs, capture exactly one test
//! frame with `ext-image-copy-capture` (falling back to `wlr-screencopy`) and
//! write it to disk as a PNG. No encoding and no network yet — those land
//! later in milestone M1.

mod capture;
mod frame;
mod shm;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context as _;
use capture::{Backend, BackendPref};
use wayland_client::Connection;

/// Crate version, e.g. `0.0.1`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the M1.1 test frame lands unless `--out` says otherwise.
const DEFAULT_OUT: &str = "/tmp/emersia-test-frame.png";

/// Exit code for a completed run.
const EXIT_OK: i32 = 0;
/// Exit code for a runtime failure (no compositor, capture error, I/O error).
const EXIT_FAILURE: i32 = 1;
/// Exit code for invalid command-line usage.
const EXIT_USAGE: i32 = 2;

/// Parsed command-line options.
struct Opts {
    out: PathBuf,
    output: Option<String>,
    backend: BackendPref,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match parse_args(&args) {
        // `--help` / `--version` already printed and asked us to stop.
        Ok(None) => {}
        Ok(Some(opts)) => {
            if let Err(err) = run(&opts) {
                eprintln!("emersia-daemon: {err:#}");
                std::process::exit(EXIT_FAILURE);
            }
        }
        Err(msg) => {
            eprintln!("emersia-daemon: {msg}");
            eprintln!("Try 'emersia-daemon --help' for more information.");
            std::process::exit(EXIT_USAGE);
        }
    }
}

/// Parse the command line. `Ok(None)` means help/version was printed.
fn parse_args(args: &[String]) -> Result<Option<Opts>, String> {
    let mut out = PathBuf::from(DEFAULT_OUT);
    let mut output = None;
    let mut backend = BackendPref::Auto;

    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("emersia-daemon {VERSION}");
                return Ok(None);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
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
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Some(Opts {
        out,
        output,
        backend,
    }))
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
    println!("emersia-daemon {VERSION} — Emersia host daemon (M1.1 Wayland capture)");
    println!("\nUsage: emersia-daemon [OPTIONS]\n");
    println!("Captures one test frame from a Wayland output and writes it to disk as a PNG.");
    println!("No encoding or streaming yet — that arrives later in milestone M1.\n");
    println!("Options:");
    println!("  --out PATH        output PNG path [default: {DEFAULT_OUT}]");
    println!("  --output NAME     output to capture (wl_output name, e.g. DP-1) [default: first]");
    println!("  --backend WHICH   capture backend: auto, ext, wlr [default: auto]");
    println!("  -V, --version     print version and exit");
    println!("  -h, --help        print this help and exit");
    println!(
        "\nExit codes: {EXIT_OK} success, {EXIT_FAILURE} runtime failure, \
         {EXIT_USAGE} usage error."
    );
}

/// Connect to Wayland, capture one frame, and write it out.
fn run(opts: &Opts) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .context("failed to connect to a Wayland compositor (is WAYLAND_DISPLAY set?)")?;

    let mut queue = conn.new_event_queue::<capture::State>();
    let qh = queue.handle();
    // Kept alive for the connection's lifetime; events route through `qh`.
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = capture::State::new();

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
        Backend::Ext => capture::ext::capture(&mut queue, &mut state, &output)?,
        Backend::Wlr => capture::wlr::capture(&mut queue, &mut state, &output, transform)?,
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
    fn defaults_are_applied() {
        let opts = parse_args(&args(&[])).unwrap().unwrap();
        assert_eq!(opts.out, PathBuf::from(DEFAULT_OUT));
        assert!(opts.output.is_none());
        assert_eq!(opts.backend, BackendPref::Auto);
    }

    #[test]
    fn parses_capture_options() {
        let opts = parse_args(&args(&[
            "--out",
            "/tmp/frame.png",
            "--output",
            "DP-3",
            "--backend",
            "wlr",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(opts.out, PathBuf::from("/tmp/frame.png"));
        assert_eq!(opts.output.as_deref(), Some("DP-3"));
        assert_eq!(opts.backend, BackendPref::Wlr);
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(parse_args(&args(&["--nope"])).is_err());
        assert!(parse_args(&args(&["--out"])).is_err());
        assert!(parse_args(&args(&["--output"])).is_err());
        assert!(parse_args(&args(&["--backend"])).is_err());
        assert!(parse_args(&args(&["--backend", "vulkan"])).is_err());
    }

    #[test]
    fn help_and_version_exit_early() {
        assert!(matches!(parse_args(&args(&["--help"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["-h"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["--version"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["-V"])), Ok(None)));
    }
}

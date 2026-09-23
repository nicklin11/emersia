//! `emersia` — local control CLI for the Emersia host daemon.
//!
//! Speaks the JSON-lines protocol from `emersia-protocol` over the Unix
//! control socket described in `docs/design/control-api.md`. Every command maps
//! 1:1 to a daemon command; the daemon owns all policy, this is a thin client.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use emersia_protocol::{
    decode_response, encode_request, Command, ErrorInfo, PairAction, PairArgs, Request, Response,
    RevokeArgs, SelectArgs, StartArgs, PROTOCOL_VERSION,
};

/// Crate version, e.g. `0.0.1`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Exit code for a completed command.
const EXIT_OK: i32 = 0;
/// Exit code for a runtime failure (daemon unreachable, command failed).
const EXIT_FAILURE: i32 = 1;
/// Exit code for invalid command-line usage.
const EXIT_USAGE: i32 = 2;

/// Correlation id: monotonic per invocation, unique enough for one-shot calls.
fn next_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("c{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(&args) {
        Ok(()) => ExitCode::from(EXIT_OK as u8),
        Err(Failure::Usage(msg)) => {
            eprintln!("emersia: {msg}");
            eprintln!("Try 'emersia --help' for more information.");
            ExitCode::from(EXIT_USAGE as u8)
        }
        Err(Failure::Runtime(msg)) => {
            eprintln!("emersia: {msg}");
            ExitCode::from(EXIT_FAILURE as u8)
        }
    }
}

#[derive(Debug)]
enum Failure {
    Usage(String),
    Runtime(String),
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure::Usage(msg)
    }
}

impl From<std::io::Error> for Failure {
    fn from(err: std::io::Error) -> Self {
        Failure::Runtime(format!("socket I/O failed: {err}"))
    }
}

/// Parsed invocation.
struct Invocation {
    socket: Option<std::path::PathBuf>,
    json: bool,
    command: Command,
}

/// Commands that take no further arguments.
fn simple_command(name: &str) -> Option<Command> {
    Some(match name {
        "status" => Command::Status,
        "stop" => Command::Stop,
        "events" => Command::Events,
        "devices" => Command::Devices,
        _ => return None,
    })
}

fn run(args: &[String]) -> Result<(), Failure> {
    let invocation = parse_args(args)?;
    let Some(invocation) = invocation else {
        return Ok(());
    };
    let socket = match invocation.socket {
        Some(path) => path,
        None => emersia_protocol::default_socket_path().map_err(|err| {
            Failure::Runtime(format!(
                "could not determine the control socket path: {err}; \
                 is XDG_RUNTIME_DIR set?"
            ))
        })?,
    };

    let request = Request {
        v: PROTOCOL_VERSION,
        id: next_id(),
        command: invocation.command,
    };
    let response = send(&socket, &request)?;
    print_response(&response, invocation.json)
}

/// Connect, send one request line, read one response line.
fn send(socket: &std::path::Path, request: &Request) -> Result<Response, Failure> {
    let mut stream = UnixStream::connect(socket).map_err(|err| {
        Failure::Runtime(format!(
            "could not reach emersia-daemon at {}: {err}\nIs the daemon running? \
             (systemctl --user status emersia-daemon)",
            socket.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| Failure::Runtime(format!("failed to set read timeout: {e}")))?;

    let line = encode_request(request)
        .map_err(|err| Failure::Runtime(format!("failed to encode request: {err}")))?;
    stream
        .write_all(line.as_bytes())
        .map_err(|err| Failure::Runtime(format!("failed to send request: {err}")))?;
    stream
        .flush()
        .map_err(|err| Failure::Runtime(format!("failed to flush request: {err}")))?;

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    let read = reader
        .read_line(&mut buf)
        .map_err(|err| Failure::Runtime(format!("failed to read response: {err}")))?;
    if read == 0 {
        return Err(Failure::Runtime(
            "daemon closed the connection without responding".into(),
        ));
    }
    decode_response(buf.trim())
        .map_err(|err| Failure::Runtime(format!("malformed response from daemon: {err}")))
}

/// Print a response; a daemon-side error is a CLI runtime failure.
fn print_response(response: &Response, raw_json: bool) -> Result<(), Failure> {
    match response {
        Response::Ok(ok) => {
            if raw_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ok.ok).unwrap_or_default()
                );
            } else {
                print_human(&ok.ok);
            }
            Ok(())
        }
        Response::Err(err) => {
            if raw_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&err.error).unwrap_or_default()
                );
            }
            Err(Failure::Runtime(describe_error(&err.error)))
        }
    }
}

/// Human-readable rendering of the common payload shapes.
fn print_human(payload: &serde_json::Value) {
    if let Some(arr) = payload.get("screens").and_then(|v| v.as_array()) {
        if arr.is_empty() {
            println!("no capturable screens");
        } else {
            println!("screens:");
            for s in arr {
                let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let kind = s.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                let w = s.get("width").and_then(|v| v.as_i64()).unwrap_or(0);
                let h = s.get("height").and_then(|v| v.as_i64()).unwrap_or(0);
                let transform = s
                    .get("transform")
                    .and_then(|v| v.as_str())
                    .unwrap_or("normal");
                println!("  {name}  {kind}  {w}x{h}  transform={transform}");
            }
        }
        return;
    }

    if payload.get("devices").is_some() {
        let arr = payload["devices"].as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            println!("no paired devices");
        } else {
            println!("devices:");
            for d in arr {
                let id = d.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let name = d.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let revoked = d.get("revoked").and_then(|v| v.as_bool()).unwrap_or(false);
                let state = if revoked { "revoked" } else { "active" };
                println!("  {id}  {name}  [{state}]");
            }
        }
        return;
    }

    // Default: key/value status rendering.
    if let Some(obj) = payload.as_object() {
        let streaming = obj
            .get("streaming")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let state = if streaming { "streaming" } else { "idle" };
        println!("daemon: {state}");
        for (key, value) in obj {
            if key == "streaming" {
                continue;
            }
            let rendered = match value {
                serde_json::Value::Null => "none".to_string(),
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Bool(b) => b.to_string(),
                other => other.to_string(),
            };
            println!("  {key}: {rendered}");
        }
        return;
    }
    println!("{payload}");
}

fn describe_error(error: &ErrorInfo) -> String {
    format!("daemon error [{}]: {}", code_token(error), error.message)
}

fn code_token(error: &ErrorInfo) -> &'static str {
    use emersia_protocol::ErrorCode::*;
    match error.code {
        UnknownCmd => "unknown_cmd",
        NotPaired => "not_paired",
        Busy => "busy",
        NoCaptureBackend => "no_capture_backend",
        Denied => "denied",
        InvalidArgs => "invalid_args",
        NotFound => "not_found",
        Internal => "internal",
    }
}

/// Parse the command line. `Ok(None)` means help/version was printed.
///
/// Global options are accepted before or after the command, so both
/// `emersia --socket X status` and `emersia status --socket X` work.
fn parse_args(args: &[String]) -> Result<Option<Invocation>, Failure> {
    let mut socket = None;
    let mut json = false;
    let mut fps: Option<u32> = None;
    let mut device_name: Option<String> = None;
    let mut public_key: Option<String> = None;
    let mut command_name: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();

    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("emersia {VERSION}");
                return Ok(None);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            "--socket" => {
                let value = rest
                    .next()
                    .ok_or_else(|| Failure::Usage("missing value for --socket".into()))?;
                socket = Some(std::path::PathBuf::from(value));
            }
            "--json" => json = true,
            "--fps" => {
                let value = rest
                    .next()
                    .ok_or_else(|| Failure::Usage("missing value for --fps".into()))?;
                fps = Some(value.parse::<u32>().map_err(|_| {
                    Failure::Usage(format!("--fps must be a number, got {value:?}"))
                })?);
            }
            "--name" => {
                let value = rest
                    .next()
                    .ok_or_else(|| Failure::Usage("missing value for --name".into()))?;
                device_name = Some(value.to_string());
            }
            "--public-key" => {
                let value = rest
                    .next()
                    .ok_or_else(|| Failure::Usage("missing value for --public-key".into()))?;
                public_key = Some(value.to_string());
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(Failure::Usage(format!("unknown flag: {other}")));
            }
            other => {
                if command_name.is_none() {
                    command_name = Some(other.to_string());
                } else {
                    positionals.push(other.to_string());
                }
            }
        }
    }

    let Some(name) = command_name else {
        print_help();
        return Ok(None);
    };
    if name == "help" {
        print_help();
        return Ok(None);
    }

    let command = build_command(&name, &positionals, fps, device_name, public_key)?;
    Ok(Some(Invocation {
        socket,
        json,
        command,
    }))
}

fn build_command(
    name: &str,
    positionals: &[String],
    fps: Option<u32>,
    device_name: Option<String>,
    public_key: Option<String>,
) -> Result<Command, Failure> {
    if let Some(command) = simple_command(name) {
        if !positionals.is_empty() {
            return Err(Failure::Usage(format!(
                "'{name}' takes no arguments (got {positionals:?})"
            )));
        }
        return Ok(command);
    }

    match name {
        "screens" => Ok(Command::Screens),
        "start" => {
            if positionals.len() > 1 {
                return Err(Failure::Usage(
                    "'start' takes at most one output name".into(),
                ));
            }
            Ok(Command::Start(StartArgs {
                output: positionals.first().cloned(),
                fps,
            }))
        }
        "pair" => {
            let action = match positionals.first().map(String::as_str) {
                Some("new") => PairAction::New,
                Some("accept") => PairAction::Accept,
                other => {
                    return Err(Failure::Usage(format!(
                        "'pair' needs 'new' or 'accept' (got {other:?})"
                    )))
                }
            };
            // `accept` cannot work without a code, a name and a public key.
            if action == PairAction::Accept {
                if positionals.get(1).is_none() {
                    return Err(Failure::Usage(
                        "'pair accept' needs a code (e.g. emersia pair accept 123-456 --name …)"
                            .into(),
                    ));
                }
                if device_name.is_none() {
                    return Err(Failure::Usage("'pair accept' needs --name".into()));
                }
                if public_key.is_none() {
                    return Err(Failure::Usage("'pair accept' needs --public-key".into()));
                }
            }
            Ok(Command::Pair(PairArgs {
                action,
                code: positionals.get(1).cloned(),
                name: device_name,
                public_key,
            }))
        }
        "revoke" => {
            let device = positionals
                .first()
                .cloned()
                .ok_or_else(|| Failure::Usage("'revoke' needs a device id".into()))?;
            Ok(Command::Revoke(RevokeArgs { device }))
        }
        "select" => Ok(Command::Select(SelectArgs {
            targets: positionals.to_vec(),
        })),
        other => Err(Failure::Usage(format!("unknown command: {other}"))),
    }
}

fn print_help() {
    println!("emersia {VERSION} — Emersia host control CLI");
    println!("\nUsage: emersia [options] <command> [args]\n");
    println!("Commands:");
    println!("  status                 daemon and stream state");
    println!("  screens                list capturable outputs/windows");
    println!("  start [OUTPUT]         begin capture streaming");
    println!("  stop                   end capture streaming");
    println!("  events                 current state snapshot (follow-mode later)");
    println!("  pair new                 mint a short-lived pairing code");
    println!("  pair accept CODE --name N --public-key HEX");
    println!("                           register a device identity");
    println!("  devices                  list paired devices");
    println!("  revoke <id>              revoke a paired device");
    println!("  select [TARGET…]         set capture targets (empty = none)");
    println!("\nOptions:");
    println!("  --socket PATH   control socket [default: $XDG_RUNTIME_DIR/emersia/control.sock]");
    println!("  --fps N         target capture rate for 'start'");
    println!("  --name NAME     display name for 'pair accept'");
    println!("  --public-key H  hex X25519 public key for 'pair accept'");
    println!("  --json          print the raw JSON payload");
    println!("  -V, --version   print version and exit");
    println!("  -h, --help      print this help and exit");
    println!("\nExit codes: {EXIT_OK} success, {EXIT_FAILURE} daemon/runtime failure, {EXIT_USAGE} usage error.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("emersia".to_string())
            .chain(list.iter().map(|s| s.to_string()))
            .collect()
    }

    fn parsed(list: &[&str]) -> Invocation {
        parse_args(&args(list)).unwrap().expect("a command")
    }

    #[test]
    fn version_is_semver() {
        assert_eq!(
            VERSION.matches('.').count(),
            2,
            "expected MAJOR.MINOR.PATCH"
        );
    }

    #[test]
    fn simple_commands_map_one_to_one() {
        assert_eq!(parsed(&["status"]).command, Command::Status);
        assert_eq!(parsed(&["stop"]).command, Command::Stop);
        assert_eq!(parsed(&["events"]).command, Command::Events);
        assert_eq!(parsed(&["devices"]).command, Command::Devices);
        assert_eq!(parsed(&["screens"]).command, Command::Screens);
    }

    #[test]
    fn start_parses_output_and_fps() {
        let inv = parsed(&["start", "DP-3", "--fps", "60"]);
        match inv.command {
            Command::Start(args) => {
                assert_eq!(args.output.as_deref(), Some("DP-3"));
                assert_eq!(args.fps, Some(60));
            }
            other => panic!("expected start, got {other:?}"),
        }
        // Defaults when bare.
        match parsed(&["start"]).command {
            Command::Start(args) => {
                assert!(args.output.is_none());
                assert!(args.fps.is_none());
            }
            other => panic!("expected start, got {other:?}"),
        }
    }

    #[test]
    fn pair_new_needs_nothing_else() {
        assert_eq!(
            parsed(&["pair", "new"]).command,
            Command::Pair(PairArgs {
                action: PairAction::New,
                code: None,
                name: None,
                public_key: None,
            })
        );
    }

    #[test]
    fn pair_accept_carries_code_name_and_key() {
        let inv = parsed(&[
            "pair",
            "accept",
            "123-456",
            "--name",
            "Quest 3",
            "--public-key",
            "aabbcc",
        ]);
        match inv.command {
            Command::Pair(a) => {
                assert_eq!(a.action, PairAction::Accept);
                assert_eq!(a.code.as_deref(), Some("123-456"));
                assert_eq!(a.name.as_deref(), Some("Quest 3"));
                assert_eq!(a.public_key.as_deref(), Some("aabbcc"));
            }
            other => panic!("expected pair, got {other:?}"),
        }
    }

    #[test]
    fn pair_accept_without_required_flags_is_a_usage_error() {
        let bad = |list: &[&str]| parse_args(&args(list)).is_err();
        assert!(bad(&["pair", "accept"]), "needs a code");
        assert!(bad(&["pair", "accept", "123-456"]), "needs a name and key");
        assert!(
            bad(&["pair", "accept", "123-456", "--name", "Quest"]),
            "needs a public key"
        );
    }

    #[test]
    fn revoke_and_select_parse() {
        match parsed(&["revoke", "abc123"]).command {
            Command::Revoke(a) => assert_eq!(a.device, "abc123"),
            other => panic!("expected revoke, got {other:?}"),
        }
        match parsed(&["select", "DP-1", "HDMI-A-1"]).command {
            Command::Select(a) => assert_eq!(a.targets, vec!["DP-1", "HDMI-A-1"]),
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn socket_and_json_flags_parse() {
        // Flags work on either side of the command.
        for list in [
            ["status", "--socket", "/tmp/x.sock", "--json"],
            ["--socket", "/tmp/x.sock", "--json", "status"],
        ] {
            let inv = parsed(&list);
            assert_eq!(inv.socket, Some(std::path::PathBuf::from("/tmp/x.sock")));
            assert!(inv.json);
            assert_eq!(inv.command, Command::Status);
        }
    }

    #[test]
    fn rejects_bad_usage() {
        let bad = |list: &[&str]| parse_args(&args(list)).is_err();
        assert!(bad(&["teleport"]));
        assert!(bad(&["--nope"]));
        assert!(bad(&["--socket"]));
        assert!(bad(&["--fps"]));
        assert!(bad(&["--fps", "fast"]));
        assert!(bad(&["status", "extra-arg"]));
        assert!(bad(&["pair"]));
        assert!(bad(&["revoke"]));
        assert!(bad(&["start", "a", "b"]));
    }

    #[test]
    fn help_and_version_exit_early() {
        assert!(matches!(parse_args(&args(&["--help"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["-h"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["help"])), Ok(None)));
        assert!(matches!(parse_args(&args(&["--version"])), Ok(None)));
        assert!(matches!(parse_args(&args(&[])), Ok(None)));
    }

    #[test]
    fn error_tokens_map_to_stable_strings() {
        use emersia_protocol::ErrorCode;
        let cases = [
            (ErrorCode::UnknownCmd, "unknown_cmd"),
            (ErrorCode::NotPaired, "not_paired"),
            (ErrorCode::Busy, "busy"),
            (ErrorCode::NoCaptureBackend, "no_capture_backend"),
            (ErrorCode::Denied, "denied"),
            (ErrorCode::InvalidArgs, "invalid_args"),
            (ErrorCode::NotFound, "not_found"),
            (ErrorCode::Internal, "internal"),
        ];
        for (code, token) in cases {
            let info = ErrorInfo::new(code, "x");
            assert_eq!(describe_error(&info), format!("daemon error [{token}]: x"));
        }
    }
}

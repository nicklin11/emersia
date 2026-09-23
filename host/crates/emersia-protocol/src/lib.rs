//! Shared control-plane wire protocol for Emersia.
//!
//! One definition of the format described in `docs/design/control-api.md`,
//! used by both `emersia-daemon` (server) and `emersia` (client).
//!
//! Transport is a Unix domain socket; framing is **JSON lines** — one JSON
//! object per line, UTF-8, `\n`-terminated.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Protocol version. Bumped on breaking changes; a mismatch is rejected
/// rather than silently misinterpreted.
pub const PROTOCOL_VERSION: u8 = 1;

/// Environment variable holding the runtime directory (socket lives inside).
pub const RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";

/// Socket filename inside the per-user runtime directory.
pub const SOCKET_FILE: &str = "control.sock";

/// Default socket path: `$XDG_RUNTIME_DIR/emersia/control.sock`.
pub fn default_socket_path() -> std::io::Result<PathBuf> {
    let dir = std::env::var_os(RUNTIME_DIR_ENV).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{RUNTIME_DIR_ENV} is not set"),
        )
    })?;
    Ok(Path::new(&dir).join("emersia").join(SOCKET_FILE))
}

/// Stable, machine-readable error tokens. These are part of the wire contract:
/// clients may switch on them, so do not rename without bumping the version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Command not recognised by this daemon version.
    UnknownCmd,
    /// Requested device is not paired.
    NotPaired,
    /// Daemon is busy with an operation that conflicts.
    Busy,
    /// No capture backend usable on this compositor.
    NoCaptureBackend,
    /// Auth/permission failure (e.g. peer UID mismatch).
    Denied,
    /// Request arguments missing or malformed.
    InvalidArgs,
    /// Referenced entity (device, output) does not exist.
    NotFound,
    /// Unexpected server-side failure.
    Internal,
}

/// An error reply: a stable `code` plus a human-facing `message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorInfo {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Commands understood by the daemon. Extensible; unknown wire commands
/// decode to [`Command::Unknown`] so a newer CLI gets a clean `unknown_cmd`
/// instead of a parse failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Daemon + stream state.
    Status,
    /// List capturable outputs/windows.
    Screens,
    /// Begin capture streaming.
    Start(StartArgs),
    /// End capture streaming.
    Stop,
    /// Subscribe to state changes.
    Events,
    /// Start/accept pairing.
    Pair(PairArgs),
    /// List paired devices.
    Devices,
    /// Remove a paired device.
    Revoke(RevokeArgs),
    /// Set capture targets.
    Select(SelectArgs),
    /// Anything this daemon does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartArgs {
    /// Optional output name to capture; `None` = daemon's default choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Optional target frames per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairArgs {
    /// `new` to generate a code, `accept` to redeem one.
    pub action: PairAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairAction {
    New,
    Accept,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeArgs {
    pub device: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectArgs {
    /// Empty list means "no targets" (capture stops).
    #[serde(default)]
    pub targets: Vec<String>,
}

/// A client request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub v: u8,
    /// Client-chosen correlation id, echoed back verbatim.
    pub id: String,
    #[serde(flatten)]
    pub command: Command,
}

/// A successful reply; `result` is command-specific JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseOk {
    pub v: u8,
    pub id: String,
    pub ok: serde_json::Value,
}

/// A failed reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseErr {
    pub v: u8,
    pub id: String,
    pub error: ErrorInfo,
}

/// A reply: exactly one of the two shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Ok(ResponseOk),
    Err(ResponseErr),
}

/// Decode errors are protocol-level, surfaced to the client as a message.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("malformed JSON line: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported protocol version {found} (this daemon speaks {expected})")]
    Version { found: u8, expected: u8 },
}

/// Serialize a request to a single JSON line (including the trailing newline).
pub fn encode_request(req: &Request) -> Result<String, ProtocolError> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    Ok(line)
}

/// Serialize a response to a single JSON line (including the trailing newline).
pub fn encode_response(res: &Response) -> Result<String, ProtocolError> {
    let mut line = serde_json::to_string(res)?;
    line.push('\n');
    Ok(line)
}

/// Parse a request from a JSON line, rejecting unsupported protocol versions.
pub fn decode_request(line: &str) -> Result<Request, ProtocolError> {
    let req: Request = serde_json::from_str(line.trim())?;
    if req.v != PROTOCOL_VERSION {
        return Err(ProtocolError::Version {
            found: req.v,
            expected: PROTOCOL_VERSION,
        });
    }
    Ok(req)
}

/// Parse a response from a JSON line.
pub fn decode_response(line: &str) -> Result<Response, ProtocolError> {
    Ok(serde_json::from_str(line.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_through_a_line() {
        let req = Request {
            v: PROTOCOL_VERSION,
            id: "42".into(),
            command: Command::Status,
        };
        let line = encode_request(&req).unwrap();
        assert!(line.ends_with('\n'), "lines are newline-terminated");
        assert!(!line[..line.len() - 1].contains('\n'), "exactly one line");
        assert_eq!(decode_request(&line).unwrap(), req);
    }

    #[test]
    fn start_carries_args() {
        let req = Request {
            v: PROTOCOL_VERSION,
            id: "a".into(),
            command: Command::Start(StartArgs {
                output: Some("DP-3".into()),
                fps: Some(60),
            }),
        };
        let line = encode_request(&req).unwrap();
        assert_eq!(decode_request(&line).unwrap(), req);
    }

    #[test]
    fn responses_roundtrip() {
        let ok = Response::Ok(ResponseOk {
            v: PROTOCOL_VERSION,
            id: "1".into(),
            ok: serde_json::json!({"streaming": false}),
        });
        assert_eq!(decode_response(&encode_response(&ok).unwrap()).unwrap(), ok);

        let err = Response::Err(ResponseErr {
            v: PROTOCOL_VERSION,
            id: "1".into(),
            error: ErrorInfo::new(ErrorCode::UnknownCmd, "nope"),
        });
        assert_eq!(
            decode_response(&encode_response(&err).unwrap()).unwrap(),
            err
        );
    }

    #[test]
    fn unknown_command_decodes_to_unknown() {
        // A newer CLI sending a command this daemon lacks must not be a parse
        // error — it becomes Command::Unknown and yields an `unknown_cmd` reply.
        let line = r#"{"v":1,"id":"9","cmd":"teleport"}"#;
        let req = decode_request(line).unwrap();
        assert_eq!(req.command, Command::Unknown);
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let line = r#"{"v":99,"id":"1","cmd":"status"}"#;
        let err = decode_request(line).unwrap_err();
        assert!(matches!(err, ProtocolError::Version { found: 99, .. }));
    }

    #[test]
    fn error_tokens_are_stable() {
        // These strings are the wire contract; changing one is a breaking change.
        let pairs = [
            (ErrorCode::UnknownCmd, "\"unknown_cmd\""),
            (ErrorCode::NotPaired, "\"not_paired\""),
            (ErrorCode::Busy, "\"busy\""),
            (ErrorCode::NoCaptureBackend, "\"no_capture_backend\""),
            (ErrorCode::Denied, "\"denied\""),
            (ErrorCode::InvalidArgs, "\"invalid_args\""),
            (ErrorCode::NotFound, "\"not_found\""),
            (ErrorCode::Internal, "\"internal\""),
        ];
        for (code, json) in pairs {
            assert_eq!(serde_json::to_string(&code).unwrap(), json);
        }
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(matches!(
            decode_request("{not json"),
            Err(ProtocolError::Json(_))
        ));
    }
}

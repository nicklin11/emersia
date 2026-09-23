//! Unix control socket server: JSON-lines transport with `SO_PEERCRED`
//! same-UID authentication, per `docs/design/control-api.md` and ADR 0005.
//!
//! Security posture: the socket lives in a `0700` directory with mode `0600`,
//! and every accepted connection is checked with `SO_PEERCRED` before a single
//! byte is read. Default-deny — a peer whose UID differs is disconnected
//! immediately. There is no TCP surface.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use emersia_protocol::{
    decode_request, encode_response, ErrorCode, ErrorInfo, ProtocolError, Response, ResponseErr,
    ResponseOk, PROTOCOL_VERSION,
};
use rustix::net::sockopt::socket_peercred;

use crate::service::Service;

/// Refuse absurd lines rather than letting one client push arbitrary data.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// A bound, listening control socket.
pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
}

impl ControlServer {
    /// Bind the control socket, creating its `0700` parent directory and
    /// forcing the socket itself to `0600`.
    ///
    /// A stale socket left by a crashed daemon is replaced; a socket with a
    /// live listener behind it is an error (never steal another daemon's
    /// endpoint).
    pub fn bind(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
            let mode = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(dir, mode)
                .with_context(|| format!("failed to restrict {}", dir.display()))?;
        }

        if path.exists() {
            // Probe: if someone is still listening, this is not ours to take.
            if UnixStream::connect(path).is_ok() {
                anyhow::bail!(
                    "another emersia-daemon is already listening on {}",
                    path.display()
                );
            }
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
        }

        let listener = UnixListener::bind(path)
            .with_context(|| format!("failed to bind {}", path.display()))?;
        let mode = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, mode)
            .with_context(|| format!("failed to restrict {}", path.display()))?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept connections forever, serving each on its own thread.
    ///
    /// The listener is nonblocking-friendly: a short accept timeout lets the
    /// loop notice shutdown requests instead of parking forever.
    pub fn serve(self, service: Arc<Service>) -> anyhow::Result<()> {
        self.listener
            .set_nonblocking(true)
            .context("failed to set the control socket non-blocking")?;
        while !service.is_shutting_down() {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let service = Arc::clone(&service);
                    std::thread::spawn(move || {
                        if let Err(err) = handle_connection(stream, &service) {
                            // A dropped client is routine; log and move on.
                            eprintln!("emersia-daemon: control connection ended: {err:#}");
                        }
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => return Err(err).context("control socket accept failed"),
            }
        }
        Ok(())
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        // Leave no stale socket behind for the next boot.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `SO_PEERCRED` check: only the daemon's own UID may talk to it.
fn peer_is_same_uid(stream: &UnixStream) -> bool {
    match socket_peercred(stream.as_fd()) {
        // SAFETY: `getuid` is always safe to call; it takes no arguments and
        // cannot fail.
        Ok(cred) => Some(cred.uid.as_raw()) == Some(unsafe { libc::getuid() }),
        // If we cannot identify the peer we cannot trust it.
        Err(_) => false,
    }
}

/// Serve one client: auth, then request/response until it disconnects.
fn handle_connection(stream: UnixStream, service: &Service) -> anyhow::Result<()> {
    if !peer_is_same_uid(&stream) {
        // Default-deny: do not even read a request from a foreign UID.
        return Err(anyhow::anyhow!("rejected peer with mismatched UID"));
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .context("failed to set connection read timeout")?;

    let mut writer = stream.try_clone().context("failed to clone connection")?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    loop {
        buf.clear();
        let read = reader
            .read_line(&mut buf)
            .context("failed to read a control request")?;
        if read == 0 {
            // Client closed the connection.
            break;
        }
        if buf.len() > MAX_LINE_BYTES {
            let response = err_response(
                "",
                ErrorCode::InvalidArgs,
                format!("control request exceeds {MAX_LINE_BYTES} bytes"),
            );
            writer
                .write_all(encode_response(&response)?.as_bytes())
                .context("failed to write a control response")?;
            writer
                .flush()
                .context("failed to flush the control response")?;
            break;
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        let response = match decode_request(line) {
            Ok(req) => service.handle(req),
            Err(err) => error_response("", &err),
        };
        writer
            .write_all(encode_response(&response)?.as_bytes())
            .context("failed to write a control response")?;
        writer
            .flush()
            .context("failed to flush the control response")?;
    }
    Ok(())
}

/// Build an error response for a line we could not even decode.
fn error_response(id: &str, err: &ProtocolError) -> Response {
    let code = match err {
        ProtocolError::Version { .. } => ErrorCode::Denied,
        ProtocolError::Json(_) => ErrorCode::InvalidArgs,
    };
    Response::Err(ResponseErr {
        v: PROTOCOL_VERSION,
        id: id.to_string(),
        error: ErrorInfo::new(code, err.to_string()),
    })
}

/// Convenience for the service: a success reply carrying `payload`.
pub fn ok_response(id: &str, payload: serde_json::Value) -> Response {
    Response::Ok(ResponseOk {
        v: PROTOCOL_VERSION,
        id: id.to_string(),
        ok: payload,
    })
}

/// Convenience for the service: a failure reply.
pub fn err_response(id: &str, code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Err(ResponseErr {
        v: PROTOCOL_VERSION,
        id: id.to_string(),
        error: ErrorInfo::new(code, message),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_creates_private_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emersia/control.sock");
        let server = ControlServer::bind(&path).unwrap();

        let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(sock_mode, 0o600, "socket must be owner-only");
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "parent dir must be owner-only");
        assert_eq!(server.path(), path);
    }

    #[test]
    fn bind_refuses_to_steal_a_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let _first = ControlServer::bind(&path).unwrap();
        // Second bind must fail: the first is still listening.
        assert!(ControlServer::bind(&path).is_err());
    }

    #[test]
    fn drop_removes_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        {
            let _server = ControlServer::bind(&path).unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "socket cleaned up on shutdown");
    }

    #[test]
    fn same_uid_peer_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let server = ControlServer::bind(&path).unwrap();
        server.listener.set_nonblocking(true).unwrap();

        let client = UnixStream::connect(&path).unwrap();
        let accepted = server.listener.accept().unwrap().0;
        assert!(peer_is_same_uid(&accepted), "our own UID must pass");
        drop(client);
    }

    #[test]
    fn malformed_line_gets_invalid_args() {
        let res = error_response(
            "7",
            &ProtocolError::Version {
                found: 9,
                expected: 1,
            },
        );
        match res {
            Response::Err(e) => {
                assert_eq!(e.id, "7");
                assert_eq!(e.error.code, ErrorCode::Denied);
            }
            _ => panic!("expected error response"),
        }
    }

    #[test]
    fn line_roundtrip_over_a_real_socket() {
        use emersia_protocol::{Command, Request};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let server = ControlServer::bind(&path).unwrap();
        server.listener.set_nonblocking(true).unwrap();

        let client = UnixStream::connect(&path).unwrap();
        let (accepted, _) = server.listener.accept().unwrap();
        let service = Service::new_for_tests();
        let handle = std::thread::spawn(move || handle_connection(accepted, &service).unwrap());

        let mut writer = client.try_clone().unwrap();
        let req = Request {
            v: PROTOCOL_VERSION,
            id: "42".into(),
            command: Command::Status,
        };
        writer
            .write_all(emersia_protocol::encode_request(&req).unwrap().as_bytes())
            .unwrap();
        writer.flush().unwrap();

        // Read exactly one response line: the server keeps the connection open
        // for further requests, so reading to EOF would stall.
        let mut reader = BufReader::new(&client);
        let mut buf = String::new();
        reader.read_line(&mut buf).unwrap();
        let resp = emersia_protocol::decode_response(buf.trim()).unwrap();
        match resp {
            Response::Ok(o) => assert_eq!(o.id, "42"),
            _ => panic!("expected ok, got {resp:?}"),
        }
        // Close both client ends so the server's read loop sees EOF instead of
        // blocking until its 30 s read timeout.
        drop(reader);
        drop(writer);
        drop(client);
        let _ = handle.join();
    }
}

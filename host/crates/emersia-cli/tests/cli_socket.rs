//! Process-level integration test for the `emersia` CLI.
//!
//! Spawns the real binary against a real Unix socket served by a stub, so the
//! transport, wire format and exit-code contract are all exercised end to end.
//! No Wayland session is required, so this runs in CI.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

use emersia_protocol::{decode_request, encode_response, Response, ResponseOk, PROTOCOL_VERSION};

/// Start a stub daemon that answers every request with `payload`.
fn stub_daemon(dir: &tempfile::TempDir, payload: serde_json::Value) -> std::path::PathBuf {
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).expect("stub listener binds");
    std::thread::spawn(move || {
        // Serve a couple of connections; the CLI is one-shot per invocation.
        for _ in 0..4 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().expect("clone stream");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            // Echo back the caller's id so correlation is verified too.
            let id = decode_request(line.trim())
                .map(|r| r.id)
                .unwrap_or_else(|_| "unknown".to_string());
            let response = Response::Ok(ResponseOk {
                v: PROTOCOL_VERSION,
                id,
                ok: payload.clone(),
            });
            let _ = writer.write_all(encode_response(&response).unwrap().as_bytes());
            let _ = writer.flush();
        }
    });
    path
}

fn run_cli(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_emersia"))
        .args(args)
        .output()
        .expect("emersia binary runs")
}

#[test]
fn status_prints_payload_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let sock = stub_daemon(
        &dir,
        serde_json::json!({"streaming": false, "backend": "wlr-screencopy"}),
    );

    let out = run_cli(&["--socket", sock.to_str().unwrap(), "--json", "status"]);
    assert!(out.status.success(), "status should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(parsed["backend"], "wlr-screencopy");
    assert_eq!(parsed["streaming"], false);
}

#[test]
fn flags_work_before_or_after_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let sock = stub_daemon(&dir, serde_json::json!({"streaming": true}));

    for args in [
        vec!["--socket", sock.to_str().unwrap(), "status"],
        vec!["status", "--socket", sock.to_str().unwrap()],
    ] {
        let out = run_cli(&args);
        assert!(out.status.success(), "{args:?} should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("daemon: streaming"), "{args:?}: {stdout}");
    }
}

#[test]
fn screens_are_listed_for_humans() {
    let dir = tempfile::tempdir().unwrap();
    let sock = stub_daemon(
        &dir,
        serde_json::json!({"screens": [
            {"kind": "output", "name": "DP-3", "width": 1920, "height": 1080, "transform": "normal"}
        ]}),
    );

    let out = run_cli(&["--socket", sock.to_str().unwrap(), "screens"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("DP-3"), "{stdout}");
    assert!(stdout.contains("1920x1080"), "{stdout}");
}

#[test]
fn daemon_error_response_exits_nonzero_with_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let response = "{\"v\":1,\"id\":\"x\",\"error\":{\"code\":\"not_found\",\"message\":\"no such output\"}}\n";
            let _ = writer.write_all(response.as_bytes());
            let _ = writer.flush();
        }
    });

    let out = run_cli(&["--socket", path.to_str().unwrap(), "start"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "daemon error is a runtime failure"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not_found"), "{stderr}");
    assert!(stderr.contains("no such output"), "{stderr}");
}

#[test]
fn unreachable_daemon_exits_one_with_hint() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.sock");
    let out = run_cli(&["--socket", missing.to_str().unwrap(), "status"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("could not reach"), "{stderr}");
    assert!(stderr.contains("daemon running"), "{stderr}");
}

#[test]
fn unknown_command_exits_two() {
    let out = run_cli(&["teleport"]);
    assert_eq!(out.status.code(), Some(2), "usage error is exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown command"), "{stderr}");
}

#[test]
fn help_and_version_exit_zero() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["--version"],
        vec!["-V"],
        vec![],
    ] {
        let out = run_cli(&args);
        assert!(out.status.success(), "{args:?} should exit 0");
    }
}

#[test]
fn request_id_is_echoed_by_the_daemon() {
    // Proves the CLI sends a correlation id the daemon can echo (the stub
    // replies with whatever id it parsed), and that ids are not hardcoded.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        for _ in 0..2 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let req = decode_request(line.trim()).unwrap();
            seen2.lock().unwrap().push(req.id.clone());
            let response = Response::Ok(ResponseOk {
                v: PROTOCOL_VERSION,
                id: req.id,
                ok: serde_json::json!({}),
            });
            let _ = writer.write_all(encode_response(&response).unwrap().as_bytes());
        }
    });

    let out = run_cli(&["--socket", path.to_str().unwrap(), "status"]);
    assert!(out.status.success());
    let ids = seen.lock().unwrap().clone();
    assert_eq!(ids.len(), 1, "one request, one id");
    assert!(!ids[0].is_empty(), "id is non-empty");
    assert_ne!(ids[0], "0", "id is not a hardcoded placeholder");
}

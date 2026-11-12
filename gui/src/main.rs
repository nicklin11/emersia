//! Emersia companion app (M4, ADR 0004/0005).
//!
//! A client of the daemon's local control socket — exactly like the CLI, so
//! the same SO_PEERCRED same-user rule applies and there is no network
//! surface. The tray icon is the visible consent indicator: it turns red with
//! a tooltip naming the output and headset count whenever the screen is
//! shared.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use emersia_protocol::{
    decode_response, encode_request, Command, PairAction, PairArgs, Request, Response, RevokeArgs,
    StartArgs, PROTOCOL_VERSION,
};
use serde_json::Value;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One request/response over the control socket.
fn call(command: Command) -> Result<Value, String> {
    let socket =
        emersia_protocol::default_socket_path().map_err(|e| format!("control socket path: {e}"))?;
    let mut stream = UnixStream::connect(&socket).map_err(|e| {
        format!(
            "emersia-daemon is not running ({e}). Start it: systemctl --user start emersia-daemon"
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let request = Request {
        v: PROTOCOL_VERSION,
        id: format!("gui-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)),
        command,
    };
    let line = encode_request(&request).map_err(|e| e.to_string())?;
    stream
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    match decode_response(reply.trim_end()).map_err(|e| e.to_string())? {
        Response::Ok(ok) => Ok(ok.ok),
        Response::Err(err) => Err(err.error.message),
    }
}

fn pair(action: PairAction, device: Option<String>) -> Result<Value, String> {
    call(Command::Pair(PairArgs {
        action,
        code: None,
        name: None,
        public_key: None,
        device,
    }))
}

#[tauri::command]
async fn status() -> Result<Value, String> {
    call(Command::Status)
}

#[tauri::command]
async fn devices() -> Result<Value, String> {
    call(Command::Devices)
}

#[tauri::command]
async fn screens() -> Result<Value, String> {
    call(Command::Screens)
}

#[tauri::command]
async fn revoke(device: String) -> Result<Value, String> {
    call(Command::Revoke(RevokeArgs { device }))
}

#[tauri::command]
async fn start(outputs: Vec<String>, fps: Option<u32>) -> Result<Value, String> {
    let mut outputs = outputs.into_iter();
    call(Command::Start(StartArgs {
        output: outputs.next(),
        fps,
        also: outputs.collect(),
    }))
}

#[tauri::command]
async fn stop() -> Result<Value, String> {
    call(Command::Stop)
}

#[tauri::command]
async fn pair_open() -> Result<Value, String> {
    pair(PairAction::Open, None)
}

#[tauri::command]
async fn pair_pending() -> Result<Value, String> {
    pair(PairAction::Pending, None)
}

#[tauri::command]
async fn pair_decide(device: String, accept: bool) -> Result<Value, String> {
    let action = if accept {
        PairAction::Confirm
    } else {
        PairAction::Reject
    };
    let result = pair(action, Some(device));
    let _ = pair(PairAction::Close, None);
    result
}

#[tauri::command]
async fn pair_close() -> Result<Value, String> {
    pair(PairAction::Close, None)
}

/// Tray tooltip and whether the screen is being shared, from a status reply.
fn sharing_summary(status: &Value) -> (bool, String) {
    let streaming = status["streaming"].as_bool().unwrap_or(false);
    let headsets = status["connected_devices"].as_u64().unwrap_or(0);
    let outputs: Vec<&str> = status["streams"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if streaming && headsets > 0 {
        let what = if outputs.is_empty() {
            "your screen".to_string()
        } else {
            outputs.join(" + ")
        };
        let who = if headsets == 1 {
            "1 headset".to_string()
        } else {
            format!("{headsets} headsets")
        };
        (true, format!("Emersia — sharing {what} with {who}"))
    } else if streaming {
        (false, "Emersia — ready, no headset connected".into())
    } else {
        (false, "Emersia — not sharing".into())
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            status,
            devices,
            screens,
            revoke,
            start,
            stop,
            pair_open,
            pair_pending,
            pair_decide,
            pair_close
        ])
        .setup(|app| {
            let idle = Image::from_bytes(include_bytes!("../icons/tray-idle.png"))?;
            let sharing = Image::from_bytes(include_bytes!("../icons/tray-sharing.png"))?;
            let open = MenuItem::with_id(app, "open", "Open Emersia", true, None::<&str>)?;
            let stop = MenuItem::with_id(app, "stop", "Stop sharing", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &stop, &quit])?;
            let tray = TrayIconBuilder::with_id("emersia")
                .icon(idle.clone())
                .tooltip("Emersia")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => show_main(app),
                    "stop" => {
                        let _ = call(Command::Stop);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            // Keep the tray truthful: poll the daemon and flip icon/tooltip.
            std::thread::spawn(move || {
                let mut last: Option<(bool, String)> = None;
                loop {
                    let summary = match call(Command::Status) {
                        Ok(s) => sharing_summary(&s),
                        Err(_) => (false, "Emersia — daemon not running".into()),
                    };
                    if last.as_ref() != Some(&summary) {
                        let _ = tray.set_icon(Some(if summary.0 {
                            sharing.clone()
                        } else {
                            idle.clone()
                        }));
                        let _ = tray.set_tooltip(Some(&summary.1));
                        last = Some(summary);
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the tray (and the indicator) alive.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Emersia");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tray_summary_says_what_is_shared_with_whom() {
        let s = json!({"streaming": true, "connected_devices": 1, "streams": ["DP-3", "HDMI-A-1"]});
        assert_eq!(
            sharing_summary(&s),
            (
                true,
                "Emersia — sharing DP-3 + HDMI-A-1 with 1 headset".into()
            )
        );
        let idle = json!({"streaming": true, "connected_devices": 0, "streams": ["DP-3"]});
        assert!(!sharing_summary(&idle).0);
        let off = json!({"streaming": false, "connected_devices": 0});
        assert_eq!(sharing_summary(&off).1, "Emersia — not sharing");
    }
}

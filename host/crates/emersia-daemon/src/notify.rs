//! Visible consent (ADR 0005 §5): tell the desktop user when a headset starts
//! or stops viewing the screen, through the freedesktop notification service
//! (`notify-send`). Best-effort and non-blocking: no notification server, or
//! no `notify-send`, just means the event is logged.

use std::process::{Command, Stdio};

/// What happened to a device's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Connected,
    Disconnected,
}

/// Title and body for an event.
pub fn message(event: Event, device: &str) -> (String, String) {
    match event {
        Event::Connected => (
            format!("{device} is viewing your screen"),
            "Emersia is streaming your desktop to this headset. \
             Stop: emersia stop · revoke: emersia revoke <id>"
                .to_string(),
        ),
        Event::Disconnected => (
            format!("{device} stopped viewing your screen"),
            "The headset disconnected.".to_string(),
        ),
    }
}

/// Raise the notification in the background.
pub fn send(event: Event, device: &str) {
    let (title, body) = message(event, device);
    eprintln!("emersia-daemon: {title}");
    let urgency = match event {
        Event::Connected => "normal",
        Event::Disconnected => "low",
    };
    let spawned = Command::new("notify-send")
        .args(["--app-name=Emersia", "--icon=video-display", "-u", urgency])
        .arg(&title)
        .arg(&body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        // Reap it off the network thread.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_device_and_how_to_stop() {
        let (title, body) = message(Event::Connected, "Quest 2");
        assert_eq!(title, "Quest 2 is viewing your screen");
        assert!(body.contains("emersia stop"));
        let (title, _) = message(Event::Disconnected, "Quest 2");
        assert!(title.contains("stopped"));
    }
}

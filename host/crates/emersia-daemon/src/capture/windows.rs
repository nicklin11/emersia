//! Single-window capture targets (M4 "capture target picker").
//!
//! `ext-foreign-toplevel-list` enumerates windows; with
//! `ext-foreign-toplevel-image-capture-source` a window becomes an
//! `ext-image-copy-capture` source like an output. sway ≥ 1.12 (wlroots
//! 0.20) has both; niri lists windows but cannot capture them this way.
//!
//! Targets are named `window:<identifier>`; the identifier is the
//! compositor's stable id for that window.

use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};

use super::State;

/// Prefix that marks a capture target as a window rather than an output.
pub const WINDOW_PREFIX: &str = "window:";

/// One open window.
#[derive(Debug, Clone)]
pub struct Toplevel {
    pub handle: ExtForeignToplevelHandleV1,
    pub identifier: String,
    pub title: String,
    pub app_id: String,
}

impl Toplevel {
    /// The target name used by `start` (`window:<identifier>`).
    pub fn target(&self) -> String {
        format!("{WINDOW_PREFIX}{}", self.identifier)
    }
}

/// The window id named by a `window:<id>` target, if it is one.
pub fn window_id(target: &str) -> Option<&str> {
    target.strip_prefix(WINDOW_PREFIX)
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(Toplevel {
                handle: toplevel,
                identifier: String::new(),
                title: String::new(),
                app_id: String::new(),
            });
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        if let Event::Closed = event {
            state.toplevels.retain(|t| t.handle.id() != handle.id());
            handle.destroy();
            return;
        }
        let Some(t) = state
            .toplevels
            .iter_mut()
            .find(|t| t.handle.id() == handle.id())
        else {
            return;
        };
        match event {
            Event::Identifier { identifier } => t.identifier = identifier,
            Event::Title { title } => t.title = title,
            Event::AppId { app_id } => t.app_id = app_id,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_targets_are_prefixed() {
        assert_eq!(window_id("window:abc"), Some("abc"));
        assert_eq!(window_id("DP-3"), None);
        assert_eq!(window_id("window:"), Some(""));
    }
}

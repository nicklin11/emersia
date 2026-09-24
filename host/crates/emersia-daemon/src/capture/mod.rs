//! Wayland capture plumbing shared by both backends (ADR 0002):
//! `ext-image-copy-capture` first, `wlr-screencopy` as the fallback.
//!
//! Everything runs over a plain `$WAYLAND_DISPLAY` connection — no desktop
//! portal or DE session is required. M1.1's contract is deliberately tiny:
//! connect, detect outputs, grab exactly one frame, then exit.

pub mod ext;
pub mod wlr;

use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};

use crate::frame::Frame;
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_output::{self, WlOutput},
    wl_registry::{self, WlRegistry},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

/// How long to wait for the compositor to publish capture constraints.
pub const CONSTRAINT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for a frame copy after requesting it.
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// Timings for the measurable phases of one successful capture.
///
/// `frame_wait` includes both compositor scheduling and the copy itself: the
/// Wayland protocol does not expose a separate timestamp at which a copy
/// started. The other fields isolate userspace readback, pixel decoding, and
/// transform work. `total` is measured around the complete capture operation
/// and is intentionally not a sum of the phase fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureTimings {
    pub constraints: Duration,
    pub frame_wait: Duration,
    pub shm_read: Duration,
    pub decode: Duration,
    pub transform: Duration,
    pub total: Duration,
}

/// A decoded frame plus the timing evidence from the same capture operation.
#[derive(Debug)]
pub struct CapturedFrame {
    pub frame: Frame,
    pub timings: CaptureTimings,
}

/// A capture backend offered by the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Ext,
    Wlr,
}

impl Backend {
    /// The protocol's name, for user-facing messages.
    pub fn protocol(self) -> &'static str {
        match self {
            Backend::Ext => "ext-image-copy-capture",
            Backend::Wlr => "wlr-screencopy",
        }
    }
}

/// The backend requested on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendPref {
    Auto,
    Ext,
    Wlr,
}

/// Pick the backend: `ext-image-copy-capture` is preferred (ADR 0002),
/// `wlr-screencopy` is the fallback for compositors without it.
pub fn choose_backend(pref: BackendPref, has_ext: bool, has_wlr: bool) -> anyhow::Result<Backend> {
    match pref {
        BackendPref::Ext if has_ext => Ok(Backend::Ext),
        BackendPref::Ext => bail!(
            "--backend ext requested but the compositor does not advertise \
             ext-image-copy-capture"
        ),
        BackendPref::Wlr if has_wlr => Ok(Backend::Wlr),
        BackendPref::Wlr => {
            bail!("--backend wlr requested but the compositor does not advertise wlr-screencopy")
        }
        BackendPref::Auto if has_ext => Ok(Backend::Ext),
        BackendPref::Auto if has_wlr => Ok(Backend::Wlr),
        BackendPref::Auto => {
            bail!("the compositor advertises neither ext-image-copy-capture nor wlr-screencopy")
        }
    }
}

/// Resolve `--output NAME` against the detected outputs (no name = first).
pub fn select_output(labels: &[String], want: Option<&str>) -> anyhow::Result<usize> {
    if labels.is_empty() {
        bail!("the compositor advertised no wl_output monitors");
    }
    match want {
        None => Ok(0),
        Some(name) => labels
            .iter()
            .position(|l| l == name)
            .with_context(|| format!("no output named {name:?}; available: {}", labels.join(", "))),
    }
}

/// Choose a shared-memory format the capture session accepts, preferring
/// opaque XRGB (universally available) over formats carrying alpha.
pub fn pick_shm_format(formats: &[wl_shm::Format]) -> anyhow::Result<wl_shm::Format> {
    for preferred in [
        wl_shm::Format::Xrgb8888,
        wl_shm::Format::Argb8888,
        wl_shm::Format::Xbgr8888,
        wl_shm::Format::Rgba8888,
    ] {
        if formats.contains(&preferred) {
            return Ok(preferred);
        }
    }
    bail!("the compositor offered no supported shm format (got: {formats:?})");
}

/// What we know about one `wl_output` after startup roundtrips.
#[derive(Debug)]
pub struct OutputInfo {
    pub global_name: u32,
    pub proxy: WlOutput,
    pub name: Option<String>,
    pub make: String,
    pub model: String,
    pub width: u32,
    pub height: u32,
    pub scale: i32,
    pub transform: wl_output::Transform,
}

impl OutputInfo {
    fn new(global_name: u32, proxy: WlOutput) -> Self {
        Self {
            global_name,
            proxy,
            name: None,
            make: String::new(),
            model: String::new(),
            width: 0,
            height: 0,
            scale: 1,
            transform: wl_output::Transform::Normal,
        }
    }

    /// Name used for `--output` matching: `wl_output.name` when the
    /// compositor provides it (v4+), otherwise a positional fallback.
    pub fn label(&self, index: usize) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("output-{index}"))
    }
}

/// Events received for the in-flight `ext-image-copy-capture` session/frame.
#[derive(Debug, Default)]
pub struct ExtState {
    pub buffer_width: u32,
    pub buffer_height: u32,
    pub shm_formats: Vec<wl_shm::Format>,
    pub constraints_done: bool,
    pub stopped: bool,
    pub transform: Option<wl_output::Transform>,
    pub frame_ready: bool,
    pub failure: Option<String>,
}

impl ExtState {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Events received for the in-flight `wlr-screencopy` frame.
#[derive(Debug, Default)]
pub struct WlrState {
    pub have_buffer: bool,
    pub buffer_done: bool,
    pub dmabuf_only: bool,
    pub buffer_format: Option<wl_shm::Format>,
    pub buffer_width: u32,
    pub buffer_height: u32,
    pub buffer_stride: u32,
    pub y_invert: bool,
    pub ready: bool,
    pub failed: bool,
}

impl WlrState {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Per-connection capture state, updated by the `Dispatch` implementations.
pub struct State {
    pub shm: Option<WlShm>,
    pub ext_source_mgr: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub ext_copy_mgr: Option<ExtImageCopyCaptureManagerV1>,
    pub wlr_mgr: Option<ZwlrScreencopyManagerV1>,
    pub outputs: Vec<OutputInfo>,
    pub ext: ExtState,
    pub wlr: WlrState,
}

impl State {
    pub fn new() -> Self {
        Self {
            shm: None,
            ext_source_mgr: None,
            ext_copy_mgr: None,
            wlr_mgr: None,
            outputs: Vec::new(),
            ext: ExtState::default(),
            wlr: WlrState::default(),
        }
    }

    /// Display names of every detected output, index-aligned with `outputs`.
    pub fn output_labels(&self) -> Vec<String> {
        self.outputs
            .iter()
            .enumerate()
            .map(|(i, o)| o.label(i))
            .collect()
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// Dispatch Wayland events until `ready` holds or `deadline` passes.
///
/// `poll()`s the connection socket so a wedged compositor surfaces as a clean
/// error instead of a hung daemon.
pub fn wait_until(
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
    what: &str,
    ready: impl Fn(&State) -> bool,
) -> anyhow::Result<()> {
    loop {
        queue.dispatch_pending(state)?;
        if ready(state) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {what}");
        }
        // Send whatever requests the previous dispatch produced (binds,
        // create_session, capture, ...), then wait for the socket.
        queue.flush()?;
        if let Some(guard) = queue.prepare_read() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout =
                Timespec::try_from(remaining).context("capture timeout is out of range")?;
            let poll_result = {
                let fd = guard.connection_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                poll(&mut fds, Some(&timeout))
            };
            match poll_result {
                Ok(0) => bail!("timed out waiting for {what}"),
                Ok(_) => {}
                // Interrupted: cancel this read and poll again.
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e).context("failed to poll the Wayland socket"),
            }
            guard.read()?;
        }
        // Otherwise events were already queued; loop back and dispatch them.
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(3), qh, ())),
                "wl_output" => {
                    // v4 adds the `name` event we use for `--output`.
                    let proxy = registry.bind::<WlOutput, u32, _>(name, version.min(4), qh, name);
                    state.outputs.push(OutputInfo::new(name, proxy));
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.ext_source_mgr = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.ext_copy_mgr = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_screencopy_manager_v1" => {
                    state.wlr_mgr = Some(registry.bind(name, version.min(3), qh, ()));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|output| output.global_name != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _output: &WlOutput,
        event: wl_output::Event,
        global_name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(info) = state
            .outputs
            .iter_mut()
            .find(|o| o.global_name == *global_name)
        else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                make,
                model,
                transform,
                ..
            } => {
                info.make = make;
                info.model = model;
                info.transform = match transform {
                    WEnum::Value(t) => t,
                    _ => wl_output::Transform::Normal,
                };
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } => {
                if flags.contains(wl_output::Mode::Current) {
                    info.width = u32::try_from(width).unwrap_or(0);
                    info.height = u32::try_from(height).unwrap_or(0);
                }
            }
            wl_output::Event::Scale { factor } => info.scale = factor,
            wl_output::Event::Name { name } => info.name = Some(name),
            // `description` and `done` carry nothing we display in M1.1.
            _ => {}
        }
    }
}

// Interfaces we bind but whose events we never need. `ignore` keeps the bodies
// empty, which also suits the interfaces that have no events at all.
wayland_client::delegate_noop!(State: ignore WlShm);
wayland_client::delegate_noop!(State: ignore WlShmPool);
wayland_client::delegate_noop!(State: ignore WlBuffer);
wayland_client::delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(State: ignore ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
wayland_client::delegate_noop!(State: ignore ZwlrScreencopyManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn auto_prefers_ext_then_wlr() {
        assert_eq!(
            choose_backend(BackendPref::Auto, true, true).unwrap(),
            Backend::Ext
        );
        assert_eq!(
            choose_backend(BackendPref::Auto, false, true).unwrap(),
            Backend::Wlr
        );
        assert!(choose_backend(BackendPref::Auto, false, false).is_err());
    }

    #[test]
    fn explicit_backend_must_be_advertised() {
        assert_eq!(
            choose_backend(BackendPref::Ext, true, false).unwrap(),
            Backend::Ext
        );
        assert_eq!(
            choose_backend(BackendPref::Wlr, false, true).unwrap(),
            Backend::Wlr
        );
        assert!(choose_backend(BackendPref::Ext, false, true).is_err());
        assert!(choose_backend(BackendPref::Wlr, true, false).is_err());
    }

    #[test]
    fn selects_first_output_by_default() {
        let list = labels(&["DP-3", "HDMI-A-1"]);
        assert_eq!(select_output(&list, None).unwrap(), 0);
    }

    #[test]
    fn selects_by_name_or_reports_alternatives() {
        let list = labels(&["DP-3", "HDMI-A-1"]);
        assert_eq!(select_output(&list, Some("HDMI-A-1")).unwrap(), 1);
        let err = select_output(&list, Some("eDP-9")).unwrap_err();
        assert!(format!("{err:#}").contains("DP-3"), "lists alternatives");
    }

    #[test]
    fn rejects_a_compositor_with_no_outputs() {
        assert!(select_output(&[], None).is_err());
    }

    #[test]
    fn shm_format_prefers_opaque_xrgb() {
        use wl_shm::Format as F;
        let offered = [F::Argb8888, F::Xrgb8888, F::Rgb565];
        assert_eq!(pick_shm_format(&offered).unwrap(), F::Xrgb8888);
        assert_eq!(pick_shm_format(&[F::Rgba8888]).unwrap(), F::Rgba8888);
        assert!(pick_shm_format(&[F::Rgb565]).is_err());
        assert!(pick_shm_format(&[]).is_err());
    }
}

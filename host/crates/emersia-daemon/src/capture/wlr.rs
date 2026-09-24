//! `wlr-screencopy` backend — the legacy wlroots API used as a fallback when
//! a compositor does not implement `ext-image-copy-capture` (ADR 0002).
//!
//! niri may not advertise this protocol at all; the path is covered by unit
//! tests and by `--backend wlr` whenever the compositor does offer it.

use std::time::Instant;

use anyhow::{bail, Context as _};
use wayland_client::protocol::{wl_output, wl_output::WlOutput};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    Event as FrameEvent, Flags, ZwlrScreencopyFrameV1,
};

use crate::frame::{decode, flip_rows, untransform, Frame};
use crate::shm::ShmBuffer;

use super::{wait_until, CaptureTimings, CapturedFrame, State, CONSTRAINT_TIMEOUT, FRAME_TIMEOUT};

/// Capture one frame from `output` with `wlr-screencopy`.
///
/// `transform` is the output's `wl_output.geometry` transform — unlike ext,
/// this protocol reports no per-frame transform of its own.
pub fn capture(
    queue: &mut EventQueue<State>,
    state: &mut State,
    output: &WlOutput,
    transform: wl_output::Transform,
) -> anyhow::Result<CapturedFrame> {
    let total_started = Instant::now();
    let qh = queue.handle();
    state.wlr.reset();

    let mgr = state
        .wlr_mgr
        .clone()
        .context("the compositor does not advertise zwlr_screencopy_manager_v1")?;
    let shm = state
        .shm
        .clone()
        .context("the compositor does not advertise wl_shm")?;

    let frame = mgr.capture_output(0, output, &qh, ());
    // v3 adds the explicit `buffer_done` terminator; older versions signal the
    // buffer description with `buffer` alone.
    let needs_buffer_done = frame.version() >= 3;

    let constraints_started = Instant::now();
    wait_until(
        queue,
        state,
        Instant::now() + CONSTRAINT_TIMEOUT,
        "screencopy buffer description",
        |s| {
            s.wlr.failed
                || (s.wlr.have_buffer && (!needs_buffer_done || s.wlr.buffer_done))
                || s.wlr.dmabuf_only
        },
    )?;
    let constraints = constraints_started.elapsed();
    if state.wlr.failed {
        bail!("the compositor failed the screencopy request");
    }
    if state.wlr.dmabuf_only && !state.wlr.have_buffer {
        bail!("the compositor only offers dmabuf screencopy buffers (unsupported in M1.1)");
    }

    let format = state
        .wlr
        .buffer_format
        .context("the compositor sent no usable buffer format")?;
    let width = state.wlr.buffer_width;
    let height = state.wlr.buffer_height;
    let stride = state.wlr.buffer_stride;
    let mut shm_buffer = ShmBuffer::create(&shm, &qh, width, height, stride, format)?;

    frame.copy(shm_buffer.buffer());
    let frame_wait_started = Instant::now();
    wait_until(
        queue,
        state,
        Instant::now() + FRAME_TIMEOUT,
        "screencopy frame",
        |s| s.wlr.ready || s.wlr.failed,
    )?;
    let frame_wait = frame_wait_started.elapsed();

    let y_invert = state.wlr.y_invert;
    frame.destroy();
    if !state.wlr.ready {
        bail!("the compositor failed the screencopy");
    }

    let shm_read_started = Instant::now();
    let data = shm_buffer.read_pixels(stride, height)?;
    let shm_read = shm_read_started.elapsed();
    let decode_started = Instant::now();
    let mut rgba = decode(format, width, height, stride, &data)?;
    let decode = decode_started.elapsed();
    let transform_started = Instant::now();
    if y_invert {
        rgba = flip_rows(&rgba, width, height);
    }
    let (rgba, out_w, out_h) = untransform(&rgba, width, height, transform);
    let transform = transform_started.elapsed();
    Ok(CapturedFrame {
        frame: Frame::new(out_w, out_h, rgba),
        timings: CaptureTimings {
            constraints,
            frame_wait,
            shm_read,
            decode,
            transform,
            total: total_started.elapsed(),
        },
    })
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrScreencopyFrameV1,
        event: FrameEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            FrameEvent::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                state.wlr.buffer_format = match format {
                    WEnum::Value(f) => Some(f),
                    _ => None,
                };
                state.wlr.buffer_width = width;
                state.wlr.buffer_height = height;
                state.wlr.buffer_stride = stride;
                state.wlr.have_buffer = state.wlr.buffer_format.is_some();
            }
            FrameEvent::Flags {
                flags: WEnum::Value(f),
            } => state.wlr.y_invert = f.contains(Flags::YInvert),
            FrameEvent::Ready { .. } => state.wlr.ready = true,
            FrameEvent::Failed => state.wlr.failed = true,
            FrameEvent::BufferDone => state.wlr.buffer_done = true,
            FrameEvent::LinuxDmabuf { .. } => state.wlr.dmabuf_only = true,
            // `damage` is informational for a one-shot grab.
            _ => {}
        }
    }
}

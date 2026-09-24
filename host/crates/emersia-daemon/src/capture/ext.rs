//! `ext-image-copy-capture` backend — the staging protocol niri implements,
//! and the primary path per ADR 0002.

use std::time::Instant;

use anyhow::{bail, Context as _};
use wayland_client::protocol::{wl_output, wl_output::WlOutput};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{
        Event as FrameEvent, ExtImageCopyCaptureFrameV1, FailureReason,
    },
    ext_image_copy_capture_manager_v1::Options,
    ext_image_copy_capture_session_v1::{Event as SessionEvent, ExtImageCopyCaptureSessionV1},
};

use crate::frame::{decode, untransform, Frame};
use crate::shm::ShmBuffer;

use super::{
    pick_shm_format, wait_until, CaptureTimings, CapturedFrame, State, CONSTRAINT_TIMEOUT,
    FRAME_TIMEOUT,
};

/// Capture one frame from `output` with `ext-image-copy-capture`.
///
/// Flow: create source/session → wait for the compositor's buffer constraints
/// → allocate a matching shm buffer → `capture()` → wait for `ready` → read
/// the pixels back and undo the frame transform.
pub fn capture(
    queue: &mut EventQueue<State>,
    state: &mut State,
    output: &WlOutput,
) -> anyhow::Result<CapturedFrame> {
    let total_started = Instant::now();
    let qh = queue.handle();
    state.ext.reset();

    let source_mgr = state
        .ext_source_mgr
        .clone()
        .context("the compositor does not advertise ext_output_image_capture_source_manager_v1")?;
    let copy_mgr = state
        .ext_copy_mgr
        .clone()
        .context("the compositor does not advertise ext_image_copy_capture_manager_v1")?;
    let shm = state
        .shm
        .clone()
        .context("the compositor does not advertise wl_shm")?;

    let source = source_mgr.create_source(output, &qh, ());
    let session = copy_mgr.create_session(&source, Options::empty(), &qh, ());

    // 1. The compositor tells us what buffer it will accept.
    let constraints_started = Instant::now();
    wait_until(
        queue,
        state,
        Instant::now() + CONSTRAINT_TIMEOUT,
        "capture constraints",
        |s| s.ext.constraints_done || s.ext.stopped,
    )?;
    let constraints = constraints_started.elapsed();
    if state.ext.stopped {
        bail!("the compositor stopped the capture session before publishing constraints");
    }

    let format = pick_shm_format(&state.ext.shm_formats)?;
    let width = state.ext.buffer_width;
    let height = state.ext.buffer_height;
    let stride = width.checked_mul(4).context("capture stride overflows")?;
    let mut shm_buffer = ShmBuffer::create(&shm, &qh, width, height, stride, format)?;

    // 2. Ask for exactly one full-frame copy.
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(shm_buffer.buffer());
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();

    let frame_wait_started = Instant::now();
    wait_until(
        queue,
        state,
        Instant::now() + FRAME_TIMEOUT,
        "frame copy",
        |s| s.ext.frame_ready || s.ext.failure.is_some() || s.ext.stopped,
    )?;
    let frame_wait = frame_wait_started.elapsed();

    let failure = state.ext.failure.clone();
    let transform = state.ext.transform.unwrap_or(wl_output::Transform::Normal);
    frame.destroy();
    session.destroy();
    source.destroy();

    if let Some(reason) = failure {
        bail!("the compositor failed the frame capture: {reason}");
    }
    if !state.ext.frame_ready {
        bail!("the capture session stopped before a frame was produced");
    }

    // 3. Read back and convert to an upright RGBA image.
    let shm_read_started = Instant::now();
    let data = shm_buffer.read_pixels(stride, height)?;
    let shm_read = shm_read_started.elapsed();
    let decode_started = Instant::now();
    let rgba = decode(format, width, height, stride, &data)?;
    let decode = decode_started.elapsed();
    let transform_started = Instant::now();
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

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: SessionEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            SessionEvent::BufferSize { width, height } => {
                state.ext.buffer_width = width;
                state.ext.buffer_height = height;
            }
            SessionEvent::ShmFormat {
                format: WEnum::Value(f),
            } => state.ext.shm_formats.push(f),
            SessionEvent::Done => state.ext.constraints_done = true,
            SessionEvent::Stopped => state.ext.stopped = true,
            // dmabuf_device / dmabuf_format: dmabuf is out of scope in M1.1.
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: FrameEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            FrameEvent::Transform { transform } => {
                state.ext.transform = Some(match transform {
                    WEnum::Value(t) => t,
                    _ => wl_output::Transform::Normal,
                });
            }
            FrameEvent::Ready => state.ext.frame_ready = true,
            FrameEvent::Failed { reason } => {
                state.ext.failure = Some(describe_failure(reason));
            }
            // damage / presentation_time are informational for a one-shot grab.
            _ => {}
        }
    }
}

/// Turn a protocol failure reason into a human-readable message.
fn describe_failure(reason: WEnum<FailureReason>) -> String {
    match reason {
        WEnum::Value(FailureReason::BufferConstraints) => {
            "buffer constraints could not be satisfied".to_string()
        }
        WEnum::Value(FailureReason::Stopped) => "capture session stopped".to_string(),
        WEnum::Value(other) => format!("failure reason {other:?}"),
        WEnum::Unknown(v) => format!("unknown failure reason {v}"),
    }
}

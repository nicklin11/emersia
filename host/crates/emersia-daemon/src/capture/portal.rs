//! `xdg-desktop-portal` ScreenCast + PipeWire backend (M5).
//!
//! The compositor-neutral path: GNOME, KDE and any desktop whose portal
//! implements ScreenCast. The user picks what to share in the portal's own
//! dialog; the restore token it returns is saved, so later sessions start
//! without asking again until the user revokes it.
//!
//! The portal handshake and the PipeWire main loop run on a worker thread.
//! The engine only waits — with a timeout — for the newest frame, so a
//! dialog nobody has answered yet is a status message, not a hung daemon.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use pipewire as pw;
use pw::spa;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::frame::{Frame, PixelFormat};

use super::{CaptureTimings, CapturedFrame};

const DESKTOP: &str = "org.freedesktop.portal.Desktop";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";

/// Newest frame from the PipeWire thread, plus how the session is doing.
#[derive(Default)]
struct Shared {
    frame: Option<RawFrame>,
    /// Bumped for every frame, so a waiter can tell new from seen.
    seq: u64,
    /// Fatal error from the worker (portal refused, stream failed).
    error: Option<String>,
    /// The portal session is up and PipeWire is connected.
    streaming: bool,
    /// Set by `Drop`: the worker should quit.
    stop: bool,
    /// Where the shared monitor sits in the desktop, when the portal says.
    geometry: Option<super::Rect>,
}

struct RawFrame {
    width: u32,
    height: u32,
    format: PixelFormat,
    pixels: Vec<u8>,
}

/// A running portal screencast.
pub struct PortalCapture {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
    seen: u64,
}

impl PortalCapture {
    /// Start a session in the background. `token_file` keeps the portal's
    /// restore token between runs; `cursor` asks for the pointer to be
    /// embedded in the frames.
    pub fn start(token_file: PathBuf, cursor: bool) -> Self {
        let shared: Arc<(Mutex<Shared>, Condvar)> = Arc::default();
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("emersia-portal".into())
            .spawn(move || {
                if let Err(e) = run(&worker_shared, &token_file, cursor) {
                    let (lock, cv) = &*worker_shared;
                    let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
                    if !s.stop {
                        s.error = Some(format!("{e:#}"));
                    }
                    cv.notify_all();
                }
            })
            .ok();
        Self {
            shared,
            worker,
            seen: 0,
        }
    }

    /// The shared monitor's position and size in logical coordinates, as
    /// reported by the portal (absent for windows or until it answers).
    pub fn geometry(&self) -> Option<super::Rect> {
        self.shared.0.lock().ok()?.geometry
    }

    /// Wait up to `timeout` for a frame newer than the last one returned.
    pub fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<CapturedFrame> {
        let started = Instant::now();
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = started + timeout;
        loop {
            if let Some(e) = &s.error {
                bail!("screen-share portal: {e}");
            }
            let now = Instant::now();
            // A static screen sends nothing: repeat the last frame once the
            // wait runs out, as a keepalive, rather than report an error.
            let repeat = now >= deadline && s.streaming;
            if s.seq != self.seen || repeat {
                if let Some(f) = &s.frame {
                    self.seen = s.seq;
                    let frame_wait = started.elapsed();
                    let copy_started = Instant::now();
                    let frame = Frame::new(f.width, f.height, f.pixels.clone(), f.format);
                    let shm_read = copy_started.elapsed();
                    return Ok(CapturedFrame {
                        frame,
                        timings: CaptureTimings {
                            frame_wait,
                            shm_read,
                            total: started.elapsed(),
                            ..CaptureTimings::default()
                        },
                    });
                }
            }
            if now >= deadline {
                if s.streaming {
                    bail!("screen-share portal: no new frame (nothing changed on screen)");
                }
                bail!("waiting for screen-share permission (answer the portal's dialog)");
            }
            s = cv
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.shared;
            lock.lock().unwrap_or_else(|p| p.into_inner()).stop = true;
            cv.notify_all();
        }
        if let Some(w) = self.worker.take() {
            // The worker polls `stop` every 100 ms; a dialog still open is
            // abandoned (the D-Bus connection closes with the thread).
            let _ = w.join();
        }
    }
}

/// Portal handshake, then the PipeWire loop until `stop`.
fn run(
    shared: &Arc<(Mutex<Shared>, Condvar)>,
    token_file: &PathBuf,
    cursor: bool,
) -> anyhow::Result<()> {
    let conn = zbus::blocking::Connection::session().context("no D-Bus session bus")?;
    let restore = std::fs::read_to_string(token_file)
        .ok()
        .map(|t| t.trim().to_owned());
    let (node, fd, token) =
        open_screencast(&conn, restore.filter(|t| !t.is_empty()), cursor, shared)?;
    if let Some(token) = token {
        if let Some(dir) = token_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(token_file, token) {
            eprintln!("emersia-daemon: could not save the portal restore token: {e}");
        }
    }
    stream_frames(shared, node, fd)
}

fn stopped(shared: &Arc<(Mutex<Shared>, Condvar)>) -> bool {
    shared.0.lock().map(|s| s.stop).unwrap_or(true)
}

/// CreateSession → SelectSources → Start → OpenPipeWireRemote.
/// Returns the PipeWire node, the remote's fd and a new restore token.
fn open_screencast(
    conn: &zbus::blocking::Connection,
    restore: Option<String>,
    cursor: bool,
    shared: &Arc<(Mutex<Shared>, Condvar)>,
) -> anyhow::Result<(u32, OwnedFd, Option<String>)> {
    let sender = conn
        .unique_name()
        .context("no unique bus name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let mut counter = 0u32;
    let mut request = |method: &str,
                       body: &dyn Fn(&str) -> zbus::Result<zbus::message::Message>|
     -> anyhow::Result<HashMap<String, OwnedValue>> {
        counter += 1;
        let token = format!("emersia{}_{counter}", std::process::id());
        let path = format!("{DESKTOP_PATH}/request/{sender}/{token}");
        // Subscribe before calling, or a fast Response is missed.
        let proxy = zbus::blocking::Proxy::new(
            conn,
            DESKTOP,
            path.as_str(),
            "org.freedesktop.portal.Request",
        )?;
        let mut responses = proxy.receive_signal("Response")?;
        body(&token).with_context(|| format!("ScreenCast.{method}"))?;
        // The Response may wait on a person; poll `stop` meanwhile by
        // reading on a helper thread.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(responses.next());
        });
        let msg = loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(msg)) => break msg,
                Ok(None) => bail!("portal connection closed during {method}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if stopped(shared) => {
                    bail!("cancelled")
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => bail!("portal {method}: {e}"),
            }
        };
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
        match code {
            0 => Ok(results),
            1 => bail!("the user declined to share the screen"),
            _ => bail!("the portal ended {method} (code {code})"),
        }
    };
    let res = request("CreateSession", &|token| {
        let opts: HashMap<&str, Value> = HashMap::from([
            ("handle_token", Value::from(token)),
            ("session_handle_token", Value::from(format!("{token}s"))),
        ]);
        conn.call_method(
            Some(DESKTOP),
            DESKTOP_PATH,
            Some(SCREENCAST),
            "CreateSession",
            &(opts,),
        )
    })?;
    let session: String = res
        .get("session_handle")
        .context("no session_handle")?
        .try_clone()?
        .try_into()?;
    let session = OwnedObjectPath::try_from(session)?;

    request("SelectSources", &|token| {
        let mut opts: HashMap<&str, Value> = HashMap::from([
            ("handle_token", Value::from(token)),
            ("types", Value::from(1u32 | 2u32)), // monitor | window
            ("multiple", Value::from(false)),
            ("cursor_mode", Value::from(if cursor { 2u32 } else { 1u32 })), // embedded | hidden
            ("persist_mode", Value::from(2u32)),                            // until revoked
        ]);
        if let Some(t) = &restore {
            opts.insert("restore_token", Value::from(t.as_str()));
        }
        conn.call_method(
            Some(DESKTOP),
            DESKTOP_PATH,
            Some(SCREENCAST),
            "SelectSources",
            &(&session, opts),
        )
    })?;

    let res = request("Start", &|token| {
        let opts: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token))]);
        conn.call_method(
            Some(DESKTOP),
            DESKTOP_PATH,
            Some(SCREENCAST),
            "Start",
            &(&session, "", opts),
        )
    })?;
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = res
        .get("streams")
        .context("the portal returned no streams")?
        .try_clone()?
        .try_into()?;
    let (node, props) = streams.first().context("the portal returned no streams")?;
    let node = *node;
    let pair =
        |key: &str| -> Option<(i32, i32)> { props.get(key)?.try_clone().ok()?.try_into().ok() };
    if let (Some((x, y)), Some((width, height))) = (pair("position"), pair("size")) {
        let (lock, _) = &**shared;
        if let Ok(mut s) = lock.lock() {
            s.geometry = Some(super::Rect {
                x,
                y,
                width,
                height,
            });
        }
    }
    let token: Option<String> = res
        .get("restore_token")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| v.try_into().ok());

    let opts: HashMap<&str, Value> = HashMap::new();
    let reply = conn.call_method(
        Some(DESKTOP),
        DESKTOP_PATH,
        Some(SCREENCAST),
        "OpenPipeWireRemote",
        &(&session, opts),
    )?;
    let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;
    Ok((node, fd.into(), token))
}

/// Map a negotiated SPA format onto our packed pixel formats.
fn pixel_format(f: spa::param::video::VideoFormat) -> Option<PixelFormat> {
    use spa::param::video::VideoFormat as V;
    Some(match f {
        V::BGRx => PixelFormat::Bgr0,
        V::RGBx => PixelFormat::Rgb0,
        V::BGRA => PixelFormat::Bgra,
        V::RGBA => PixelFormat::Rgba,
        _ => return None,
    })
}

/// The EnumFormat we accept: 32-bit packed RGB in any size and rate.
fn format_param() -> anyhow::Result<Vec<u8>> {
    use spa::param::format::{FormatProperties as P, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat as V;
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(P::MediaType, Id, MediaType::Video),
        spa::pod::property!(P::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            P::VideoFormat,
            Choice,
            Enum,
            Id,
            V::BGRx,
            V::BGRx,
            V::RGBx,
            V::BGRA,
            V::RGBA
        ),
        spa::pod::property!(
            P::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            P::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("format pod: {e:?}"))?
    .0
    .into_inner())
}

/// Connect to the portal's PipeWire remote and publish frames until `stop`.
fn stream_frames(
    shared: &Arc<(Mutex<Shared>, Condvar)>,
    node: u32,
    fd: OwnedFd,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(fd, None)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "emersia",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    struct Negotiated {
        info: spa::param::video::VideoInfoRaw,
        warned: bool,
    }
    let shared_cb = Arc::clone(shared);
    let shared_state = Arc::clone(shared);
    let _listener = stream
        .add_local_listener_with_user_data(Negotiated {
            info: Default::default(),
            warned: false,
        })
        .state_changed(move |_, _, _old, new| {
            let (lock, cv) = &*shared_state;
            let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
            eprintln!("emersia-daemon: portal stream {_old:?} -> {new:?}");
            match new {
                pw::stream::StreamState::Streaming => s.streaming = true,
                pw::stream::StreamState::Error(e) => {
                    s.error = Some(format!("PipeWire stream: {e}"))
                }
                pw::stream::StreamState::Unconnected if s.streaming => {
                    s.error = Some("the screencast ended (stopped from the desktop?)".into())
                }
                _ => {}
            }
            cv.notify_all();
        })
        .param_changed(|_, n, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let _ = n.info.parse(param);
        })
        .process(move |stream, n| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(format) = pixel_format(n.info.format()) else {
                if !n.warned {
                    n.warned = true;
                    eprintln!(
                        "emersia-daemon: portal negotiated an unsupported format {:?}",
                        n.info.format()
                    );
                }
                return;
            };
            let (w, h) = (n.info.size().width, n.info.size().height);
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let (offset, size, stride) = {
                let c = data.chunk();
                (c.offset() as usize, c.size() as usize, c.stride())
            };
            let kind = data.type_();
            let Some(bytes) = data.data() else {
                if !n.warned {
                    n.warned = true;
                    eprintln!("emersia-daemon: portal buffer is not mappable ({kind:?})");
                }
                return;
            };
            if std::env::var_os("EMERSIA_PORTAL_DEBUG").is_some() {
                eprintln!(
                    "portal frame {w}x{h} {:?} {kind:?} size={size} stride={stride}",
                    n.info.format()
                );
            }
            let row = w as usize * 4;
            let stride = if stride > 0 { stride as usize } else { row };
            let need = stride * (h as usize).saturating_sub(1) + row;
            if w == 0 || h == 0 || size < need || bytes.len() < offset + need {
                return; // empty (cursor-only) or short buffer
            }
            let src = &bytes[offset..offset + need];
            let mut pixels = Vec::with_capacity(row * h as usize);
            for y in 0..h as usize {
                pixels.extend_from_slice(&src[y * stride..y * stride + row]);
            }
            let (lock, cv) = &*shared_cb;
            let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
            s.frame = Some(RawFrame {
                width: w,
                height: h,
                format,
                pixels,
            });
            s.seq += 1;
            cv.notify_all();
        })
        .register()?;

    let values = format_param()?;
    let mut params = [spa::pod::Pod::from_bytes(&values).context("format pod")?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    // Iterate the loop in slices so `stop` is noticed promptly.
    let pw_loop = mainloop.loop_();
    while !stopped(shared) {
        pw_loop.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }
    Ok(())
}

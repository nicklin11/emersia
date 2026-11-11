//! JNI surface for `org.emersia.client.NativeBridge`.
//!
//! Deliberately tiny: Java hands over a Surface and connection settings, then
//! polls a status string. Everything per-frame stays native.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring, JNI_TRUE};
use jni::JNIEnv;
use ndk::native_window::NativeWindow;

use emersia_transport::crypto::parse_public_key_hex;
use emersia_transport::input::{Button, InputEvent};
use std::sync::mpsc::Sender;

use crate::decoder::SurfaceDecoder;
use crate::session::{self, Config, Stats};

struct Running {
    stop: Arc<AtomicBool>,
    input: Sender<InputEvent>,
    stats: Arc<Stats>,
    worker: Option<JoinHandle<()>>,
    server: String,
    sampler: crate::status::Sampler,
}

fn init_logging() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("emersia")
            .with_max_level(log::LevelFilter::Info),
    );
}

fn string(env: &mut JNIEnv, s: &JString) -> Option<String> {
    env.get_string(s).ok().map(Into::into)
}

fn jstr(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Raise the calling thread's scheduling priority. Best-effort: Android lets
/// apps go to -10 (URGENT_DISPLAY-ish) for their own threads.
fn boost_current_thread() {
    // SAFETY: plain syscalls on the calling thread.
    unsafe {
        let tid = libc::gettid();
        if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -10) != 0 {
            log::warn!("could not raise thread priority");
        }
    }
}

/// `String nativeIdentity(String filesDir)` → `"<pubkey> <device id>"` or
/// `"error: …"`.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeIdentity(
    mut env: JNIEnv,
    _class: JClass,
    files_dir: JString,
) -> jstring {
    init_logging();
    let Some(dir) = string(&mut env, &files_dir) else {
        return jstr(&mut env, "error: bad files dir");
    };
    let text = match crate::identity::load_or_create(&PathBuf::from(dir)) {
        Ok(identity) => crate::identity::describe(&identity),
        Err(e) => format!("error: {e}"),
    };
    jstr(&mut env, &text)
}

/// `long nativeStart(Surface surface, String filesDir, String server, String hostKey)`
/// → opaque handle, or 0 on invalid arguments (logged).
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    surface: JObject,
    files_dir: JString,
    server: JString,
    host_key: JString,
) -> jlong {
    init_logging();
    let (Some(dir), Some(server), Some(host_key)) = (
        string(&mut env, &files_dir),
        string(&mut env, &server),
        string(&mut env, &host_key),
    ) else {
        log::error!("nativeStart: missing arguments");
        return 0;
    };
    let host_key = match parse_public_key_hex(host_key.trim()) {
        Ok(k) => k,
        Err(e) => {
            log::error!("nativeStart: host key: {e}");
            return 0;
        }
    };
    let identity = match crate::identity::load_or_create(&PathBuf::from(dir)) {
        Ok(i) => i,
        Err(e) => {
            log::error!("nativeStart: {e}");
            return 0;
        }
    };
    // SAFETY: env and surface are valid for the duration of this JNI call;
    // ANativeWindow_fromSurface takes its own reference.
    let window =
        unsafe { NativeWindow::from_surface(env.get_raw().cast(), surface.as_raw().cast()) };
    let Some(window) = window else {
        log::error!("nativeStart: surface has no native window");
        return 0;
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    let config = Config {
        server: server.clone(),
        host_key,
    };
    let worker = {
        let (stop, stats) = (stop.clone(), stats.clone());
        std::thread::Builder::new()
            .name("emersia-stream".into())
            .spawn(move || {
                boost_current_thread();
                let mut decoder = SurfaceDecoder::new(window, stats.clone());
                session::run(
                    &config,
                    &identity,
                    &mut decoder,
                    Some(&input_rx),
                    &stop,
                    &stats,
                );
                log::info!("stream thread exiting");
            })
    };
    let worker = match worker {
        Ok(w) => w,
        Err(e) => {
            log::error!("nativeStart: spawn: {e}");
            return 0;
        }
    };
    let running = Box::new(Running {
        stop,
        input: input_tx,
        stats,
        worker: Some(worker),
        server,
        sampler: crate::status::Sampler::default(),
    });
    Box::into_raw(running) as jlong
}

/// `void nativeStop(long handle)`. Blocks until the stream thread has
/// released the Surface, which must happen before `surfaceDestroyed` returns.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: handle came from Box::into_raw in nativeStart and Java clears
    // its copy after this call, so it is reclaimed exactly once.
    let mut running = unsafe { Box::from_raw(handle as *mut Running) };
    running.stop.store(true, Ordering::Relaxed);
    if let Some(worker) = running.worker.take() {
        let _ = worker.join();
    }
}

/// `String nativeStatus(long handle)` — one human-readable status block.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeStatus(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    if handle == 0 {
        return jstr(&mut env, "not running");
    }
    // SAFETY: valid until nativeStop, which Java never calls concurrently with
    // this (both run on the UI thread).
    let running = unsafe { &*(handle as *const Running) };
    let text = crate::status::render(&running.server, &running.stats, &running.sampler);
    jstr(&mut env, &text)
}

/// Queue an input event for the stream thread. Called on the UI thread; never
/// blocks (unbounded channel, drained every ≤ 2 ms by the stream thread).
fn push_input(handle: jlong, event: InputEvent) {
    if handle == 0 {
        return;
    }
    // SAFETY: as in nativeStatus — valid until nativeStop, same thread.
    let running = unsafe { &*(handle as *const Running) };
    let _ = running.input.send(event);
}

fn unit(v: jfloat) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

/// `void nativePointer(long handle, float x, float y)` — position on the
/// video in 0..1.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativePointer(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    x: jfloat,
    y: jfloat,
) {
    push_input(
        handle,
        InputEvent::Pointer {
            x: unit(x),
            y: unit(y),
        },
    );
}

/// `void nativeButton(long handle, int button, boolean down)` — 0 left,
/// 1 right, 2 middle.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeButton(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    button: jint,
    down: jboolean,
) {
    let button = match button {
        1 => Button::Right,
        2 => Button::Middle,
        _ => Button::Left,
    };
    push_input(
        handle,
        InputEvent::Button {
            button,
            down: down == JNI_TRUE,
        },
    );
}

/// `void nativeScroll(long handle, float dx, float dy)` — in wheel detents,
/// positive = right / down.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeScroll(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    dx: jfloat,
    dy: jfloat,
) {
    let hires = |v: jfloat| (v * 120.0).round().clamp(-32768.0, 32767.0) as i16;
    push_input(
        handle,
        InputEvent::Scroll {
            dx: hires(dx),
            dy: hires(dy),
        },
    );
}

/// `void nativeKey(long handle, int evdevCode, boolean down)`.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeKey(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    code: jint,
    down: jboolean,
) {
    if let Ok(code) = u16::try_from(code) {
        if (1..=emersia_transport::input::KEY_MAX).contains(&code) {
            push_input(
                handle,
                InputEvent::Key {
                    code,
                    down: down == JNI_TRUE,
                },
            );
        }
    }
}

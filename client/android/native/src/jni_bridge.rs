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
    decoders: crate::decoder::Decoders,
    idr_wanted: Arc<std::sync::atomic::AtomicU32>,
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
            .with_max_level(log::LevelFilter::Debug),
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
    // A null surface starts the session headless: immersive mode attaches
    // its OpenXR surfaces per stream instead of a panel's.
    let window = if surface.as_raw().is_null() {
        None
    } else {
        // SAFETY: env and surface are valid for the duration of this JNI call;
        // ANativeWindow_fromSurface takes its own reference.
        let window =
            unsafe { NativeWindow::from_surface(env.get_raw().cast(), surface.as_raw().cast()) };
        if window.is_none() {
            log::error!("nativeStart: surface has no native window");
            return 0;
        }
        window
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    let decoders: crate::decoder::Decoders = Default::default();
    let idr_wanted = Arc::new(std::sync::atomic::AtomicU32::new(0));
    if let Some(window) = window {
        decoders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(0, SurfaceDecoder::new(window, stats.clone()));
    }
    let config = Config {
        server: server.clone(),
        host_key,
    };
    let worker = {
        let (stop, stats) = (stop.clone(), stats.clone());
        let (decoders, idr_wanted) = (decoders.clone(), idr_wanted.clone());
        std::thread::Builder::new()
            .name("emersia-stream".into())
            .spawn(move || {
                boost_current_thread();
                // Built here: the audio stream must live on this thread.
                let mut decoder = crate::decoder::MultiSink {
                    decoders,
                    idr_wanted,
                    audio: Default::default(),
                };
                // Presence announcements arrive on the discovery port; if it
                // is busy (pairing running), simply do not follow.
                let discovery = std::net::UdpSocket::bind((
                    "0.0.0.0",
                    emersia_transport::pairing::DISCOVERY_PORT,
                ))
                .ok();
                session::run_following(
                    &config,
                    &identity,
                    &mut decoder,
                    Some(&input_rx),
                    discovery.as_ref(),
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
        decoders,
        idr_wanted,
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

/// A running in-headset pairing (ADR 0008).
struct Pairing {
    state: Arc<std::sync::Mutex<crate::pairing::PairState>>,
    confirmed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

/// `long nativePairStart(String filesDir, String deviceName)` → handle, or 0.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativePairStart(
    mut env: JNIEnv,
    _class: JClass,
    files_dir: JString,
    device_name: JString,
) -> jlong {
    init_logging();
    let (Some(dir), Some(name)) = (string(&mut env, &files_dir), string(&mut env, &device_name))
    else {
        return 0;
    };
    let identity = match crate::identity::load_or_create(&PathBuf::from(dir)) {
        Ok(i) => i,
        Err(e) => {
            log::error!("pairing: {e}");
            return 0;
        }
    };
    let state = Arc::new(std::sync::Mutex::new(crate::pairing::PairState::Searching));
    let confirmed = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (state, confirmed, stop) = (state.clone(), confirmed.clone(), stop.clone());
        std::thread::Builder::new()
            .name("emersia-pair".into())
            .spawn(move || {
                let addr = ("0.0.0.0", emersia_transport::pairing::DISCOVERY_PORT);
                match std::net::UdpSocket::bind(addr) {
                    Ok(discovery) => {
                        crate::pairing::run(&discovery, &identity, &name, &state, &confirmed, &stop)
                    }
                    Err(e) => {
                        *state.lock().unwrap_or_else(|p| p.into_inner()) =
                            crate::pairing::PairState::Failed(format!("discovery port: {e}"));
                    }
                }
            })
            .ok()
    };
    Box::into_raw(Box::new(Pairing {
        state,
        confirmed,
        stop,
        worker,
    })) as jlong
}

/// `String nativePairStatus(long handle)` — see `PairState::to_line`.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativePairStatus(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    if handle == 0 {
        return jstr(&mut env, "error|not started");
    }
    // SAFETY: valid until nativePairStop, called on the same (UI) thread.
    let pairing = unsafe { &*(handle as *const Pairing) };
    let line = pairing
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .to_line();
    jstr(&mut env, &line)
}

/// `void nativePairConfirm(long handle)` — the headset user says the codes match.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativePairConfirm(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle != 0 {
        // SAFETY: as in nativePairStatus.
        let pairing = unsafe { &*(handle as *const Pairing) };
        pairing.confirmed.store(true, Ordering::Relaxed);
    }
}

/// `void nativePairStop(long handle)` — cancel/finish; frees the handle.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativePairStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: from Box::into_raw in nativePairStart; Java drops its copy.
    let mut pairing = unsafe { Box::from_raw(handle as *mut Pairing) };
    pairing.stop.store(true, Ordering::Relaxed);
    if let Some(w) = pairing.worker.take() {
        let _ = w.join();
    }
}

/// `String nativeServer(long handle)` — the address in use, so the app can
/// remember where the host moved to.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeServer(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    if handle == 0 {
        return jstr(&mut env, "");
    }
    // SAFETY: as in nativeStatus.
    let running = unsafe { &*(handle as *const Running) };
    let server = running.stats.server();
    jstr(&mut env, &server)
}

/// `boolean nativeAttach(long handle, int stream, Surface surface)` — a panel
/// shows `stream` on `surface`; a keyframe is requested for it.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeAttach(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    stream: jint,
    surface: JObject,
) -> jboolean {
    let Ok(stream) = u8::try_from(stream) else {
        return 0;
    };
    if handle == 0 || stream >= emersia_transport::transport::MAX_STREAMS {
        return 0;
    }
    // SAFETY: as in nativeStatus.
    let running = unsafe { &*(handle as *const Running) };
    // SAFETY: env and surface are valid for this call; the window holds its
    // own reference.
    let window =
        unsafe { NativeWindow::from_surface(env.get_raw().cast(), surface.as_raw().cast()) };
    let Some(window) = window else {
        return 0;
    };
    running
        .decoders
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(stream, SurfaceDecoder::new(window, running.stats.clone()));
    running.idr_wanted.fetch_or(1 << stream, Ordering::Relaxed);
    log::info!("panel attached to stream {stream}");
    1
}

/// `void nativeDetach(long handle, int stream)` — the panel's Surface is
/// going away; returns only once the decoder no longer uses it.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeDetach(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    stream: jint,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: as in nativeStatus.
    let running = unsafe { &*(handle as *const Running) };
    if let Ok(stream) = u8::try_from(stream) {
        let removed = running
            .decoders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&stream);
        drop(removed); // stops the codec while the lock guarantees no user
    }
}

/// `int nativeStreams(long handle)` — bitmask of stream ids seen so far.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeStreams(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    if handle == 0 {
        return 0;
    }
    // SAFETY: as in nativeStatus.
    let running = unsafe { &*(handle as *const Running) };
    running.stats.streams_seen.load(Ordering::Relaxed) as jint
}

/// `void nativePointerOn(long handle, int stream, float x, float y)`.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativePointerOn(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    stream: jint,
    x: jfloat,
    y: jfloat,
) {
    if let Ok(stream) = u8::try_from(stream) {
        push_input(
            handle,
            InputEvent::PointerOn {
                stream,
                x: unit(x),
                y: unit(y),
            },
        );
    }
}

struct Immersive {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    _activity: jni::objects::GlobalRef,
}

/// `long nativeImmersiveStart(long session, Activity activity)` — curved
/// screens in an OpenXR scene for every stream of `session` (ADR 0009).
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeImmersiveStart(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    activity: JObject,
) -> jlong {
    init_logging();
    if handle == 0 {
        return 0;
    }
    // SAFETY: as in nativeStatus.
    let running = unsafe { &*(handle as *const Running) };
    let (Ok(vm), Ok(activity)) = (env.get_java_vm(), env.new_global_ref(activity)) else {
        return 0;
    };
    let shared = crate::immersive::Shared {
        decoders: running.decoders.clone(),
        idr_wanted: running.idr_wanted.clone(),
        stats: running.stats.clone(),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let vm_ptr = vm.get_java_vm_pointer() as usize;
    let activity_ptr = activity.as_obj().as_raw() as usize;
    let worker = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("emersia-xr".into())
            .spawn(move || {
                boost_current_thread();
                // SAFETY: the VM outlives the process; the activity is held
                // by a global reference in `Immersive` until this thread ends.
                let result = unsafe {
                    crate::immersive::run(
                        vm_ptr as *mut std::ffi::c_void,
                        activity_ptr as *mut std::ffi::c_void,
                        &shared,
                        &stop,
                    )
                };
                match result {
                    Ok(()) => log::info!("immersive session ended"),
                    Err(e) => log::error!("immersive mode failed: {e}"),
                }
            })
            .ok()
    };
    Box::into_raw(Box::new(Immersive {
        stop,
        worker,
        _activity: activity,
    })) as jlong
}

/// `void nativeImmersiveStop(long immersive)`.
#[no_mangle]
pub extern "system" fn Java_org_emersia_client_NativeBridge_nativeImmersiveStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: from Box::into_raw in nativeImmersiveStart; freed once.
    let mut immersive = unsafe { Box::from_raw(handle as *mut Immersive) };
    immersive.stop.store(true, Ordering::Relaxed);
    if let Some(w) = immersive.worker.take() {
        let _ = w.join();
    }
}

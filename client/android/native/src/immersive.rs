//! Immersive mode (ADR 0009): each stream is a curved screen in an OpenXR
//! scene, with no rendering of our own.
//!
//! For every stream the runtime creates an `XR_KHR_android_surface_swapchain`,
//! whose Android Surface becomes that stream's MediaCodec output. The frame is
//! then submitted as an `XR_KHR_composition_layer_cylinder`: the compositor
//! samples the decoded image directly at display resolution and reprojects it
//! with head motion. Decoded pixels are never copied or touched by the GPU on
//! our side, which is the cheapest and lowest-latency path the platform has.
//! A minimal EGL context exists only because a session needs a graphics
//! binding.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use emersia_transport::transport::MAX_STREAMS;
use ndk::native_window::NativeWindow;
use openxr as xr;

use crate::decoder::{Decoders, SurfaceDecoder};
use crate::session::Stats;

use crate::layout::Layout;
use emersia_transport::input::{Button, InputEvent};

/// Swapchain size: the stream's native resolution.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// What the immersive thread shares with the stream thread.
pub struct Shared {
    /// Where the user's screen arrangement is remembered.
    pub layout_file: std::path::PathBuf,
    /// Pointer/button/scroll events for the host (controller input).
    pub input: std::sync::mpsc::Sender<emersia_transport::input::InputEvent>,
    pub decoders: Decoders,
    pub idr_wanted: Arc<AtomicU32>,
    pub stats: Arc<Stats>,
}

/// One controller's input this frame.
#[derive(Clone, Copy)]
struct HandState {
    right: bool,
    /// Aim position and orientation, when tracked.
    ray: Option<(xr::Vector3f, xr::Quaternionf)>,
    trigger: f32,
    /// Vertical stick deflection used for scrolling (0 for the left hand).
    scroll: f32,
}

/// Controller → desktop pointer, for the hand last used.
#[derive(Default)]
struct Pointer {
    /// Which hand drives the pointer (true = right); switches on a click.
    right: Option<bool>,
    pressed: bool,
    last: Option<(u8, u16, u16)>,
}

impl Pointer {
    fn update(
        &mut self,
        hand: &HandState,
        streams: &[u8],
        layout: &Layout,
        out: &std::sync::mpsc::Sender<InputEvent>,
    ) {
        let HandState {
            right,
            ray,
            trigger,
            scroll: stick,
        } = *hand;
        // A click with the other hand hands the pointer over.
        if trigger > 0.7 && self.right != Some(right) && !self.pressed {
            self.right = Some(right);
        }
        if self.right.unwrap_or(true) != right {
            return;
        }
        if let Some((p, q)) = ray {
            // Aim direction = -Z rotated by the controller orientation.
            let dir = [
                -2.0 * (q.x * q.z + q.w * q.y),
                -2.0 * (q.y * q.z - q.w * q.x),
                -(1.0 - 2.0 * (q.x * q.x + q.y * q.y)),
            ];
            if let Some((index, u, v)) = layout.hit([p.x, p.y, p.z], dir, streams.len()) {
                let at = (streams[index], (u * 65535.0) as u16, (v * 65535.0) as u16);
                if self.last != Some(at) {
                    let _ = out.send(InputEvent::PointerOn {
                        stream: at.0,
                        x: at.1,
                        y: at.2,
                    });
                    self.last = Some(at);
                }
            }
        }
        // Hysteresis so a half-pulled trigger does not chatter.
        if !self.pressed && trigger > 0.7 {
            self.pressed = true;
            let _ = out.send(InputEvent::Button {
                button: Button::Left,
                down: true,
            });
        } else if self.pressed && trigger < 0.3 {
            self.pressed = false;
            let _ = out.send(InputEvent::Button {
                button: Button::Left,
                down: false,
            });
        }
        if stick.abs() > 0.2 {
            // Up on the stick scrolls up; up to ~18 detents/s at 72 Hz.
            let _ = out.send(InputEvent::Scroll {
                dx: 0,
                dy: (-stick * 30.0) as i16,
            });
        }
    }
}

struct Screen {
    swapchain: xr::sys::Swapchain,
}

struct Egl {
    egl: khronos_egl::Instance<khronos_egl::Static>,
    display: khronos_egl::Display,
    config: khronos_egl::Config,
    context: khronos_egl::Context,
    surface: khronos_egl::Surface,
}

impl Egl {
    fn new() -> Result<Self, String> {
        use khronos_egl as egl;
        let instance = egl::Instance::new(egl::Static);
        // SAFETY: EGL_DEFAULT_DISPLAY is always a valid argument.
        let display =
            unsafe { instance.get_display(egl::DEFAULT_DISPLAY) }.ok_or("no EGL display")?;
        instance
            .initialize(display)
            .map_err(|e| format!("eglInitialize: {e}"))?;
        let attribs = [
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES3_BIT,
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::NONE,
        ];
        let config = instance
            .choose_first_config(display, &attribs)
            .map_err(|e| format!("eglChooseConfig: {e}"))?
            .ok_or("no EGL config")?;
        let context = instance
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            )
            .map_err(|e| format!("eglCreateContext: {e}"))?;
        let surface = instance
            .create_pbuffer_surface(
                display,
                config,
                &[egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE],
            )
            .map_err(|e| format!("eglCreatePbufferSurface: {e}"))?;
        instance
            .make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|e| format!("eglMakeCurrent: {e}"))?;
        Ok(Self {
            egl: instance,
            display,
            config,
            context,
            surface,
        })
    }
}

impl Drop for Egl {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
    }
}

/// Run the immersive session until `stop` is set or the runtime ends it.
///
/// # Safety
/// `vm` and `activity` must be the process's JavaVM and a global reference to
/// the running Activity; they are handed to the OpenXR loader.
pub unsafe fn run(
    vm: *mut c_void,
    activity: *mut c_void,
    shared: &Shared,
    stop: &AtomicBool,
) -> Result<(), String> {
    // The loader and instance creation read the VM/Activity from here.
    ndk_context::initialize_android_context(vm, activity);
    let jvm = jni::JavaVM::from_raw(vm.cast()).map_err(|e| format!("JavaVM: {e}"))?;
    let env = jvm
        .attach_current_thread_permanently()
        .map_err(|e| format!("attach: {e}"))?;

    let entry = xr::Entry::load().map_err(|e| format!("OpenXR loader: {e}"))?;
    entry
        .initialize_android_loader()
        .map_err(|e| format!("xrInitializeLoaderKHR: {e}"))?;
    let available = entry
        .enumerate_extensions()
        .map_err(|e| format!("extensions: {e}"))?;
    for (have, name) in [
        (available.khr_opengl_es_enable, "XR_KHR_opengl_es_enable"),
        (
            available.khr_android_surface_swapchain,
            "XR_KHR_android_surface_swapchain",
        ),
        (
            available.khr_composition_layer_cylinder,
            "XR_KHR_composition_layer_cylinder",
        ),
    ] {
        if !have {
            return Err(format!("runtime lacks {name}"));
        }
    }
    let mut wanted = xr::ExtensionSet::default();
    wanted.khr_android_create_instance = true;
    wanted.khr_opengl_es_enable = true;
    wanted.khr_android_surface_swapchain = true;
    wanted.khr_composition_layer_cylinder = true;
    // Optional: the room behind the screens (Quest passthrough).
    wanted.fb_passthrough = available.fb_passthrough;
    let instance = entry
        .create_instance(
            &xr::ApplicationInfo {
                application_name: "Emersia",
                application_version: 1,
                engine_name: "Emersia",
                engine_version: 1,
                api_version: xr::Version::new(1, 0, 0),
            },
            &wanted,
            &[],
        )
        .map_err(|e| format!("xrCreateInstance: {e}"))?;
    let props = instance.properties().map_err(|e| e.to_string())?;
    log::info!(
        "OpenXR runtime: {} {}",
        props.runtime_name,
        props.runtime_version
    );
    let system = instance
        .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
        .map_err(|e| format!("xrGetSystem: {e}"))?;
    // Required before session creation, even though we draw nothing.
    let _ = instance
        .graphics_requirements::<xr::OpenGlEs>(system)
        .map_err(|e| format!("GLES requirements: {e}"))?;
    let egl = Egl::new()?;
    let (session, mut waiter, mut stream) = instance
        .create_session::<xr::OpenGlEs>(
            system,
            &xr::opengles::SessionCreateInfo::Android {
                display: egl.display.as_ptr() as _,
                config: egl.config.as_ptr() as _,
                context: egl.context.as_ptr() as _,
            },
        )
        .map_err(|e| format!("xrCreateSession: {e}"))?;
    let space = session
        .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
        .map_err(|e| format!("reference space: {e}"))?;
    let create_surface = instance
        .exts()
        .khr_android_surface_swapchain
        .ok_or("surface swapchain extension not loaded")?
        .create_swapchain_android_surface;

    // Passthrough: the user's room as the bottom layer, screens on top.
    let passthrough = if available.fb_passthrough {
        match session
            .create_passthrough(xr::PassthroughFlagsFB::IS_RUNNING_AT_CREATION)
            .and_then(|p| {
                let layer = session.create_passthrough_layer(
                    &p,
                    xr::PassthroughFlagsFB::IS_RUNNING_AT_CREATION,
                    xr::PassthroughLayerPurposeFB::RECONSTRUCTION,
                )?;
                Ok((p, layer))
            }) {
            Ok(pair) => {
                log::info!("passthrough on");
                Some(pair)
            }
            Err(e) => {
                log::warn!("passthrough unavailable ({e}); black background");
                None
            }
        }
    } else {
        None
    };

    // Controller input: aim pose, trigger (click), thumbstick (scroll).
    let action_set = instance
        .create_action_set("desktop", "Desktop", 0)
        .map_err(|e| format!("action set: {e}"))?;
    let path = |p: &str| instance.string_to_path(p).map_err(|e| format!("{p}: {e}"));
    let hands = [path("/user/hand/left")?, path("/user/hand/right")?];
    let aim = action_set
        .create_action::<xr::Posef>("aim", "Aim", &hands)
        .map_err(|e| e.to_string())?;
    let select = action_set
        .create_action::<f32>("select", "Click", &hands)
        .map_err(|e| e.to_string())?;
    let scroll = action_set
        .create_action::<xr::Vector2f>("scroll", "Scroll", &hands)
        .map_err(|e| e.to_string())?;
    instance
        .suggest_interaction_profile_bindings(
            path("/interaction_profiles/oculus/touch_controller")?,
            &[
                xr::Binding::new(&aim, path("/user/hand/left/input/aim/pose")?),
                xr::Binding::new(&aim, path("/user/hand/right/input/aim/pose")?),
                xr::Binding::new(&select, path("/user/hand/left/input/trigger/value")?),
                xr::Binding::new(&select, path("/user/hand/right/input/trigger/value")?),
                xr::Binding::new(&scroll, path("/user/hand/left/input/thumbstick")?),
                xr::Binding::new(&scroll, path("/user/hand/right/input/thumbstick")?),
            ],
        )
        .map_err(|e| format!("bindings: {e}"))?;
    session
        .attach_action_sets(&[&action_set])
        .map_err(|e| format!("attach actions: {e}"))?;
    let aim_spaces = [
        aim.create_space(session.clone(), hands[0], xr::Posef::IDENTITY)
            .map_err(|e| e.to_string())?,
        aim.create_space(session.clone(), hands[1], xr::Posef::IDENTITY)
            .map_err(|e| e.to_string())?,
    ];
    let mut pointer = Pointer::default();
    let mut layout = std::fs::read_to_string(&shared.layout_file)
        .map(|l| Layout::from_line(&l))
        .unwrap_or_default();
    let mut layout_dirty: Option<std::time::Instant> = None;
    let mut last_frame = std::time::Instant::now();

    let mut screens: BTreeMap<u8, Screen> = BTreeMap::new();
    let mut running = false;
    let mut events = xr::EventDataBuffer::new();
    let mut result = Ok(());
    'main: while !stop.load(Ordering::Relaxed) {
        while let Some(event) = instance
            .poll_event(&mut events)
            .map_err(|e| e.to_string())?
        {
            if let xr::Event::SessionStateChanged(change) = event {
                log::info!("XR session state: {:?}", change.state());
                match change.state() {
                    xr::SessionState::READY => {
                        session
                            .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
                            .map_err(|e| format!("xrBeginSession: {e}"))?;
                        running = true;
                    }
                    xr::SessionState::STOPPING => {
                        let _ = session.end();
                        running = false;
                    }
                    xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => break 'main,
                    _ => {}
                }
            }
        }
        if !running {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        // A swapchain (and decoder) per stream the host sends.
        let seen = shared.stats.streams_seen.load(Ordering::Relaxed);
        for id in 0..MAX_STREAMS {
            if seen & (1 << id) == 0 || screens.contains_key(&id) {
                continue;
            }
            let info = xr::sys::SwapchainCreateInfo {
                ty: xr::sys::SwapchainCreateInfo::TYPE,
                next: std::ptr::null(),
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::SAMPLED
                    | xr::SwapchainUsageFlags::COLOR_ATTACHMENT,
                // The Surface defines format and layout; Meta's runtime
                // rejects non-zero counts here (ERROR_VALIDATION_FAILURE).
                format: 0,
                sample_count: 0,
                width: WIDTH,
                height: HEIGHT,
                face_count: 0,
                array_size: 0,
                mip_count: 0,
            };
            let mut swapchain = xr::sys::Swapchain::NULL;
            let mut surface: *mut c_void = std::ptr::null_mut();
            let r = create_surface(
                session.as_raw(),
                &info,
                &mut swapchain,
                (&mut surface as *mut *mut c_void).cast(),
            );
            if r.into_raw() < 0 || surface.is_null() {
                result = Err(format!("xrCreateSwapchainAndroidSurfaceKHR: {r:?}"));
                break 'main;
            }
            let window = NativeWindow::from_surface(env.get_raw().cast(), surface.cast());
            // Meta's runtime returns the Surface as a *global* reference that it owns
            // for the swapchain's lifetime (CheckJNI aborted when it was deleted as
            // local), so it is not deleted here; the window holds its own ref.
            let Some(window) = window else {
                result = Err("surface swapchain without a native window".into());
                break 'main;
            };
            shared
                .decoders
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, SurfaceDecoder::new(window, shared.stats.clone()));
            shared.idr_wanted.fetch_or(1 << id, Ordering::Relaxed);
            screens.insert(id, Screen { swapchain });
            log::info!("immersive screen for stream {id}");
        }

        let state = waiter.wait().map_err(|e| format!("xrWaitFrame: {e}"))?;
        let frame_dt = last_frame.elapsed().as_secs_f32().min(0.1);
        last_frame = std::time::Instant::now();
        // Save the arrangement once the user has stopped adjusting it.
        if layout_dirty.is_some_and(|t| t.elapsed() > Duration::from_secs(1)) {
            layout_dirty = None;
            if let Err(e) = std::fs::write(&shared.layout_file, layout.to_line()) {
                log::warn!("saving layout: {e}");
            }
        }
        if session.sync_actions(&[(&action_set).into()]).is_ok() {
            let ids: Vec<u8> = screens.keys().copied().collect();
            for (hand, aim_space) in hands.iter().zip(&aim_spaces) {
                let trigger = select
                    .state(&session, *hand)
                    .map(|s| s.current_state)
                    .unwrap_or(0.0);
                let stick = scroll
                    .state(&session, *hand)
                    .map(|s| s.current_state)
                    .unwrap_or_default();
                let left = *hand == hands[0];
                // Left stick arranges the screens; right stick scrolls.
                if left && layout.adjust(stick.x, stick.y, frame_dt) {
                    layout_dirty = Some(std::time::Instant::now());
                }
                let scroll_y = if left { 0.0 } else { stick.y };
                let ray = aim_space
                    .locate(&space, state.predicted_display_time)
                    .ok()
                    .filter(|l| {
                        l.location_flags.contains(
                            xr::SpaceLocationFlags::POSITION_VALID
                                | xr::SpaceLocationFlags::ORIENTATION_VALID,
                        )
                    })
                    .map(|l| (l.pose.position, l.pose.orientation));
                let hand = HandState {
                    right: !left,
                    ray,
                    trigger,
                    scroll: scroll_y,
                };
                pointer.update(&hand, &ids, &layout, &shared.input);
            }
        }
        stream.begin().map_err(|e| format!("xrBeginFrame: {e}"))?;
        let count = screens.len();
        let layers: Vec<xr::sys::CompositionLayerCylinderKHR> = screens
            .values()
            .enumerate()
            .map(|(i, screen)| {
                let yaw = layout.yaw(i, count);
                xr::sys::CompositionLayerCylinderKHR {
                    ty: xr::sys::CompositionLayerCylinderKHR::TYPE,
                    next: std::ptr::null(),
                    layer_flags: xr::CompositionLayerFlags::EMPTY,
                    space: space.as_raw(),
                    eye_visibility: xr::EyeVisibility::BOTH,
                    sub_image: xr::sys::SwapchainSubImage {
                        swapchain: screen.swapchain,
                        image_rect: xr::Rect2Di {
                            offset: xr::Offset2Di { x: 0, y: 0 },
                            extent: xr::Extent2Di {
                                width: WIDTH as i32,
                                height: HEIGHT as i32,
                            },
                        },
                        image_array_index: 0,
                    },
                    pose: xr::Posef {
                        orientation: xr::Quaternionf {
                            x: 0.0,
                            y: (yaw / 2.0).sin(),
                            z: 0.0,
                            w: (yaw / 2.0).cos(),
                        },
                        position: xr::Vector3f::default(),
                    },
                    radius: layout.radius,
                    central_angle: layout.arc(),
                    aspect_ratio: WIDTH as f32 / HEIGHT as f32,
                }
            })
            .collect();
        let passthrough_layer =
            passthrough
                .as_ref()
                .map(|(_, layer)| xr::sys::CompositionLayerPassthroughFB {
                    ty: xr::sys::CompositionLayerPassthroughFB::TYPE,
                    next: std::ptr::null(),
                    flags: xr::CompositionLayerFlags::EMPTY,
                    space: xr::sys::Space::NULL,
                    layer_handle: *layer.inner(),
                });
        let mut pointers: Vec<*const xr::sys::CompositionLayerBaseHeader> = Vec::new();
        if state.should_render {
            // Passthrough must come first: layers composite bottom to top.
            if let Some(pt) = &passthrough_layer {
                pointers.push((pt as *const xr::sys::CompositionLayerPassthroughFB).cast());
            }
            pointers.extend(
                layers
                    .iter()
                    .map(|l| (l as *const xr::sys::CompositionLayerCylinderKHR).cast()),
            );
        }
        let end = xr::sys::FrameEndInfo {
            ty: xr::sys::FrameEndInfo::TYPE,
            next: std::ptr::null(),
            display_time: state.predicted_display_time,
            environment_blend_mode: xr::EnvironmentBlendMode::OPAQUE,
            layer_count: pointers.len() as u32,
            layers: pointers.as_ptr(),
        };
        let r = (instance.fp().end_frame)(session.as_raw(), &end);
        if r.into_raw() < 0 {
            result = Err(format!("xrEndFrame: {r:?}"));
            break;
        }
    }

    // Decoders first: they must stop using the surfaces before the
    // swapchains that own them go away.
    {
        let mut decoders = shared.decoders.lock().unwrap_or_else(|p| p.into_inner());
        for id in screens.keys() {
            decoders.remove(id);
        }
    }
    for screen in screens.values() {
        let _ = (instance.fp().destroy_swapchain)(screen.swapchain);
    }
    drop(passthrough);
    drop(space);
    drop(session);
    drop(egl);
    result
}

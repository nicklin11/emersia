//! Zero-copy capture and encode (#35).
//!
//! The compositor copies each frame into a GPU buffer we allocated (GBM,
//! LINEAR, exported as a dmabuf). libavutil maps that dmabuf to a VAAPI
//! surface without copying, `scale_vaapi` converts BGRX → NV12 (BT.709 limited)
//! on the GPU, and the VAAPI encoder runs in-process. Pixels never touch the
//! CPU and no child process is involved: measured on the reference box at
//! ~3.5 % of one core per 1080p60 stream, against ~18 % for the shm + ffmpeg
//! pipeline (daemon share plus the ffmpeg child).
//!
//! In-process encoding also allows what the CLI could not: an IDR on demand
//! (`pict_type = I`, no encoder restart) and presentation timestamps from the
//! real capture time, so rate control sees the actual frame spacing.
//!
//! Anything missing (no dmabuf screencopy, no GBM, no VAAPI, an encoder that
//! refuses the setup) makes [`ZeroCopy::start`] fail; the engine then keeps
//! the shm + ffmpeg path for that stream.

// FFmpeg constants change integer type between releases (bindgen output for
// FFmpeg 6 vs 8), so casts that are no-ops on one version are needed on another.
#![allow(clippy::unnecessary_cast)]

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use ffmpeg_sys_next as ff;
use wayland_client::protocol::{wl_buffer::WlBuffer, wl_output::WlOutput};
use wayland_client::{EventQueue, Proxy as _};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1;

use crate::capture::{wait_until, CaptureTimings, State, CONSTRAINT_TIMEOUT, FRAME_TIMEOUT};
use crate::codec::Codec;

/// `DRM_FORMAT_MOD_LINEAR`.
const MOD_LINEAR: u64 = 0;

/// Two GPU buffers used alternately: one is being encoded from while the
/// compositor writes the other.
const POOL_SIZE: usize = 2;

struct Slot {
    _bo: gbm::BufferObject<()>,
    fd: OwnedFd,
    stride: u32,
    buffer: WlBuffer,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// GPU buffers registered with the compositor for one output geometry.
pub struct DmabufPool {
    gbm: gbm::Device<std::fs::File>,
    slots: Vec<Slot>,
    next: usize,
    fourcc: u32,
    width: u32,
    height: u32,
}

impl DmabufPool {
    pub fn new(render_node: &str) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(render_node)
            .with_context(|| format!("open {render_node}"))?;
        Ok(Self {
            gbm: gbm::Device::new(file).context("GBM device")?,
            slots: Vec::new(),
            next: 0,
            fourcc: 0,
            width: 0,
            height: 0,
        })
    }

    /// DRM fourcc of the buffers the compositor fills.
    pub fn fourcc(&self) -> u32 {
        self.fourcc
    }

    /// (Re)allocate for a format/geometry the compositor asked for.
    fn ensure(
        &mut self,
        state: &State,
        qh: &wayland_client::QueueHandle<State>,
        fourcc: u32,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        if !self.slots.is_empty()
            && (fourcc, width, height) == (self.fourcc, self.width, self.height)
        {
            return Ok(());
        }
        self.slots.clear();
        let mgr = state
            .dmabuf_mgr
            .clone()
            .context("the compositor does not advertise zwp_linux_dmabuf_v1")?;
        let format = gbm::Format::try_from(fourcc)
            .map_err(|_| anyhow::anyhow!("unsupported dmabuf fourcc {fourcc:#x}"))?;
        for _ in 0..POOL_SIZE {
            let bo = self
                .gbm
                .create_buffer_object::<()>(
                    width,
                    height,
                    format,
                    gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::LINEAR,
                )
                .context("GBM buffer")?;
            let fd = bo.fd().context("export dmabuf")?;
            let stride = bo.stride();
            let params = mgr.create_params(qh, ());
            params.add(
                fd.as_fd(),
                0,
                0,
                stride,
                (MOD_LINEAR >> 32) as u32,
                MOD_LINEAR as u32,
            );
            let buffer = params.create_immed(
                width as i32,
                height as i32,
                fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                qh,
                (),
            );
            params.destroy();
            self.slots.push(Slot {
                _bo: bo,
                fd,
                stride,
                buffer,
            });
        }
        (self.fourcc, self.width, self.height) = (fourcc, width, height);
        Ok(())
    }
}

/// A frame the compositor wrote into one of the pool's buffers.
pub struct DmabufFrame {
    pub fd: i32,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
    pub timings: CaptureTimings,
}

/// Capture one frame of `output` into the pool with `wlr-screencopy`.
pub fn capture(
    queue: &mut EventQueue<State>,
    state: &mut State,
    output: &WlOutput,
    pool: &mut DmabufPool,
) -> anyhow::Result<DmabufFrame> {
    let total_started = Instant::now();
    let qh = queue.handle();
    state.wlr.reset();
    let mgr = state
        .wlr_mgr
        .clone()
        .context("the compositor does not advertise zwlr_screencopy_manager_v1")?;
    let frame = mgr.capture_output(i32::from(state.paint_cursor), output, &qh, ());
    let needs_buffer_done = frame.version() >= 3;
    let constraints_started = Instant::now();
    let described = wait_until(
        queue,
        state,
        Instant::now() + CONSTRAINT_TIMEOUT,
        "screencopy buffer description",
        |s| s.wlr.failed || (s.wlr.dmabuf.is_some() && (!needs_buffer_done || s.wlr.buffer_done)),
    );
    let constraints = constraints_started.elapsed();
    if let Err(e) = described {
        frame.destroy();
        return Err(e);
    }
    let Some((fourcc, width, height)) = state.wlr.dmabuf.filter(|_| !state.wlr.failed) else {
        frame.destroy();
        bail!("the compositor offered no dmabuf screencopy buffer");
    };
    if state.wlr.y_invert {
        frame.destroy();
        bail!("y-inverted dmabuf frames are not supported on the zero-copy path");
    }
    if let Err(e) = pool.ensure(state, &qh, fourcc, width, height) {
        frame.destroy();
        return Err(e);
    }
    let slot = &pool.slots[pool.next % POOL_SIZE];
    pool.next = pool.next.wrapping_add(1);
    frame.copy(&slot.buffer);
    let frame_wait_started = Instant::now();
    let copied = wait_until(
        queue,
        state,
        Instant::now() + FRAME_TIMEOUT,
        "screencopy frame",
        |s| s.wlr.ready || s.wlr.failed,
    );
    let frame_wait = frame_wait_started.elapsed();
    frame.destroy();
    copied?;
    if !state.wlr.ready {
        bail!("the compositor failed the dmabuf screencopy");
    }
    Ok(DmabufFrame {
        fd: slot.fd.as_raw_fd(),
        stride: slot.stride,
        width,
        height,
        timings: CaptureTimings {
            constraints,
            frame_wait,
            shm_read: Duration::ZERO,
            decode: Duration::ZERO,
            transform: Duration::ZERO,
            total: total_started.elapsed(),
        },
    })
}

fn av_err(code: i32, what: &str) -> anyhow::Error {
    let mut buf = [0 as std::ffi::c_char; 128];
    // SAFETY: buf is a valid, writable, NUL-terminated buffer of its size.
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    // SAFETY: av_strerror always NUL-terminates.
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    anyhow::anyhow!("{what}: {msg}")
}

macro_rules! av {
    ($e:expr, $what:expr) => {{
        let r = $e;
        if r < 0 {
            return Err(av_err(r, $what));
        }
        r
    }};
}

/// One encoded access unit.
pub struct Packet {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// In-process VAAPI encoder fed with dmabufs.
pub struct ZeroCopy {
    drm_frames: *mut ff::AVBufferRef,
    vaapi_frames: *mut ff::AVBufferRef,
    vaapi_dev: *mut ff::AVBufferRef,
    drm_dev: *mut ff::AVBufferRef,
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    codec: *mut ff::AVCodecContext,
    filtered: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    width: u32,
    height: u32,
    epoch: Instant,
    last_pts: i64,
    pub name: String,
}

// SAFETY: all FFmpeg objects are owned exclusively by this value and only
// used through &mut self, from one thread at a time.
unsafe impl Send for ZeroCopy {}

/// What the encoder needs to know up front.
pub struct Config {
    pub render_node: String,
    pub codec: Codec,
    pub bitrate: u32,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
}

const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258; // 'XR24'
const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241; // 'AR24'

impl ZeroCopy {
    /// Set up devices, frame contexts, the GPU colour-conversion graph and the
    /// encoder. Fails cleanly (freeing what was created) when anything is
    /// unavailable.
    pub fn start(config: &Config) -> anyhow::Result<Self> {
        let mut this = Self {
            drm_frames: ptr::null_mut(),
            vaapi_frames: ptr::null_mut(),
            vaapi_dev: ptr::null_mut(),
            drm_dev: ptr::null_mut(),
            graph: ptr::null_mut(),
            src: ptr::null_mut(),
            sink: ptr::null_mut(),
            codec: ptr::null_mut(),
            filtered: ptr::null_mut(),
            packet: ptr::null_mut(),
            width: config.width,
            height: config.height,
            epoch: Instant::now(),
            last_pts: -1,
            name: String::new(),
        };
        // SAFETY: FFI calls with pointers we own; every created object is
        // stored in `this`, whose Drop frees it on any early return.
        unsafe { this.init(config)? };
        Ok(this)
    }

    unsafe fn init(&mut self, c: &Config) -> anyhow::Result<()> {
        let (w, h) = (c.width as i32, c.height as i32);
        let node = CString::new(c.render_node.as_str())?;
        // DRM device first; VAAPI derived from it (the reverse is unsupported).
        av!(
            ff::av_hwdevice_ctx_create(
                &mut self.drm_dev,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                node.as_ptr(),
                ptr::null_mut(),
                0
            ),
            "DRM device"
        );
        av!(
            ff::av_hwdevice_ctx_create_derived(
                &mut self.vaapi_dev,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                self.drm_dev,
                0
            ),
            "VAAPI device"
        );
        self.drm_frames = ff::av_hwframe_ctx_alloc(self.drm_dev);
        if self.drm_frames.is_null() {
            bail!("DRM frames context: out of memory");
        }
        let fc = (*self.drm_frames).data as *mut ff::AVHWFramesContext;
        (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
        (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_BGR0;
        (*fc).width = w;
        (*fc).height = h;
        av!(ff::av_hwframe_ctx_init(self.drm_frames), "DRM frames");
        av!(
            ff::av_hwframe_ctx_create_derived(
                &mut self.vaapi_frames,
                ff::AVPixelFormat::AV_PIX_FMT_VAAPI,
                self.vaapi_dev,
                self.drm_frames,
                ff::AV_HWFRAME_MAP_DIRECT as i32
            ),
            "VAAPI frames"
        );

        // buffer(vaapi) -> scale_vaapi(nv12, BT.709 limited) -> buffersink
        self.graph = ff::avfilter_graph_alloc();
        if self.graph.is_null() {
            bail!("filter graph: out of memory");
        }
        self.src = ff::avfilter_graph_alloc_filter(
            self.graph,
            ff::avfilter_get_by_name(c"buffer".as_ptr()),
            c"in".as_ptr(),
        );
        if self.src.is_null() {
            bail!("buffer source");
        }
        // A hardware source needs its frames context before init (FFmpeg 8).
        let par = ff::av_buffersrc_parameters_alloc();
        (*par).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        (*par).width = w;
        (*par).height = h;
        (*par).time_base = ff::AVRational { num: 1, den: 1000 };
        (*par).hw_frames_ctx = self.vaapi_frames; // borrowed: set_parameters refs it
        let r = ff::av_buffersrc_parameters_set(self.src, par);
        ff::av_free(par as *mut _);
        av!(r, "buffer source parameters");
        av!(
            ff::avfilter_init_str(self.src, ptr::null()),
            "buffer source init"
        );
        let mut scale = ptr::null_mut();
        av!(
            ff::avfilter_graph_create_filter(
                &mut scale,
                ff::avfilter_get_by_name(c"scale_vaapi".as_ptr()),
                c"scale".as_ptr(),
                c"format=nv12:out_color_matrix=bt709:out_range=tv".as_ptr(),
                ptr::null_mut(),
                self.graph
            ),
            "scale_vaapi"
        );
        av!(
            ff::avfilter_graph_create_filter(
                &mut self.sink,
                ff::avfilter_get_by_name(c"buffersink".as_ptr()),
                c"out".as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                self.graph
            ),
            "buffer sink"
        );
        av!(ff::avfilter_link(self.src, 0, scale, 0), "link source");
        av!(ff::avfilter_link(scale, 0, self.sink, 0), "link sink");
        for i in 0..(*self.graph).nb_filters as usize {
            let f = *(*self.graph).filters.add(i);
            (*f).hw_device_ctx = ff::av_buffer_ref(self.vaapi_dev);
        }
        av!(
            ff::avfilter_graph_config(self.graph, ptr::null_mut()),
            "filter graph"
        );

        let name = match c.codec {
            Codec::H264 => c"h264_vaapi",
            Codec::Hevc => c"hevc_vaapi",
        };
        let encoder = ff::avcodec_find_encoder_by_name(name.as_ptr());
        if encoder.is_null() {
            bail!("{} not available", name.to_string_lossy());
        }
        self.codec = ff::avcodec_alloc_context3(encoder);
        if self.codec.is_null() {
            bail!("encoder context: out of memory");
        }
        let cc = self.codec;
        (*cc).width = w;
        (*cc).height = h;
        // Millisecond PTS from the real capture time: rate control sees the
        // actual (damage-driven, variable) frame spacing.
        (*cc).time_base = ff::AVRational { num: 1, den: 1000 };
        (*cc).framerate = ff::AVRational {
            num: c.fps.max(1) as i32,
            den: 1,
        };
        (*cc).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
        // Long GOP: keyframes come on demand (new session, loss) via pict_type.
        (*cc).gop_size = (c.fps.max(1) * 4) as i32;
        (*cc).max_b_frames = 0;
        (*cc).bit_rate = i64::from(c.bitrate);
        (*cc).color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
        (*cc).color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*cc).colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
        (*cc).color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
        (*cc).hw_frames_ctx = ff::av_buffer_ref(ff::av_buffersink_get_hw_frames_ctx(self.sink));
        // One frame in, its packet out: no encode pipelining (see #88).
        ff::av_opt_set((*cc).priv_data, c"async_depth".as_ptr(), c"1".as_ptr(), 0);
        av!(
            ff::avcodec_open2(cc, encoder, ptr::null_mut()),
            "open encoder"
        );
        self.filtered = ff::av_frame_alloc();
        self.packet = ff::av_packet_alloc();
        if self.filtered.is_null() || self.packet.is_null() {
            bail!("frame/packet: out of memory");
        }
        self.name = format!("{} (zero-copy)", name.to_string_lossy());
        Ok(())
    }

    pub fn geometry(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Encode the dmabuf `fd` (as captured into the pool). `force_idr`
    /// makes this frame a keyframe.
    pub fn encode(
        &mut self,
        fd: i32,
        stride: u32,
        fourcc: u32,
        captured_at: Instant,
        force_idr: bool,
    ) -> anyhow::Result<Vec<Packet>> {
        // SAFETY: see `start`; the descriptor owns a dup of `fd`, released by
        // its buffer's free callback, and frames are freed on every path.
        unsafe { self.encode_inner(fd, stride, fourcc, captured_at, force_idr) }
    }

    unsafe fn encode_inner(
        &mut self,
        fd: i32,
        stride: u32,
        fourcc: u32,
        captured_at: Instant,
        force_idr: bool,
    ) -> anyhow::Result<Vec<Packet>> {
        unsafe extern "C" fn free_desc(_: *mut std::ffi::c_void, data: *mut u8) {
            let desc = data as *mut ff::AVDRMFrameDescriptor;
            libc::close((*desc).objects[0].fd);
            ff::av_free(data as *mut _);
        }
        let drm_format = match fourcc {
            DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => fourcc,
            other => bail!("zero-copy path handles XRGB/ARGB8888, got {other:#x}"),
        };
        let size = std::mem::size_of::<ff::AVDRMFrameDescriptor>();
        let desc = ff::av_mallocz(size) as *mut ff::AVDRMFrameDescriptor;
        if desc.is_null() {
            bail!("descriptor: out of memory");
        }
        (*desc).nb_objects = 1;
        (*desc).objects[0].fd = libc::dup(fd);
        (*desc).objects[0].size = stride as usize * self.height as usize;
        (*desc).objects[0].format_modifier = MOD_LINEAR;
        (*desc).nb_layers = 1;
        (*desc).layers[0].format = drm_format;
        (*desc).layers[0].nb_planes = 1;
        (*desc).layers[0].planes[0].object_index = 0;
        (*desc).layers[0].planes[0].offset = 0;
        (*desc).layers[0].planes[0].pitch = stride as _;

        let mut drm = ff::av_frame_alloc();
        let mut va = ff::av_frame_alloc();
        let result = (|| -> anyhow::Result<Vec<Packet>> {
            if drm.is_null() || va.is_null() {
                bail!("frame: out of memory");
            }
            (*drm).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*drm).width = self.width as i32;
            (*drm).height = self.height as i32;
            (*drm).data[0] = desc as *mut u8;
            (*drm).buf[0] =
                ff::av_buffer_create(desc as *mut u8, size, Some(free_desc), ptr::null_mut(), 0);
            if (*drm).buf[0].is_null() {
                free_desc(ptr::null_mut(), desc as *mut u8);
                bail!("descriptor buffer: out of memory");
            }
            (*drm).hw_frames_ctx = ff::av_buffer_ref(self.drm_frames);
            (*va).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            (*va).hw_frames_ctx = ff::av_buffer_ref(self.vaapi_frames);
            av!(
                ff::av_hwframe_map(
                    va,
                    drm,
                    ff::AV_HWFRAME_MAP_READ as i32 | ff::AV_HWFRAME_MAP_DIRECT as i32
                ),
                "map dmabuf to VAAPI"
            );
            // Strictly increasing millisecond PTS from the capture time.
            let pts = (captured_at
                .saturating_duration_since(self.epoch)
                .as_millis() as i64)
                .max(self.last_pts + 1);
            self.last_pts = pts;
            (*va).pts = pts;
            av!(
                ff::av_buffersrc_add_frame_flags(self.src, va, 0),
                "push frame"
            );
            let mut out = Vec::new();
            while ff::av_buffersink_get_frame(self.sink, self.filtered) >= 0 {
                if force_idr {
                    (*self.filtered).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
                }
                let r = ff::avcodec_send_frame(self.codec, self.filtered);
                ff::av_frame_unref(self.filtered);
                av!(r, "send frame");
                while ff::avcodec_receive_packet(self.codec, self.packet) >= 0 {
                    let p = &*self.packet;
                    out.push(Packet {
                        data: std::slice::from_raw_parts(p.data, p.size as usize).to_vec(),
                        keyframe: p.flags & ff::AV_PKT_FLAG_KEY as i32 != 0,
                    });
                    ff::av_packet_unref(self.packet);
                }
            }
            Ok(out)
        })();
        ff::av_frame_free(&mut va);
        ff::av_frame_free(&mut drm);
        result
    }
}

impl Drop for ZeroCopy {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or owned by self; the free functions
        // accept null and null out what they free.
        unsafe {
            ff::av_packet_free(&mut self.packet);
            ff::av_frame_free(&mut self.filtered);
            ff::avcodec_free_context(&mut self.codec);
            ff::avfilter_graph_free(&mut self.graph);
            ff::av_buffer_unref(&mut self.vaapi_frames);
            ff::av_buffer_unref(&mut self.drm_frames);
            ff::av_buffer_unref(&mut self.vaapi_dev);
            ff::av_buffer_unref(&mut self.drm_dev);
        }
    }
}

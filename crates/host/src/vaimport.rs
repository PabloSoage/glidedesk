//! Zero-copy path for VA-API (AMD, Intel): PipeWire DMA-BUF → VA surface → VPP → NV12 surface
//! → `h264_vaapi` / `hevc_vaapi`.
//!
//! Mutter's BGRx/BGRA DMA-BUF is described as a DRM PRIME frame and mapped onto the VA
//! display (`av_hwframe_map`, which imports it with `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`
//! where the driver has it). A libavfilter graph (`scale_vaapi`, the driver's video
//! processor) converts it to full-range BT.709 NV12 in a surface of its own pool, which the
//! encoder takes as is. All of it runs on the PipeWire thread, and the surface is synced
//! before the buffer goes back to Mutter: the GPU has finished reading the DMA-BUF when Mutter
//! renders into it again. Nothing touches system memory.
//!
//! The encoder encodes at the capture size (SPS cropping) instead of padding to multiples of
//! 16 as the CPU path does: the video processor cannot pad.

use std::ffi::{CStr, CString, c_int, c_void};
use std::ptr;

use anyhow::{Context, Result, bail};
use ffmpeg_sys_next as ff;
use tracing::{debug, info};

use crate::gpu::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888};

/// Linear layout: what Mutter is asked for (every VA driver imports it).
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Failed conversions in a row after which the path is given up.
const GIVE_UP_AFTER: u32 = 30;

type SyncSurface = unsafe extern "C" fn(*mut c_void, u32) -> c_int;

fn averror(e: i32) -> String {
    let mut buf = [0 as std::ffi::c_char; 128];
    // SAFETY: the buffer is large enough and NUL-terminated by av_strerror.
    unsafe {
        ff::av_strerror(e, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn check(e: i32, what: &str) -> Result<i32> {
    if e < 0 {
        bail!("{what}: {}", averror(e));
    }
    Ok(e)
}

/// An NV12 VA surface (`AV_PIX_FMT_VAAPI`) from the video processor's pool, ready for the
/// encoder. Dropping it returns the surface to the pool.
pub struct VaFrame {
    frame: *mut ff::AVFrame,
}

// The frame is reference counted by FFmpeg; the encoder thread only reads it and the VA
// display is thread safe.
unsafe impl Send for VaFrame {}

impl VaFrame {
    pub fn as_ptr(&self) -> *const ff::AVFrame {
        self.frame
    }

    /// The pool the surface belongs to (the encoder is opened on it).
    pub fn frames_ctx(&self) -> *mut ff::AVBufferRef {
        // SAFETY: `frame` is a valid hardware frame for the life of `self`.
        unsafe { (*self.frame).hw_frames_ctx }
    }

    /// The pool's frames context itself, which identifies it (references to it differ).
    pub fn frames_data(&self) -> *const u8 {
        // SAFETY: a hardware frame always has its frames context.
        unsafe { (*self.frames_ctx()).data }
    }
}

impl Drop for VaFrame {
    fn drop(&mut self) {
        // SAFETY: owned by `self`, freed once.
        unsafe { ff::av_frame_free(&mut self.frame) };
    }
}

/// The filter graph for one capture size and format.
struct Graph {
    key: (u32, u32, u32),
    /// VA frames context the DMA-BUFs are mapped into (BGR0 or BGRA, no pool).
    map_frames: *mut ff::AVBufferRef,
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
}

impl Drop for Graph {
    fn drop(&mut self) {
        // SAFETY: the graph owns its filters; both objects are freed once.
        unsafe {
            ff::avfilter_graph_free(&mut self.graph);
            ff::av_buffer_unref(&mut self.map_frames);
        }
    }
}

/// The VA display the encoder uses and the conversion graph, on the PipeWire thread.
pub struct VaImporter {
    device: *mut ff::AVBufferRef,
    display: *mut c_void,
    sync: SyncSurface,
    _libva: libloading::Library,
    graph: Option<Graph>,
    /// A conversion worked once: later failures are taken as passing (a full pool).
    worked: bool,
    failures: u32,
    pts: i64,
}

// Raw FFmpeg and VA handles, used only from the thread that created them.
unsafe impl Send for VaImporter {}

/// A buffer of `desc` the source frame owns.
unsafe extern "C" fn free_descriptor(_opaque: *mut c_void, data: *mut u8) {
    // SAFETY: `data` is the Box made in `VaImporter::convert`, released once by FFmpeg.
    drop(unsafe { Box::from_raw(data.cast::<ff::AVDRMFrameDescriptor>()) });
}

impl VaImporter {
    /// Opens the VA display on `render_node` (the encoder's, `--render-node`).
    pub fn new(render_node: &str) -> Result<Self> {
        // SAFETY: loading libva; the symbol's type follows va.h.
        let (libva, sync) = unsafe {
            let lib = libloading::Library::new("libva.so.2").context("loading libva.so.2")?;
            let sync: SyncSurface = *lib.get::<SyncSurface>(b"vaSyncSurface\0")?;
            (lib, sync)
        };
        let node = CString::new(render_node)?;
        let mut device = ptr::null_mut();
        // SAFETY: plain FFmpeg API use; the device is owned by `self` and freed in Drop.
        let display = unsafe {
            check(
                ff::av_hwdevice_ctx_create(
                    &mut device,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    node.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "opening the VA-API device",
            )?;
            // AVVAAPIDeviceContext starts with its VADisplay (hwcontext_vaapi.h).
            let ctx = (*device).data.cast::<ff::AVHWDeviceContext>();
            *(*ctx).hwctx.cast::<*mut c_void>()
        };
        info!(render_node, "VA-API import of DMA-BUFs ready");
        Ok(Self {
            device,
            display,
            sync,
            _libva: libva,
            graph: None,
            worked: false,
            failures: 0,
            pts: 0,
        })
    }

    /// Whether the path should be given up for system memory: it never worked, or it keeps
    /// failing.
    pub fn broken(&self) -> bool {
        self.failures > 0 && (!self.worked || self.failures >= GIVE_UP_AFTER)
    }

    fn graph(&mut self, width: u32, height: u32, fourcc: u32) -> Result<&Graph> {
        let key = (width, height, fourcc);
        if self.graph.as_ref().is_none_or(|g| g.key != key) {
            self.graph = None;
            self.graph = Some(self.new_graph(key)?);
            debug!(width, height, "VA-API conversion graph ready");
        }
        Ok(self.graph.as_ref().expect("made above"))
    }

    fn new_graph(&self, key: (u32, u32, u32)) -> Result<Graph> {
        let (width, height, fourcc) = key;
        let sw_format = match fourcc {
            DRM_FORMAT_XRGB8888 => ff::AVPixelFormat::AV_PIX_FMT_BGR0,
            DRM_FORMAT_ARGB8888 => ff::AVPixelFormat::AV_PIX_FMT_BGRA,
            _ => bail!("DRM format {fourcc:08x} not handled"),
        };
        // SAFETY: plain FFmpeg API use; on error `g` frees what was made so far.
        unsafe {
            let mut g = Graph {
                key,
                map_frames: ff::av_hwframe_ctx_alloc(self.device),
                graph: ff::avfilter_graph_alloc(),
                src: ptr::null_mut(),
                sink: ptr::null_mut(),
            };
            if g.map_frames.is_null() || g.graph.is_null() {
                bail!("out of memory");
            }
            // Mapping takes the frame size from this context; no pool (initial size 0).
            let fc = &mut *(*g.map_frames).data.cast::<ff::AVHWFramesContext>();
            fc.format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            fc.sw_format = sw_format;
            fc.width = width as i32;
            fc.height = height as i32;
            fc.initial_pool_size = 0;
            check(ff::av_hwframe_ctx_init(g.map_frames), "VA frames for the DMA-BUFs")?;

            // The source needs its frames context before it is initialised (FFmpeg 8 checks).
            let buffer = ff::avfilter_get_by_name(c"buffer".as_ptr());
            g.src = ff::avfilter_graph_alloc_filter(g.graph, buffer, c"in".as_ptr());
            if g.src.is_null() {
                bail!("no buffer source");
            }
            let par = ff::av_buffersrc_parameters_alloc();
            if par.is_null() {
                bail!("out of memory");
            }
            (*par).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            (*par).width = width as i32;
            (*par).height = height as i32;
            (*par).time_base = ff::AVRational { num: 1, den: 1000 };
            (*par).sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };
            (*par).hw_frames_ctx = g.map_frames;
            let r = ff::av_buffersrc_parameters_set(g.src, par);
            ff::av_free(par.cast());
            check(r, "buffer source parameters")?;
            check(ff::avfilter_init_str(g.src, ptr::null()), "buffer source")?;

            let mut csc = ptr::null_mut();
            check(
                ff::avfilter_graph_create_filter(
                    &mut csc,
                    ff::avfilter_get_by_name(c"scale_vaapi".as_ptr()),
                    c"csc".as_ptr(),
                    c"format=nv12:out_range=full:out_color_matrix=bt709".as_ptr(),
                    ptr::null_mut(),
                    g.graph,
                ),
                "scale_vaapi",
            )?;
            check(
                ff::avfilter_graph_create_filter(
                    &mut g.sink,
                    ff::avfilter_get_by_name(c"buffersink".as_ptr()),
                    c"out".as_ptr(),
                    ptr::null(),
                    ptr::null_mut(),
                    g.graph,
                ),
                "buffer sink",
            )?;
            check(ff::avfilter_link(g.src, 0, csc, 0), "linking the source")?;
            check(ff::avfilter_link(csc, 0, g.sink, 0), "linking the sink")?;
            check(
                ff::avfilter_graph_config(g.graph, ptr::null_mut()),
                "configuring the graph",
            )?;
            Ok(g)
        }
    }

    /// Converts DMA-BUF `fd` (one plane) into an NV12 surface. `None`: no free surface (the
    /// encoder is behind), the frame is dropped. Errors count towards [`VaImporter::broken`].
    #[allow(clippy::too_many_arguments)]
    pub fn convert(
        &mut self,
        fd: i32,
        offset: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
    ) -> Result<Option<VaFrame>> {
        let r = self.try_convert(fd, offset, stride, fourcc, modifier, width, height);
        match &r {
            Ok(Some(_)) => {
                self.worked = true;
                self.failures = 0;
            }
            Ok(None) => {}
            Err(_) => self.failures += 1,
        }
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn try_convert(
        &mut self,
        fd: i32,
        offset: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
    ) -> Result<Option<VaFrame>> {
        let pts = self.pts;
        self.pts += 1;
        let (display, sync) = (self.display, self.sync);
        let g = self.graph(width, height, fourcc)?;
        let (map_frames, src_ctx, sink) = (g.map_frames, g.src, g.sink);
        // SAFETY: plain FFmpeg API use. The descriptor is owned by the source frame's
        // buffer; every frame made here is freed on every path.
        unsafe {
            // SAFETY: an all-zero descriptor is valid (plain integers).
            let mut desc: Box<ff::AVDRMFrameDescriptor> = Box::new(std::mem::zeroed());
            // The size of a DMA-BUF is where its end is.
            let end = libc::lseek(fd, 0, libc::SEEK_END);
            let size = if end > 0 {
                end as usize
            } else {
                offset as usize + stride as usize * height as usize
            };
            desc.nb_objects = 1;
            desc.objects[0].fd = fd;
            desc.objects[0].size = size as _;
            desc.objects[0].format_modifier = modifier;
            desc.nb_layers = 1;
            desc.layers[0].format = fourcc;
            desc.layers[0].nb_planes = 1;
            desc.layers[0].planes[0].object_index = 0;
            desc.layers[0].planes[0].offset = offset as _;
            desc.layers[0].planes[0].pitch = stride as _;

            let mut drm = ff::av_frame_alloc();
            let mut mapped = ff::av_frame_alloc();
            let mut out = ff::av_frame_alloc();
            let free = |a: &mut *mut ff::AVFrame, b: &mut *mut ff::AVFrame| {
                ff::av_frame_free(a);
                ff::av_frame_free(b);
            };
            if drm.is_null() || mapped.is_null() || out.is_null() {
                free(&mut drm, &mut mapped);
                ff::av_frame_free(&mut out);
                bail!("out of memory");
            }
            let raw = Box::into_raw(desc);
            (*drm).buf[0] = ff::av_buffer_create(
                raw.cast(),
                std::mem::size_of::<ff::AVDRMFrameDescriptor>(),
                Some(free_descriptor),
                ptr::null_mut(),
                0,
            );
            if (*drm).buf[0].is_null() {
                drop(Box::from_raw(raw));
                free(&mut drm, &mut mapped);
                ff::av_frame_free(&mut out);
                bail!("out of memory");
            }
            (*drm).data[0] = raw.cast();
            (*drm).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*drm).width = width as i32;
            (*drm).height = height as i32;

            (*mapped).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            (*mapped).hw_frames_ctx = ff::av_buffer_ref(map_frames);
            let r = ff::av_hwframe_map(mapped, drm, 0);
            // The mapping keeps its own reference to the DRM frame.
            ff::av_frame_free(&mut drm);
            if r < 0 {
                free(&mut mapped, &mut out);
                bail!("importing the DMA-BUF into VA-API: {}", averror(r));
            }
            (*mapped).pts = pts;
            // Takes the frame's references (`mapped` is left blank).
            let r = ff::av_buffersrc_add_frame(src_ctx, mapped);
            ff::av_frame_free(&mut mapped);
            if r < 0 {
                ff::av_frame_free(&mut out);
                bail!("VA-API conversion: {}", averror(r));
            }
            let r = ff::av_buffersink_get_frame(sink, out);
            if r == ff::AVERROR(libc::ENOMEM) || r == ff::AVERROR(libc::EAGAIN) {
                // The output pool is empty while the encoder holds its surfaces.
                ff::av_frame_free(&mut out);
                return Ok(None);
            }
            if r < 0 {
                ff::av_frame_free(&mut out);
                bail!("VA-API conversion: {}", averror(r));
            }
            // The video processor may still be reading the DMA-BUF: wait for it before the
            // buffer goes back to Mutter.
            let surface = (*out).data[3] as usize as u32;
            let status = sync(display, surface);
            if status != 0 {
                ff::av_frame_free(&mut out);
                bail!("vaSyncSurface failed ({status})");
            }
            Ok(Some(VaFrame { frame: out }))
        }
    }
}

impl Drop for VaImporter {
    fn drop(&mut self) {
        self.graph = None;
        // SAFETY: created in `new`, released once (frames still in flight hold their own
        // references to the device).
        unsafe { ff::av_buffer_unref(&mut self.device) };
    }
}

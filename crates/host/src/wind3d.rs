//! Zero copy on Windows for AMF and QSV: the duplicated desktop stays in Direct3D 11 and is
//! converted to NV12 there, into the encoder's own FFmpeg frames.
//!
//! - The capture thread copies each desktop image into a texture of a small pool
//!   ([`TexturePool`], a GPU copy) and hands it over as [`crate::frame::FrameData::D3d`]: the
//!   duplicated frame must be released before the next one is acquired.
//! - The encoder ([`D3dFrames`]) wraps the capture's `ID3D11Device` in an FFmpeg
//!   `AV_HWDEVICE_TYPE_D3D11VA` device and allocates its frames there: D3D11 textures for
//!   `h264_amf`/`hevc_amf`, or QSV surfaces of a QSV device derived from it for
//!   `h264_qsv`/`hevc_qsv` (mapped back to their D3D11 texture to be written). The D3D11
//!   video processor (fixed-function on AMD and Intel) converts BGRA to full-range BT.709
//!   NV12 into that texture, padded with black to the coded size, and the frame goes to the
//!   encoder as is. Nothing touches system memory.
//!
//! Adapted from Sunshine `src/video.cpp` (`dxgi_init_avcodec_hardware_input_buffer`: an FFmpeg
//! D3D11VA device around our own device, the QSV device derived from it) and
//! `src/platform/windows/display_vram.cpp` (`d3d_avcodec_encode_device_t`: render-target
//! textures, multithread protection for QSV, mapping QSV frames to D3D11 to write them), at
//! commit 224c6be. Sunshine converts with pixel shaders; here the video processor does it.
//!
//! When anything here fails, [`give_up`] sends the capture back to system memory.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use ffmpeg_sys_next as ff;
use tracing::{info, warn};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_ARRAY_VPOV,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_YCbCrA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2DARRAY, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

/// The encoder takes D3D11 input (AMF or QSV, set at start) and nothing failed yet.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The encoder takes D3D11 frames: the capture should send [`D3dTexture`]s.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// Whether the capture should send [`D3dTexture`]s.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// The D3D11 path failed: the capture goes back to system memory for good.
pub fn give_up() {
    if ENABLED.swap(false, Ordering::SeqCst) {
        warn!("Zero copy to the encoder failed: the desktop goes through system memory");
    }
}

/// Makes the immediate context safe to share between the capture and encoder threads (as
/// FFmpeg's D3D11VA device does when it initialises; QSV needs it from the start).
pub fn protect(device: &ID3D11Device) {
    // SAFETY: plain COM calls on a live device.
    unsafe {
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
}

fn averror(e: i32) -> String {
    let mut buf = [0 as std::ffi::c_char; 128];
    // SAFETY: the buffer is large enough and NUL-terminated by av_strerror.
    unsafe {
        ff::av_strerror(e, buf.as_mut_ptr(), buf.len());
        std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

fn check(e: i32, what: &str) -> Result<i32> {
    if e < 0 {
        bail!("{what}: {}", averror(e));
    }
    Ok(e)
}

// ---------------------------------------------------------------------------------------
// Capture side

/// A copy of the desktop on the GPU (BGRA), owned by a [`TexturePool`].
pub struct D3dTexture {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
}

/// Textures the capture copies the desktop into. One is free when only the pool holds it.
#[derive(Default)]
pub struct TexturePool {
    textures: Vec<Arc<D3dTexture>>,
}

/// The one being filled, the one waiting and the one being encoded.
const POOL_SIZE: usize = 3;

impl TexturePool {
    /// Copies `src` into a free texture; `None` when all are with the encoder.
    pub fn copy(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
    ) -> Result<Option<Arc<D3dTexture>>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain D3D11 call.
        unsafe { src.GetDesc(&mut desc) };
        let (w, h) = (desc.Width, desc.Height);
        self.textures.retain(|t| (t.width, t.height) == (w, h));
        let free = self
            .textures
            .iter()
            .find(|t| Arc::strong_count(t) == 1)
            .cloned();
        let dst = match free {
            Some(t) => t,
            None if self.textures.len() < POOL_SIZE => {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    // Video processor input (shader resource) on any driver.
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                // SAFETY: plain D3D11 call with a complete description.
                unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) }?;
                let t = Arc::new(D3dTexture {
                    texture: t.context("no texture")?,
                    width: w,
                    height: h,
                });
                self.textures.push(Arc::clone(&t));
                t
            }
            None => return Ok(None),
        };
        // SAFETY: GPU copy between textures of the same size and format. The encoder's
        // conversion is queued on the same (multithread-protected) immediate context, after
        // this copy.
        unsafe {
            context.CopyResource(&dst.texture, src);
            context.Flush();
        }
        Ok(Some(dst))
    }
}

// ---------------------------------------------------------------------------------------
// Encoder side

/// The start of FFmpeg's `AVD3D11VADeviceContext` (libavutil/hwcontext_d3d11va.h: these
/// fields in FFmpeg 6.1, 8.0 and 9.0, which appends `BindFlags` and `MiscFlags`); FFmpeg
/// allocates it, and ffmpeg-sys-next does not bind that header.
#[repr(C)]
struct AvD3d11vaDeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
}

/// FFmpeg's `AVD3D11VAFramesContext` (same header).
#[repr(C)]
struct AvD3d11vaFramesContext {
    texture: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
    texture_infos: *mut c_void,
}

/// `AV_HWFRAME_MAP_WRITE | AV_HWFRAME_MAP_OVERWRITE` (an unnamed enum in hwcontext.h).
const MAP_WRITE_OVERWRITE: i32 = 2 | 4;

/// A frame of the encoder's frames context (D3D11 for AMF, QSV for QSV), converted.
pub struct D3dFrame {
    frame: *mut ff::AVFrame,
}

// Reference counted by FFmpeg; used by the encoder thread only.
unsafe impl Send for D3dFrame {}

impl D3dFrame {
    pub fn as_ptr(&self) -> *const ff::AVFrame {
        self.frame
    }
}

impl Drop for D3dFrame {
    fn drop(&mut self) {
        // SAFETY: owned by `self`, freed once.
        unsafe { ff::av_frame_free(&mut self.frame) };
    }
}

/// The video processor for one input and output size.
struct Processor {
    sizes: ((u32, u32), (u32, u32)),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    inputs: HashMap<usize, ID3D11VideoProcessorInputView>,
    outputs: HashMap<(usize, isize), ID3D11VideoProcessorOutputView>,
}

/// The encoder's frames on the capture's device, and the conversion into them.
pub struct D3dFrames {
    device: ID3D11Device,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    /// FFmpeg's D3D11VA device (its lock guards the immediate context).
    d3d11va: *mut ff::AVBufferRef,
    /// The device the frames are on: `d3d11va`, or the QSV device derived from it.
    hw_device: *mut ff::AVBufferRef,
    hw_frames: *mut ff::AVBufferRef,
    qsv: bool,
    width: u32,
    height: u32,
    processor: Option<Processor>,
}

// Raw FFmpeg handles and COM interfaces, used only from the encoder thread.
unsafe impl Send for D3dFrames {}

impl D3dFrames {
    /// Frames of `width`×`height` (the coded size) on the device of `sample` (a texture of
    /// the capture's pool), for QSV (`qsv`) or AMF.
    pub fn new(qsv: bool, sample: &ID3D11Texture2D, width: u32, height: u32) -> Result<Self> {
        // SAFETY: plain COM and FFmpeg API use; every object made is owned by `this` (freed
        // in Drop) or freed here on error.
        unsafe {
            let device = sample.GetDevice()?;
            protect(&device);
            let video_device: ID3D11VideoDevice = device.cast()?;
            let video_context: ID3D11VideoContext = device.GetImmediateContext()?.cast()?;
            let mut this = Self {
                device: device.clone(),
                video_device,
                video_context,
                d3d11va: ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA),
                hw_device: ptr::null_mut(),
                hw_frames: ptr::null_mut(),
                qsv,
                width,
                height,
                processor: None,
            };
            if this.d3d11va.is_null() {
                bail!("out of memory");
            }
            // Our device, with a reference of FFmpeg's own (released when the context goes).
            // The rest (context, video interfaces, a lock) FFmpeg fills in.
            let hw = (*this.d3d11va).data.cast::<ff::AVHWDeviceContext>();
            let d3d = (*hw).hwctx.cast::<AvD3d11vaDeviceContext>();
            (*d3d).device = device.into_raw();
            check(ff::av_hwdevice_ctx_init(this.d3d11va), "FFmpeg D3D11VA device")?;
            if qsv {
                check(
                    ff::av_hwdevice_ctx_create_derived(
                        &mut this.hw_device,
                        ff::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
                        this.d3d11va,
                        0,
                    ),
                    "QSV device on the desktop's GPU",
                )?;
            } else {
                this.hw_device = ff::av_buffer_ref(this.d3d11va);
            }
            this.hw_frames = ff::av_hwframe_ctx_alloc(this.hw_device);
            if this.hw_frames.is_null() {
                bail!("out of memory");
            }
            let fc = &mut *(*this.hw_frames).data.cast::<ff::AVHWFramesContext>();
            fc.format = if qsv {
                ff::AVPixelFormat::AV_PIX_FMT_QSV
            } else {
                ff::AVPixelFormat::AV_PIX_FMT_D3D11
            };
            fc.sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            fc.width = width as i32;
            fc.height = height as i32;
            fc.initial_pool_size = 4;
            if !qsv {
                // The video processor writes through an output view: a render target. (QSV's
                // own textures are render targets by default, `MFX_MEMTYPE_VIDEO_MEMORY_
                // PROCESSOR_TARGET`.)
                let d3d_frames = fc.hwctx.cast::<AvD3d11vaFramesContext>();
                (*d3d_frames).bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32;
                (*d3d_frames).misc_flags = 0;
            }
            check(ff::av_hwframe_ctx_init(this.hw_frames), "encoder frames")?;
            info!(
                width,
                height,
                encoder = if qsv { "QSV" } else { "AMF" },
                "The desktop goes to the encoder without leaving the GPU"
            );
            Ok(this)
        }
    }

    /// Whether these frames suit a texture of `sample`'s device at this coded size.
    pub fn fits(&self, sample: &ID3D11Texture2D, width: u32, height: u32) -> bool {
        // SAFETY: plain COM call.
        let same_device = unsafe { sample.GetDevice() }.is_ok_and(|d| d == self.device);
        same_device && (self.width, self.height) == (width, height)
    }

    /// The frames context the encoder is opened on.
    pub fn frames_ctx(&self) -> *mut ff::AVBufferRef {
        self.hw_frames
    }

    /// Its data, which identifies it.
    pub fn frames_data(&self) -> *const u8 {
        // SAFETY: a valid buffer reference owned by `self`.
        unsafe { (*self.hw_frames).data }
    }

    fn processor(&mut self, input: (u32, u32)) -> Result<&mut Processor> {
        let output = (self.width, self.height);
        if self
            .processor
            .as_ref()
            .is_none_or(|p| p.sizes != (input, output))
        {
            self.processor = None;
            let rate = DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: output.0,
                OutputHeight: output.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            // SAFETY: plain D3D11 video calls on live interfaces; the rectangles and colour
            // values outlive the calls.
            unsafe {
                let enumerator = self.video_device.CreateVideoProcessorEnumerator(&content)?;
                let processor = self.video_device.CreateVideoProcessor(&enumerator, 0)?;
                let vc = &self.video_context;
                vc.VideoProcessorSetStreamFrameFormat(
                    &processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                vc.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
                // Full-range BT.709, as the VUI says (ffenc.rs).
                if let Ok(vc1) = vc.cast::<ID3D11VideoContext1>() {
                    vc1.VideoProcessorSetStreamColorSpace1(
                        &processor,
                        0,
                        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    );
                    vc1.VideoProcessorSetOutputColorSpace1(
                        &processor,
                        DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
                    );
                } else {
                    // D3D11_VIDEO_PROCESSOR_COLOR_SPACE bits: RGB_Range (1) 0 = 0-255;
                    // YCbCr_Matrix (2) 1 = BT.709; Nominal_Range (4-5) 2 = 0-255.
                    let rgb = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                    let yuv = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                        _bitfield: (1 << 2) | (2 << 4),
                    };
                    vc.VideoProcessorSetStreamColorSpace(&processor, 0, &rgb);
                    vc.VideoProcessorSetOutputColorSpace(&processor, &yuv);
                }
                // The picture at the top left, black (full-range) in the padding.
                let picture = RECT {
                    left: 0,
                    top: 0,
                    right: input.0 as i32,
                    bottom: input.1 as i32,
                };
                let target = RECT {
                    left: 0,
                    top: 0,
                    right: output.0 as i32,
                    bottom: output.1 as i32,
                };
                vc.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&picture));
                vc.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&picture));
                vc.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
                let black = D3D11_VIDEO_COLOR {
                    Anonymous: D3D11_VIDEO_COLOR_0 {
                        YCbCr: D3D11_VIDEO_COLOR_YCbCrA {
                            Y: 0.0,
                            Cb: 0.5,
                            Cr: 0.5,
                            A: 1.0,
                        },
                    },
                };
                vc.VideoProcessorSetOutputBackgroundColor(&processor, true, &black);
                self.processor = Some(Processor {
                    sizes: (input, output),
                    enumerator,
                    processor,
                    inputs: HashMap::new(),
                    outputs: HashMap::new(),
                });
            }
        }
        Ok(self.processor.as_mut().expect("made above"))
    }

    /// Converts `src` (a [`D3dTexture`] of the capture) into a new frame for the encoder.
    pub fn convert(&mut self, src: &D3dTexture) -> Result<D3dFrame> {
        let hw_frames = self.hw_frames;
        let qsv = self.qsv;
        // SAFETY: plain FFmpeg API use; every frame made here is freed on every path, the
        // returned one by its owner.
        let (mut frame, texture, index) = unsafe {
            let mut frame = ff::av_frame_alloc();
            if frame.is_null() {
                bail!("out of memory");
            }
            let r = ff::av_hwframe_get_buffer(hw_frames, frame, 0);
            if r < 0 {
                ff::av_frame_free(&mut frame);
                bail!("encoder frame: {}", averror(r));
            }
            // A QSV surface is written through the D3D11 texture behind it.
            let mut mapped = ptr::null_mut();
            let view = if qsv {
                mapped = ff::av_frame_alloc();
                if mapped.is_null() {
                    ff::av_frame_free(&mut frame);
                    bail!("out of memory");
                }
                (*mapped).format = ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32;
                let r = ff::av_hwframe_map(mapped, frame, MAP_WRITE_OVERWRITE);
                if r < 0 {
                    ff::av_frame_free(&mut mapped);
                    ff::av_frame_free(&mut frame);
                    bail!("mapping the QSV surface to D3D11: {}", averror(r));
                }
                mapped
            } else {
                frame
            };
            // D3D11 frames: data[0] the texture, data[1] its index in an array.
            let texture = (*view).data[0] as *mut c_void;
            let index = (*view).data[1] as isize;
            // The mapping only borrows the surface's texture, which the surface keeps alive.
            if !mapped.is_null() {
                ff::av_frame_free(&mut mapped);
            }
            (frame, texture, index)
        };
        let r = self.blit(src, texture, index);
        if let Err(e) = r {
            // SAFETY: made above, not handed out.
            unsafe { ff::av_frame_free(&mut frame) };
            return Err(e);
        }
        Ok(D3dFrame { frame })
    }

    /// The video processor pass from `src` into slice `index` of `texture`.
    fn blit(&mut self, src: &D3dTexture, texture: *mut c_void, index: isize) -> Result<()> {
        let device = self.video_device.clone();
        let video_context = self.video_context.clone();
        // SAFETY: plain D3D11 call.
        let immediate = unsafe { self.device.GetImmediateContext() }.ok();
        // SAFETY: the context is FFmpeg's, created in `new`.
        let lock = unsafe {
            let hw = (*self.d3d11va).data.cast::<ff::AVHWDeviceContext>();
            let d3d = &*(*hw).hwctx.cast::<AvD3d11vaDeviceContext>();
            (d3d.lock, d3d.unlock, d3d.lock_ctx)
        };
        let p = self.processor((src.width, src.height))?;
        // SAFETY: `texture` is a live ID3D11Texture2D of the frame being written (borrowed,
        // not released here); views keep their own references. The lock is FFmpeg's, around
        // the use of the shared immediate context.
        unsafe {
            let target = ID3D11Texture2D::from_raw_borrowed(&texture).context("no texture")?;
            let input = src.texture.as_raw() as usize;
            if !p.inputs.contains_key(&input) {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                };
                let mut view = None;
                device.CreateVideoProcessorInputView(
                    &src.texture,
                    &p.enumerator,
                    &desc,
                    Some(&mut view),
                )?;
                p.inputs.insert(input, view.context("no input view")?);
            }
            let output = (texture as usize, index);
            if !p.outputs.contains_key(&output) {
                let desc = if index == 0 {
                    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    }
                } else {
                    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2DArray: D3D11_TEX2D_ARRAY_VPOV {
                                MipSlice: 0,
                                FirstArraySlice: index as u32,
                                ArraySize: 1,
                            },
                        },
                    }
                };
                let mut view = None;
                device.CreateVideoProcessorOutputView(
                    target,
                    &p.enumerator,
                    &desc,
                    Some(&mut view),
                )?;
                p.outputs.insert(output, view.context("no output view")?);
            }
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: ptr::null_mut(),
                pInputSurface: ManuallyDrop::new(p.inputs.get(&input).cloned()),
                ppFutureSurfaces: ptr::null_mut(),
                ppPastSurfacesRight: ptr::null_mut(),
                pInputSurfaceRight: ManuallyDrop::new(None),
                ppFutureSurfacesRight: ptr::null_mut(),
            };
            let mut streams = [stream];
            let (lock, unlock, lock_ctx) = lock;
            if let Some(l) = lock {
                l(lock_ctx);
            }
            let r = video_context.VideoProcessorBlt(&p.processor, &p.outputs[&output], 0, &streams);
            // Submitted now, ahead of the encoder's own work on the frame.
            if r.is_ok()
                && let Some(ctx) = &immediate
            {
                ctx.Flush();
            }
            if let Some(u) = unlock {
                u(lock_ctx);
            }
            // The stream held a reference to the input view.
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            r?;
        }
        Ok(())
    }
}

impl Drop for D3dFrames {
    fn drop(&mut self) {
        self.processor = None;
        // SAFETY: made in `new`, each released once (frames in flight hold their own
        // references to the contexts).
        unsafe {
            ff::av_buffer_unref(&mut self.hw_frames);
            ff::av_buffer_unref(&mut self.hw_device);
            ff::av_buffer_unref(&mut self.d3d11va);
        }
    }
}

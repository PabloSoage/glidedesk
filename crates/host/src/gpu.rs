//! Zero-copy path: PipeWire DMA-BUF → EGLImage → GL texture → CUDA → NVENC input buffer.
//!
//! Mutter renders the virtual monitor into DMA-BUFs on its GPU (block-linear NVIDIA
//! layouts). Each buffer is imported once as an EGLImage and bound to a GL texture. Per frame,
//! a GPU blit copies it into a texture we own, which is registered with CUDA (CUDA cannot
//! register a DMA-BUF EGLImage directly on desktop GPUs), and a device-to-device copy puts it
//! in a pitched buffer from a small pool for NVENC. All of it happens on the PipeWire thread
//! before the buffer goes back to Mutter, so Mutter renders the next frame while the encoder
//! works on the copy. Nothing touches system memory.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::fd::RawFd;
use std::ptr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use cudarc::driver::{CudaContext, sys as cu};
use tracing::{debug, info};

// EGL types and constants (EGL 1.5, EGL_EXT_device_*, EGL_EXT_image_dma_buf_import*,
// EGL_KHR_no_config_context, EGL_KHR_surfaceless_context).
type EGLDisplay = *mut c_void;
type EGLDeviceEXT = *mut c_void;
type EGLContext = *mut c_void;
type EGLImage = *mut c_void;
type EGLint = i32;
type EGLBoolean = u32;

const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_DRM_RENDER_NODE_FILE_EXT: EGLint = 0x3377;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_HEIGHT: EGLint = 0x3056;
const EGL_IMAGE_PRESERVED_KHR: EGLint = 0x30D2;
const EGL_TRUE: EGLint = 1;
const EGL_NONE: EGLint = 0x3038;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_CONTEXT_MAJOR_VERSION: EGLint = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: EGLint = 0x30FB;

// GL constants.
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA8: u32 = 0x8058;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_NEAREST: i32 = 0x2600;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;

/// DRM fourcc of PipeWire's BGRx / BGRA (little-endian XRGB8888 / ARGB8888).
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;

/// EGL and GL entry points, resolved through `eglGetProcAddress`.
#[allow(clippy::type_complexity)]
struct Gl {
    initialize: unsafe extern "C" fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean,
    get_error: unsafe extern "C" fn() -> EGLint,
    bind_api: unsafe extern "C" fn(u32) -> EGLBoolean,
    create_context:
        unsafe extern "C" fn(EGLDisplay, *mut c_void, EGLContext, *const EGLint) -> EGLContext,
    make_current:
        unsafe extern "C" fn(EGLDisplay, *mut c_void, *mut c_void, EGLContext) -> EGLBoolean,
    query_devices: unsafe extern "C" fn(EGLint, *mut EGLDeviceEXT, *mut EGLint) -> EGLBoolean,
    query_device_string: unsafe extern "C" fn(EGLDeviceEXT, EGLint) -> *const c_char,
    get_platform_display: unsafe extern "C" fn(u32, *mut c_void, *const EGLint) -> EGLDisplay,
    query_modifiers: unsafe extern "C" fn(
        EGLDisplay,
        EGLint,
        EGLint,
        *mut u64,
        *mut EGLBoolean,
        *mut EGLint,
    ) -> EGLBoolean,
    create_image:
        unsafe extern "C" fn(EGLDisplay, EGLContext, u32, *mut c_void, *const EGLint) -> EGLImage,
    destroy_image: unsafe extern "C" fn(EGLDisplay, EGLImage) -> EGLBoolean,
    image_target_texture: unsafe extern "C" fn(u32, EGLImage),
    gen_textures: unsafe extern "C" fn(i32, *mut u32),
    delete_textures: unsafe extern "C" fn(i32, *const u32),
    bind_texture: unsafe extern "C" fn(u32, u32),
    tex_parameteri: unsafe extern "C" fn(u32, u32, i32),
    tex_storage_2d: unsafe extern "C" fn(u32, i32, u32, i32, i32),
    gen_framebuffers: unsafe extern "C" fn(i32, *mut u32),
    delete_framebuffers: unsafe extern "C" fn(i32, *const u32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    framebuffer_texture_2d: unsafe extern "C" fn(u32, u32, u32, u32, i32),
    check_framebuffer_status: unsafe extern "C" fn(u32) -> u32,
    blit_framebuffer: unsafe extern "C" fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32),
    finish: unsafe extern "C" fn(),
}

#[allow(clippy::missing_transmute_annotations)]
impl Gl {
    /// # Safety
    /// `gpa` must be `eglGetProcAddress`.
    unsafe fn load(gpa: GetProcAddress) -> Result<Self> {
        macro_rules! f {
            ($name:literal) => {{
                let c = CString::new($name).unwrap();
                // SAFETY: eglGetProcAddress with a NUL-terminated name.
                let p = unsafe { gpa(c.as_ptr()) };
                if p.is_null() {
                    bail!("{} not available", $name);
                }
                // SAFETY: the field type matches the C prototype of this entry point.
                unsafe { std::mem::transmute::<*mut c_void, _>(p) }
            }};
        }
        Ok(Self {
            initialize: f!("eglInitialize"),
            get_error: f!("eglGetError"),
            bind_api: f!("eglBindAPI"),
            create_context: f!("eglCreateContext"),
            make_current: f!("eglMakeCurrent"),
            query_devices: f!("eglQueryDevicesEXT"),
            query_device_string: f!("eglQueryDeviceStringEXT"),
            get_platform_display: f!("eglGetPlatformDisplayEXT"),
            query_modifiers: f!("eglQueryDmaBufModifiersEXT"),
            create_image: f!("eglCreateImageKHR"),
            destroy_image: f!("eglDestroyImageKHR"),
            image_target_texture: f!("glEGLImageTargetTexture2DOES"),
            gen_textures: f!("glGenTextures"),
            delete_textures: f!("glDeleteTextures"),
            bind_texture: f!("glBindTexture"),
            tex_parameteri: f!("glTexParameteri"),
            tex_storage_2d: f!("glTexStorage2D"),
            gen_framebuffers: f!("glGenFramebuffers"),
            delete_framebuffers: f!("glDeleteFramebuffers"),
            bind_framebuffer: f!("glBindFramebuffer"),
            framebuffer_texture_2d: f!("glFramebufferTexture2D"),
            check_framebuffer_status: f!("glCheckFramebufferStatus"),
            blit_framebuffer: f!("glBlitFramebuffer"),
            finish: f!("glFinish"),
        })
    }
}

type RegisterGlImage =
    unsafe extern "C" fn(*mut cu::CUgraphicsResource, u32, u32, u32) -> cu::CUresult;
const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: u32 = 1;

fn cu_check(r: cu::CUresult, what: &str) -> Result<()> {
    if r != cu::CUresult::CUDA_SUCCESS {
        bail!("{what} failed: {r:?}");
    }
    Ok(())
}

/// Byte order of the pixels in a [`DeviceBuffer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    /// B, G, R, X (PipeWire BGRx, NVENC `ARGB`).
    Bgrx,
    /// R, G, B, A (GL RGBA8, NVENC `ABGR`).
    Rgba,
}

/// A pitched 32-bit buffer on the GPU. Owned by a [`Pool`]; a frame holds an `Arc` to it
/// while the encoder uses it.
pub struct DeviceBuffer {
    pub ptr: cu::CUdeviceptr,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
    pub order: PixelOrder,
    ctx: Arc<CudaContext>,
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        let _ = self.ctx.bind_to_thread();
        // SAFETY: allocated with cuMemAllocPitch by the pool, freed once.
        unsafe {
            cu::cuMemFree_v2(self.ptr);
        }
    }
}

/// Small set of device buffers. A buffer is free when only the pool holds it.
pub struct Pool {
    ctx: Arc<CudaContext>,
    buffers: Vec<Arc<DeviceBuffer>>,
    size: (u32, u32),
}

const POOL_SIZE: usize = 3;

impl Pool {
    pub fn new(ctx: Arc<CudaContext>) -> Self {
        Self {
            ctx,
            buffers: Vec::new(),
            size: (0, 0),
        }
    }

    /// A free buffer of `w`×`h`, or `None` when all are in use (the frame is then dropped).
    pub fn get(&mut self, w: u32, h: u32, order: PixelOrder) -> Result<Option<Arc<DeviceBuffer>>> {
        if self.size != (w, h) {
            self.buffers.clear();
            self.size = (w, h);
        }
        if let Some(b) = self.buffers.iter_mut().find(|b| Arc::strong_count(b) == 1) {
            if let Some(inner) = Arc::get_mut(b) {
                inner.order = order;
            }
            return Ok(Some(Arc::clone(b)));
        }
        if self.buffers.len() >= POOL_SIZE {
            return Ok(None);
        }
        self.ctx.bind_to_thread()?;
        let (mut ptr, mut pitch) = (0, 0);
        // SAFETY: the context is current on this thread.
        cu_check(
            unsafe { cu::cuMemAllocPitch_v2(&mut ptr, &mut pitch, w as usize * 4, h as usize, 16) },
            "cuMemAllocPitch",
        )?;
        let b = Arc::new(DeviceBuffer {
            ptr,
            pitch,
            width: w,
            height: h,
            order,
            ctx: Arc::clone(&self.ctx),
        });
        self.buffers.push(Arc::clone(&b));
        Ok(Some(b))
    }
}

/// An imported DMA-BUF: its EGLImage and the GL texture bound to it.
struct Imported {
    image: EGLImage,
    texture: u32,
}

/// The texture CUDA reads from, at the current frame size.
struct Staging {
    texture: u32,
    resource: cu::CUgraphicsResource,
    width: u32,
    height: u32,
}

/// EGL display and GL context on the CUDA device's GPU, plus the import cache.
pub struct DmabufImporter {
    _egl: libloading::Library,
    _cuda: libloading::Library,
    gl: Gl,
    display: EGLDisplay,
    context: EGLContext,
    register_gl_image: RegisterGlImage,
    ctx: Arc<CudaContext>,
    cache: HashMap<(RawFd, u64, u32, u32), Imported>,
    read_fbo: u32,
    draw_fbo: u32,
    staging: Option<Staging>,
}

// Raw EGL/GL/CUDA handles, used only from the thread that created them.
unsafe impl Send for DmabufImporter {}

/// Render node (`/dev/dri/renderD*`) of CUDA device `ctx`, from its PCI bus id.
fn render_node_of(ctx: &CudaContext) -> Result<String> {
    let mut dev: cu::CUdevice = 0;
    let mut bus = [0 as c_char; 32];
    // SAFETY: buffer larger than the documented minimum (13 bytes).
    unsafe {
        cu_check(
            cu::cuDeviceGet(&mut dev, ctx.ordinal() as i32),
            "cuDeviceGet",
        )?;
        cu_check(
            cu::cuDeviceGetPCIBusId(bus.as_mut_ptr(), bus.len() as i32, dev),
            "cuDeviceGetPCIBusId",
        )?;
    }
    // SAFETY: NUL-terminated by the driver.
    let bus = unsafe { CStr::from_ptr(bus.as_ptr()) }
        .to_string_lossy()
        .to_lowercase();
    let link = format!("/dev/dri/by-path/pci-{bus}-render");
    let node = std::fs::canonicalize(&link).with_context(|| format!("resolving {link}"))?;
    Ok(node.to_string_lossy().into_owned())
}

impl DmabufImporter {
    /// Creates the EGL display and GL context on `ctx`'s GPU and makes the context current
    /// on the calling thread (the PipeWire thread).
    pub fn new(ctx: Arc<CudaContext>) -> Result<Self> {
        let render_node = render_node_of(&ctx)?;
        // SAFETY: loading system libraries; entry points follow the EGL, GL and CUDA headers.
        unsafe {
            let egl = libloading::Library::new("libEGL.so.1").context("loading libEGL.so.1")?;
            let cuda = libloading::Library::new("libcuda.so.1").context("loading libcuda.so.1")?;
            let gpa: GetProcAddress = *egl.get::<GetProcAddress>(b"eglGetProcAddress\0")?;
            let gl = Gl::load(gpa)?;
            let register_gl_image: RegisterGlImage =
                *cuda.get::<RegisterGlImage>(b"cuGraphicsGLRegisterImage\0")?;

            let mut devices = [ptr::null_mut(); 16];
            let mut n = 0;
            if (gl.query_devices)(devices.len() as i32, devices.as_mut_ptr(), &mut n) == 0 {
                bail!("eglQueryDevicesEXT failed");
            }
            let device = devices[..n as usize]
                .iter()
                .copied()
                .find(|&d| {
                    let s = (gl.query_device_string)(d, EGL_DRM_RENDER_NODE_FILE_EXT);
                    !s.is_null() && CStr::from_ptr(s).to_string_lossy() == render_node
                })
                .with_context(|| format!("no EGL device for {render_node}"))?;
            let display = (gl.get_platform_display)(EGL_PLATFORM_DEVICE_EXT, device, ptr::null());
            if display.is_null() || (gl.initialize)(display, ptr::null_mut(), ptr::null_mut()) == 0
            {
                bail!(
                    "initialising the EGL device display failed (0x{:x})",
                    (gl.get_error)()
                );
            }
            (gl.bind_api)(EGL_OPENGL_API);
            let attrs = [
                EGL_CONTEXT_MAJOR_VERSION,
                3,
                EGL_CONTEXT_MINOR_VERSION,
                3,
                EGL_NONE,
            ];
            let context =
                (gl.create_context)(display, ptr::null_mut(), ptr::null_mut(), attrs.as_ptr());
            if context.is_null() {
                bail!("eglCreateContext failed (0x{:x})", (gl.get_error)());
            }
            if (gl.make_current)(display, ptr::null_mut(), ptr::null_mut(), context) == 0 {
                bail!("eglMakeCurrent failed (0x{:x})", (gl.get_error)());
            }
            let mut fbos = [0u32; 2];
            (gl.gen_framebuffers)(2, fbos.as_mut_ptr());
            info!(%render_node, "EGL/GL context ready for DMA-BUF import");
            Ok(Self {
                _egl: egl,
                _cuda: cuda,
                gl,
                display,
                context,
                register_gl_image,
                ctx,
                cache: HashMap::new(),
                read_fbo: fbos[0],
                draw_fbo: fbos[1],
                staging: None,
            })
        }
    }

    /// Modifiers the GPU can import for `fourcc` (render-capable ones only).
    pub fn modifiers(&self, fourcc: u32) -> Vec<u64> {
        let mut n = 0;
        // SAFETY: two-call pattern of eglQueryDmaBufModifiersEXT.
        unsafe {
            let q = self.gl.query_modifiers;
            if q(
                self.display,
                fourcc as i32,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut n,
            ) == 0
            {
                return Vec::new();
            }
            let mut mods = vec![0u64; n as usize];
            let mut external = vec![0u32; n as usize];
            q(
                self.display,
                fourcc as i32,
                n,
                mods.as_mut_ptr(),
                external.as_mut_ptr(),
                &mut n,
            );
            mods.into_iter()
                .zip(external)
                .filter(|(_, ext)| *ext == 0)
                .map(|(m, _)| m)
                .collect()
        }
    }

    /// Forgets imported buffers (after a renegotiation PipeWire hands out new ones).
    pub fn clear(&mut self) {
        for (_, imp) in self.cache.drain() {
            // SAFETY: each texture and image is released once, with our context current.
            unsafe {
                (self.gl.delete_textures)(1, &imp.texture);
                (self.gl.destroy_image)(self.display, imp.image);
            }
        }
    }

    fn staging(&mut self, width: u32, height: u32) -> Result<&Staging> {
        if self
            .staging
            .as_ref()
            .is_none_or(|s| (s.width, s.height) != (width, height))
        {
            self.drop_staging();
            let mut texture = 0;
            let mut resource: cu::CUgraphicsResource = ptr::null_mut();
            // SAFETY: GL context current; immutable RGBA8 storage, then CUDA registration.
            unsafe {
                (self.gl.gen_textures)(1, &mut texture);
                (self.gl.bind_texture)(GL_TEXTURE_2D, texture);
                (self.gl.tex_storage_2d)(GL_TEXTURE_2D, 1, GL_RGBA8, width as i32, height as i32);
                (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
                (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
                self.ctx.bind_to_thread()?;
                cu_check(
                    (self.register_gl_image)(
                        &mut resource,
                        texture,
                        GL_TEXTURE_2D,
                        CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
                    ),
                    "cuGraphicsGLRegisterImage",
                )?;
            }
            self.staging = Some(Staging {
                texture,
                resource,
                width,
                height,
            });
        }
        Ok(self.staging.as_ref().expect("set above"))
    }

    fn drop_staging(&mut self) {
        if let Some(s) = self.staging.take() {
            // SAFETY: registered resource and texture released once.
            unsafe {
                cu::cuGraphicsUnregisterResource(s.resource);
                (self.gl.delete_textures)(1, &s.texture);
            }
        }
    }

    /// Copies DMA-BUF `fd` (one plane, `fourcc`, `modifier`) into `dst` (RGBA order).
    #[allow(clippy::too_many_arguments)]
    pub fn copy(
        &mut self,
        fd: RawFd,
        offset: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
        dst: &DeviceBuffer,
    ) -> Result<()> {
        let key = (fd, modifier, width, height);
        if !self.cache.contains_key(&key) {
            let mut attrs = vec![
                EGL_WIDTH,
                width as i32,
                EGL_HEIGHT,
                height as i32,
                EGL_LINUX_DRM_FOURCC_EXT,
                fourcc as i32,
                EGL_DMA_BUF_PLANE0_FD_EXT,
                fd,
                EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                offset as i32,
                EGL_DMA_BUF_PLANE0_PITCH_EXT,
                stride as i32,
            ];
            if modifier != DRM_FORMAT_MOD_INVALID {
                attrs.extend_from_slice(&[
                    EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                    (modifier & 0xffff_ffff) as u32 as i32,
                    EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                    (modifier >> 32) as u32 as i32,
                ]);
            }
            attrs.extend_from_slice(&[EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE]);
            // SAFETY: valid display, attribute list terminated with EGL_NONE, context current.
            let (image, texture) = unsafe {
                let image = (self.gl.create_image)(
                    self.display,
                    ptr::null_mut(),
                    EGL_LINUX_DMA_BUF_EXT,
                    ptr::null_mut(),
                    attrs.as_ptr(),
                );
                if image.is_null() {
                    bail!(
                        "eglCreateImage for the DMA-BUF failed (0x{:x})",
                        (self.gl.get_error)()
                    );
                }
                let mut texture = 0;
                (self.gl.gen_textures)(1, &mut texture);
                (self.gl.bind_texture)(GL_TEXTURE_2D, texture);
                (self.gl.image_target_texture)(GL_TEXTURE_2D, image);
                (image, texture)
            };
            debug!(
                fd,
                modifier = format!("0x{modifier:x}"),
                width,
                height,
                "Imported DMA-BUF"
            );
            self.cache.insert(key, Imported { image, texture });
        }
        let src_tex = self.cache[&key].texture;
        let (read_fbo, draw_fbo) = (self.read_fbo, self.draw_fbo);
        let staging_tex = self.staging(width, height)?.texture;
        let gl = &self.gl;
        // SAFETY: GL context current; both textures have width×height storage.
        unsafe {
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, read_fbo);
            (gl.framebuffer_texture_2d)(
                GL_READ_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                src_tex,
                0,
            );
            (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, draw_fbo);
            (gl.framebuffer_texture_2d)(
                GL_DRAW_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                staging_tex,
                0,
            );
            if (gl.check_framebuffer_status)(GL_READ_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE
                || (gl.check_framebuffer_status)(GL_DRAW_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE
            {
                bail!("framebuffer for the DMA-BUF blit is incomplete");
            }
            let (w, h) = (width as i32, height as i32);
            (gl.blit_framebuffer)(
                0,
                0,
                w,
                h,
                0,
                0,
                w,
                h,
                GL_COLOR_BUFFER_BIT,
                GL_NEAREST as u32,
            );
            (gl.finish)();
        }

        self.ctx.bind_to_thread()?;
        let mut res = self.staging.as_ref().expect("created above").resource;
        // SAFETY: registered resource; mapped and unmapped around the copy on the null stream.
        unsafe {
            cu_check(
                cu::cuGraphicsMapResources(1, &mut res, ptr::null_mut()),
                "cuGraphicsMapResources",
            )?;
            let mut array: cu::CUarray = ptr::null_mut();
            let r = cu::cuGraphicsSubResourceGetMappedArray(&mut array, res, 0, 0);
            let copied = if r != cu::CUresult::CUDA_SUCCESS {
                Err(anyhow::anyhow!(
                    "cuGraphicsSubResourceGetMappedArray failed: {r:?}"
                ))
            } else {
                let copy = cu::CUDA_MEMCPY2D {
                    srcXInBytes: 0,
                    srcY: 0,
                    srcMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_ARRAY,
                    srcHost: ptr::null(),
                    srcDevice: 0,
                    srcArray: array,
                    srcPitch: 0,
                    dstXInBytes: 0,
                    dstY: 0,
                    dstMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_DEVICE,
                    dstHost: ptr::null_mut(),
                    dstDevice: dst.ptr,
                    dstArray: ptr::null_mut(),
                    dstPitch: dst.pitch,
                    WidthInBytes: width as usize * 4,
                    Height: height as usize,
                };
                cu_check(cu::cuMemcpy2D_v2(&copy), "cuMemcpy2D (DMA-BUF)")
            };
            cu_check(
                cu::cuGraphicsUnmapResources(1, &mut res, ptr::null_mut()),
                "cuGraphicsUnmapResources",
            )?;
            copied
        }
    }
}

impl Drop for DmabufImporter {
    fn drop(&mut self) {
        self.clear();
        self.drop_staging();
        let fbos = [self.read_fbo, self.draw_fbo];
        // SAFETY: our framebuffers; the context is released with the display at exit.
        unsafe {
            (self.gl.delete_framebuffers)(2, fbos.as_ptr());
            (self.gl.make_current)(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
        }
        let _ = self.context;
    }
}

/// Copies a BGRx frame from system memory into a pooled device buffer.
pub fn upload(
    pool: &mut Pool,
    data: &[u8],
    stride: usize,
    width: u32,
    height: u32,
) -> Result<Arc<DeviceBuffer>> {
    let dst = pool
        .get(width, height, PixelOrder::Bgrx)?
        .context("no free upload buffer")?;
    let (w, h) = (width as usize, height as usize);
    if data.len() < stride * (h - 1) + w * 4 {
        bail!("frame buffer too small for {w}x{h}");
    }
    pool.ctx.bind_to_thread()?;
    let copy = cu::CUDA_MEMCPY2D {
        srcXInBytes: 0,
        srcY: 0,
        srcMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_HOST,
        srcHost: data.as_ptr() as *const c_void,
        srcDevice: 0,
        srcArray: ptr::null_mut(),
        srcPitch: stride,
        dstXInBytes: 0,
        dstY: 0,
        dstMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_DEVICE,
        dstHost: ptr::null_mut(),
        dstDevice: dst.ptr,
        dstArray: ptr::null_mut(),
        dstPitch: dst.pitch,
        WidthInBytes: w * 4,
        Height: h,
    };
    // SAFETY: source covers h rows of `stride` bytes; destination was allocated for it.
    cu_check(unsafe { cu::cuMemcpy2D_v2(&copy) }, "cuMemcpy2D (upload)")?;
    Ok(dst)
}

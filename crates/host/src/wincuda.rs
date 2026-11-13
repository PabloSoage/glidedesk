//! Desktop frames straight from Direct3D to CUDA, when the duplicated desktop lives on the
//! encoder's NVIDIA GPU: the frame is copied on the GPU into a texture registered with CUDA,
//! then into a pitched buffer for NVENC. No system memory and no PCIe upload (at 4K the
//! system-memory path costs ~14 ms of read and ~10 ms of upload per frame).

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use cudarc::driver::{CudaContext, sys as cu};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter1;
use windows::core::Interface;

use crate::gpu::{DeviceBuffer, PixelOrder, Pool};

type RegisterD3d11 =
    unsafe extern "system" fn(*mut cu::CUgraphicsResource, *mut c_void, u32) -> cu::CUresult;
type D3d11GetDevice = unsafe extern "system" fn(*mut cu::CUdevice, *mut c_void) -> cu::CUresult;

fn cu_check(r: cu::CUresult, what: &str) -> Result<()> {
    if r != cu::CUresult::CUDA_SUCCESS {
        bail!("{what} failed: {r:?}");
    }
    Ok(())
}

/// Our copy of the desktop texture, registered with CUDA, and its size.
struct Registered {
    texture: ID3D11Texture2D,
    resource: cu::CUgraphicsResource,
    width: u32,
    height: u32,
}

pub struct D3dToCuda {
    ctx: Arc<CudaContext>,
    register: RegisterD3d11,
    staging: Option<Registered>,
    pool: Pool,
    _nvcuda: libloading::Library,
}

// Raw CUDA handles, used only from the capture thread.
unsafe impl Send for D3dToCuda {}

impl D3dToCuda {
    /// Whether `adapter` is `ctx`'s GPU; if it is, the importer for it.
    pub fn new(ctx: Arc<CudaContext>, adapter: &IDXGIAdapter1) -> Result<Self> {
        // SAFETY: loading the CUDA driver; entry points follow cudaD3D11.h.
        unsafe {
            let nvcuda = libloading::Library::new("nvcuda.dll").context("loading nvcuda.dll")?;
            let get_device: D3d11GetDevice = *nvcuda.get(b"cuD3D11GetDevice\0")?;
            let register: RegisterD3d11 = *nvcuda.get(b"cuGraphicsD3D11RegisterResource\0")?;
            ctx.bind_to_thread()?;
            let mut dev: cu::CUdevice = 0;
            cu_check(get_device(&mut dev, adapter.as_raw()), "cuD3D11GetDevice")
                .context("the desktop is not on a CUDA GPU")?;
            if dev != ctx.cu_device() {
                bail!(
                    "the desktop is on CUDA device {dev}, the encoder on {}",
                    ctx.ordinal()
                );
            }
            Ok(Self {
                pool: Pool::new(Arc::clone(&ctx)),
                ctx,
                register,
                staging: None,
                _nvcuda: nvcuda,
            })
        }
    }

    fn drop_staging(&mut self) {
        if let Some(s) = self.staging.take() {
            let _ = self.ctx.bind_to_thread();
            // SAFETY: registered once, unregistered once, before the texture goes.
            unsafe {
                cu::cuGraphicsUnregisterResource(s.resource);
            }
        }
    }

    /// Copies `tex` (BGRA, the duplicated desktop) into a pooled device buffer. `None` when
    /// all buffers are with the encoder (the frame is skipped).
    pub fn copy(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
    ) -> Result<Option<Arc<DeviceBuffer>>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain D3D11 call.
        unsafe { tex.GetDesc(&mut desc) };
        let (w, h) = (desc.Width, desc.Height);
        if !matches!(&self.staging, Some(s) if (s.width, s.height) == (w, h)) {
            self.drop_staging();
            self.staging = Some(self.register_texture(device, w, h)?);
        }
        let Some(dst) = self.pool.get(w, h, PixelOrder::Bgrx)? else {
            return Ok(None);
        };
        let s = self.staging.as_ref().expect("created above");
        // SAFETY: GPU copy between textures of the same size and format; mapping the
        // resource in CUDA waits for the D3D11 work submitted before it.
        unsafe {
            context.CopyResource(&s.texture, tex);
            context.Flush();
        }
        self.ctx.bind_to_thread()?;
        let mut res = s.resource;
        // SAFETY: registered resource; mapped and unmapped around the copy on the null stream.
        unsafe {
            cu_check(
                cu::cuGraphicsMapResources(1, &mut res, ptr::null_mut()),
                "cuGraphicsMapResources",
            )?;
            let mut array: cu::CUarray = ptr::null_mut();
            let r = cu::cuGraphicsSubResourceGetMappedArray(&mut array, res, 0, 0);
            let copied = cu_check(r, "cuGraphicsSubResourceGetMappedArray").and_then(|()| {
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
                    WidthInBytes: w as usize * 4,
                    Height: h as usize,
                };
                cu_check(cu::cuMemcpy2D_v2(&copy), "cuMemcpy2D (D3D11)")
            });
            cu_check(
                cu::cuGraphicsUnmapResources(1, &mut res, ptr::null_mut()),
                "cuGraphicsUnmapResources",
            )?;
            copied?;
        }
        Ok(Some(dst))
    }

    fn register_texture(&self, device: &ID3D11Device, w: u32, h: u32) -> Result<Registered> {
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
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: plain D3D11 call with a valid description.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.context("no texture")?;
        self.ctx.bind_to_thread()?;
        let mut resource: cu::CUgraphicsResource = ptr::null_mut();
        // SAFETY: a live D3D11 texture on the context's GPU.
        cu_check(
            unsafe { (self.register)(&mut resource, texture.as_raw(), 0) },
            "cuGraphicsD3D11RegisterResource",
        )?;
        Ok(Registered {
            texture,
            resource,
            width: w,
            height: h,
        })
    }
}

impl Drop for D3dToCuda {
    fn drop(&mut self) {
        self.drop_staging();
    }
}

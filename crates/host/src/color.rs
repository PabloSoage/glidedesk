//! RGB → YUV on the GPU for the RDP H.264 codecs.
//!
//! RDP clients rebuild RGB with full-range BT.709 (MS-RDPEGFX; FreeRDP uses the same
//! integer coefficients as below), so the conversion is done here rather than by NVENC
//! (whose own conversion is limited range). One kernel thread handles a 2×2 block and writes:
//!
//! - the main view, an NV12 frame (full luma, chroma averaged over the block): AVC420, and
//!   the first stream of AVC444v2;
//! - optionally the auxiliary view of AVC444v2 (MS-RDPEGFX 3.3.8.3.3): the chroma samples the
//!   main view dropped, packed into a second NV12 frame, so the client recovers 4:4:4.
//!
//! Frames are padded to multiples of 16 (MS-RDPEGFX 2.2.4.4); edge pixels are repeated.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use cudarc::driver::{
    CudaContext, CudaFunction, CudaStream, LaunchConfig, PushKernelArg, sys as cu,
};

use crate::gpu::{DeviceBuffer, PixelOrder};

const KERNEL: &str = r#"
__device__ __forceinline__ unsigned char clamp8(int v) {
    return (unsigned char)(v < 0 ? 0 : (v > 255 ? 255 : v));
}

// Full-range BT.709 (integer coefficients as in FreeRDP's primitives).
__device__ __forceinline__ void yuv(const unsigned char* p, int order, int* y, int* u, int* v) {
    int r, g, b;
    if (order == 0) { b = p[0]; g = p[1]; r = p[2]; } else { r = p[0]; g = p[1]; b = p[2]; }
    *y = clamp8((54 * r + 183 * g + 18 * b) >> 8);
    *u = clamp8(((-29 * r - 99 * g + 128 * b) >> 8) + 128);
    *v = clamp8(((128 * r - 116 * g - 12 * b) >> 8) + 128);
}

// src: 32-bit pixels, src_w × src_h, src_pitch bytes per row; order 0 = BGRX, 1 = RGBA.
// main/aux: NV12, w × h (multiples of 16), pitch bytes per row; aux may be 0.
extern "C" __global__ void rgb_to_avc(
    const unsigned char* src, int src_pitch, int src_w, int src_h, int order,
    unsigned char* main, unsigned char* aux, int pitch, int w, int h)
{
    int bx = blockIdx.x * blockDim.x + threadIdx.x;   // block column (2 pixels)
    int by = blockIdx.y * blockDim.y + threadIdx.y;   // block row (2 pixels)
    if (bx * 2 >= w || by * 2 >= h) return;
    int x = bx * 2, y = by * 2;

    int Y[4], U[4], V[4];
    for (int i = 0; i < 4; i++) {
        int sx = min(x + (i & 1), src_w - 1);
        int sy = min(y + (i >> 1), src_h - 1);
        yuv(src + (size_t)sy * src_pitch + sx * 4, order, &Y[i], &U[i], &V[i]);
    }
    // i: 0 = (x, y), 1 = (x+1, y), 2 = (x, y+1), 3 = (x+1, y+1)

    unsigned char* my = main;
    unsigned char* muv = main + (size_t)pitch * h;
    my[(size_t)y * pitch + x] = Y[0];
    my[(size_t)y * pitch + x + 1] = Y[1];
    my[(size_t)(y + 1) * pitch + x] = Y[2];
    my[(size_t)(y + 1) * pitch + x + 1] = Y[3];
    muv[(size_t)by * pitch + 2 * bx] = (U[0] + U[1] + U[2] + U[3]) / 4;
    muv[(size_t)by * pitch + 2 * bx + 1] = (V[0] + V[1] + V[2] + V[3]) / 4;

    // The auxiliary layout is defined on the real frame size (as FreeRDP does): halves and
    // quarters of src_w, nothing written in the padding.
    if (aux == 0 || x >= src_w || y >= src_h) return;
    unsigned char* ay = aux;
    unsigned char* auv = aux + (size_t)pitch * h;
    int half = src_w / 2, quarter = src_w / 4;
    // B4/B5: U and V of the odd columns, every row, left and right halves of the luma plane.
    ay[(size_t)y * pitch + bx] = U[1];
    ay[(size_t)y * pitch + half + bx] = V[1];
    ay[(size_t)(y + 1) * pitch + bx] = U[3];
    ay[(size_t)(y + 1) * pitch + half + bx] = V[3];
    // B6/B7 (x % 4 == 0) into the U plane, B8/B9 (x % 4 == 2) into the V plane: U and V of
    // the even columns of the odd rows, left and right halves. NV12 interleaves U and V.
    int k = bx >> 1;
    int plane = bx & 1;   // 0 = U plane, 1 = V plane
    auv[(size_t)by * pitch + 2 * k + plane] = U[2];
    auv[(size_t)by * pitch + 2 * (quarter + k) + plane] = V[2];
}
"#;

/// An NV12 frame on the GPU: luma plane then interleaved chroma, same pitch.
pub struct Nv12 {
    pub ptr: cu::CUdeviceptr,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
    ctx: Arc<CudaContext>,
}

impl Nv12 {
    fn new(ctx: &Arc<CudaContext>, width: u32, height: u32) -> Result<Self> {
        let pitch = (width as usize).next_multiple_of(256);
        let bytes = pitch * height as usize * 3 / 2;
        ctx.bind_to_thread()?;
        let mut ptr = 0;
        // SAFETY: the context is current on this thread.
        let r = unsafe { cu::cuMemAlloc_v2(&mut ptr, bytes) };
        if r != cu::CUresult::CUDA_SUCCESS {
            bail!("cuMemAlloc for an NV12 frame failed: {r:?}");
        }
        // SAFETY: clears the allocation (black luma, neutral chroma is not needed: padding
        // only costs bits if it changes, and it never does).
        unsafe {
            cu::cuMemsetD8_v2(ptr, 0, bytes);
        }
        Ok(Self {
            ptr,
            pitch,
            width,
            height,
            ctx: Arc::clone(ctx),
        })
    }
}

impl Drop for Nv12 {
    fn drop(&mut self) {
        let _ = self.ctx.bind_to_thread();
        // SAFETY: allocated in `new`, freed once.
        unsafe {
            cu::cuMemFree_v2(self.ptr);
        }
    }
}

/// Size the encoder works at: the frame padded to multiples of 16.
pub fn coded_size(width: u32, height: u32) -> (u32, u32) {
    (width.next_multiple_of(16), height.next_multiple_of(16))
}

pub struct Converter {
    ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    func: CudaFunction,
    main: Option<Nv12>,
    aux: Option<Nv12>,
}

impl Converter {
    pub fn new(ctx: Arc<CudaContext>) -> Result<Self> {
        let ptx =
            cudarc::nvrtc::compile_ptx(KERNEL).context("compiling the colour kernel (NVRTC)")?;
        let module = ctx.load_module(ptx)?;
        let func = module.load_function("rgb_to_avc")?;
        let stream = ctx.default_stream();
        Ok(Self {
            ctx,
            stream,
            func,
            main: None,
            aux: None,
        })
    }

    /// Converts `src` into the main view and, with `avc444`, the auxiliary view.
    pub fn convert(&mut self, src: &DeviceBuffer, avc444: bool) -> Result<(&Nv12, Option<&Nv12>)> {
        let (w, h) = coded_size(src.width, src.height);
        if self
            .main
            .as_ref()
            .is_none_or(|m| (m.width, m.height) != (w, h))
        {
            self.main = Some(Nv12::new(&self.ctx, w, h)?);
            self.aux = None;
        }
        if avc444 && self.aux.is_none() {
            self.aux = Some(Nv12::new(&self.ctx, w, h)?);
        }
        let main = self.main.as_ref().expect("allocated above");
        let aux_ptr: u64 = if avc444 {
            self.aux.as_ref().map_or(0, |a| a.ptr)
        } else {
            0
        };
        let (src_ptr, src_pitch, src_w, src_h) = (
            src.ptr,
            src.pitch as i32,
            src.width as i32,
            src.height as i32,
        );
        let order: i32 = match src.order {
            PixelOrder::Bgrx => 0,
            PixelOrder::Rgba => 1,
        };
        let (main_ptr, pitch, cw, ch) = (main.ptr, main.pitch as i32, w as i32, h as i32);
        let blocks = (w / 2, h / 2);
        let cfg = LaunchConfig {
            grid_dim: (blocks.0.div_ceil(16), blocks.1.div_ceil(16), 1),
            block_dim: (16, 16, 1),
            shared_mem_bytes: 0,
        };
        let mut launch = self.stream.launch_builder(&self.func);
        launch
            .arg(&src_ptr)
            .arg(&src_pitch)
            .arg(&src_w)
            .arg(&src_h)
            .arg(&order)
            .arg(&main_ptr)
            .arg(&aux_ptr)
            .arg(&pitch)
            .arg(&cw)
            .arg(&ch);
        // SAFETY: the arguments match the kernel signature; buffers cover the sizes passed.
        unsafe { launch.launch(cfg) }?;
        self.stream.synchronize()?;
        Ok((main, if avc444 { self.aux.as_ref() } else { None }))
    }
}

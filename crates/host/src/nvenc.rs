//! NVENC encoder on a CUDA context.
//!
//! Low-latency settings (see docs: ultra-low-latency tuning, no B-frames, infinite GOP, a
//! VBV of about one frame, intra refresh instead of periodic IDR). Frames are BGRx in a
//! pitched CUDA buffer that NVENC reads as `ARGB` (A8R8G8B8 little-endian is B, G, R, A in
//! memory); NVENC converts to YUV itself.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use cudarc::driver::{CudaContext, sys as cu};
use nvenc_sys as nv;
use tracing::{debug, info};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Reference frames for P-frames. AVC444 interleaves the two views in one sequence, so
    /// each view needs the picture two back to reference its own previous frame.
    pub refs: u32,
}

#[derive(Default)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Average QP NVENC used for the picture.
    pub qp: u32,
}

struct Api {
    _lib: libloading::Library,
    f: nv::NV_ENCODE_API_FUNCTION_LIST,
}

fn check(status: nv::NVENCSTATUS, what: &str) -> Result<()> {
    if status != nv::NV_ENC_SUCCESS {
        bail!("{what} failed with NVENCSTATUS {status}");
    }
    Ok(())
}

fn cu_check(r: cu::CUresult, what: &str) -> Result<()> {
    if r != cu::CUresult::CUDA_SUCCESS {
        bail!("{what} failed: {r:?}");
    }
    Ok(())
}

impl Api {
    fn load() -> Result<Self> {
        // SAFETY: loading the NVIDIA driver library; its initialisers have no preconditions.
        let lib = unsafe { libloading::Library::new("libnvidia-encode.so.1") }
            .context("loading libnvidia-encode.so.1 (NVIDIA driver)")?;
        let mut f = nv::NV_ENCODE_API_FUNCTION_LIST {
            version: nv::NV_ENCODE_API_FUNCTION_LIST_VER,
            ..Default::default()
        };
        // SAFETY: symbol type matches the header declaration.
        unsafe {
            let create: libloading::Symbol<nv::PFN_NvEncodeAPICreateInstance> =
                lib.get(b"NvEncodeAPICreateInstance\0")?;
            check(create(&mut f), "NvEncodeAPICreateInstance")?;
        }
        Ok(Self { _lib: lib, f })
    }
}

pub struct Encoder {
    api: Api,
    enc: *mut c_void,
    ctx: Arc<CudaContext>,
    input: cu::CUdeviceptr,
    input_pitch: usize,
    registered: nv::NV_ENC_REGISTERED_PTR,
    bitstream: nv::NV_ENC_OUTPUT_PTR,
    /// Device buffers from the capture pool, registered once each.
    external: std::collections::HashMap<cu::CUdeviceptr, nv::NV_ENC_REGISTERED_PTR>,
    cfg: EncoderConfig,
    frame_idx: u32,
    force_idr: bool,
    /// Constant QP of a quality pass, `None` for the normal rate control.
    const_qp: Option<u32>,
}

// The raw handles are only used from the thread that owns the encoder.
unsafe impl Send for Encoder {}

macro_rules! call {
    ($api:expr, $name:ident, $($arg:expr),*) => {{
        let f = $api.f.$name.expect(concat!(stringify!($name), " missing"));
        // SAFETY: arguments are valid NVENC structures with their version fields set.
        check(unsafe { f($($arg),*) }, stringify!($name))
    }};
}

impl Encoder {
    pub fn new(ctx: Arc<CudaContext>, cfg: EncoderConfig) -> Result<Self> {
        ctx.bind_to_thread()?;
        let api = Api::load()?;

        let mut open = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE_CUDA,
            device: ctx.cu_ctx() as *mut c_void,
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        call!(api, nvEncOpenEncodeSessionEx, &mut open, &mut enc)?;

        let mut this = Self {
            api,
            enc,
            ctx,
            input: 0,
            input_pitch: 0,
            registered: ptr::null_mut(),
            bitstream: ptr::null_mut(),
            external: Default::default(),
            cfg: cfg.clone(),
            frame_idx: 0,
            force_idr: true,
            const_qp: None,
        };
        this.initialize()?;
        this.allocate()?;
        info!(?cfg, "NVENC encoder ready");
        Ok(this)
    }

    fn codec_guid(&self) -> nv::GUID {
        match self.cfg.codec {
            Codec::H264 => nv::NV_ENC_CODEC_H264_GUID,
            Codec::Hevc => nv::NV_ENC_CODEC_HEVC_GUID,
        }
    }

    fn init_params(&self, config: &mut nv::NV_ENC_CONFIG) -> Result<nv::NV_ENC_INITIALIZE_PARAMS> {
        let c = &self.cfg;
        let mut preset = nv::NV_ENC_PRESET_CONFIG {
            version: nv::NV_ENC_PRESET_CONFIG_VER,
            presetCfg: nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..Default::default()
            },
            ..Default::default()
        };
        call!(
            self.api,
            nvEncGetEncodePresetConfigEx,
            self.enc,
            self.codec_guid(),
            nv::NV_ENC_PRESET_P1_GUID,
            nv::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            &mut preset
        )?;
        *config = preset.presetCfg;
        config.version = nv::NV_ENC_CONFIG_VER;
        config.gopLength = nv::NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1;
        let rc = &mut config.rcParams;
        rc.set_zeroReorderDelay(1);
        if let Some(qp) = self.const_qp {
            // Quality pass over a still image: a fixed QP regardless of the bit budget.
            rc.rateControlMode = nv::NV_ENC_PARAMS_RC_CONSTQP;
            rc.constQP = nv::NV_ENC_QP {
                qpInterP: qp,
                qpInterB: qp,
                qpIntra: qp,
            };
        } else {
            rc.rateControlMode = nv::NV_ENC_PARAMS_RC_CBR;
            rc.averageBitRate = c.bitrate;
            rc.maxBitRate = c.bitrate;
            // One frame of VBV: the encoder never builds a queue the network must drain.
            rc.vbvBufferSize = c.bitrate / c.fps.max(1);
            rc.vbvInitialDelay = rc.vbvBufferSize;
        }

        // SAFETY: the union member matches the codec GUID.
        unsafe {
            match c.codec {
                Codec::H264 => {
                    let h = &mut config.encodeCodecConfig.h264Config;
                    h.idrPeriod = nv::NVENC_INFINITE_GOPLENGTH;
                    h.set_repeatSPSPPS(1);
                    h.set_outputAUD(0);
                    h.chromaFormatIDC = 1;
                    h.maxNumRefFrames = c.refs;
                    if c.refs >= 2 {
                        h.numRefL0 = nv::NV_ENC_NUM_REF_FRAMES_2;
                    }
                    let v = &mut h.h264VUIParameters;
                    // Tell decoders there is no reordering (no B-frames): they can output each
                    // frame as soon as it is decoded instead of holding one back.
                    v.bitstreamRestrictionFlag = 1;
                    v.videoSignalTypePresentFlag = 1;
                    v.videoFormat = nv::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                    v.videoFullRangeFlag = 1;
                    v.colourDescriptionPresentFlag = 1;
                    v.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                    v.transferCharacteristics = nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                    v.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS_BT709;
                }
                Codec::Hevc => {
                    let h = &mut config.encodeCodecConfig.hevcConfig;
                    h.idrPeriod = nv::NVENC_INFINITE_GOPLENGTH;
                    h.set_repeatSPSPPS(1);
                    h.set_chromaFormatIDC(1);
                    let v = &mut h.hevcVUIParameters;
                    v.videoSignalTypePresentFlag = 1;
                    v.videoFullRangeFlag = 1;
                    v.colourDescriptionPresentFlag = 1;
                    v.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                    v.transferCharacteristics = nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                    v.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS_BT709;
                }
            }
        }

        Ok(nv::NV_ENC_INITIALIZE_PARAMS {
            version: nv::NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: self.codec_guid(),
            presetGUID: nv::NV_ENC_PRESET_P1_GUID,
            encodeWidth: c.width,
            encodeHeight: c.height,
            darWidth: c.width,
            darHeight: c.height,
            frameRateNum: c.fps,
            frameRateDen: 1,
            enablePTD: 1,
            tuningInfo: nv::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            maxEncodeWidth: 4096,
            maxEncodeHeight: 4096,
            ..Default::default()
        })
    }

    fn initialize(&mut self) -> Result<()> {
        let mut config = nv::NV_ENC_CONFIG::default();
        let mut init = self.init_params(&mut config)?;
        init.encodeConfig = &mut config;
        call!(self.api, nvEncInitializeEncoder, self.enc, &mut init)
    }

    fn allocate(&mut self) -> Result<()> {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        let mut dptr: cu::CUdeviceptr = 0;
        let mut pitch: usize = 0;
        // SAFETY: the context is current on this thread.
        cu_check(
            unsafe { cu::cuMemAllocPitch_v2(&mut dptr, &mut pitch, w * 4, h, 16) },
            "cuMemAllocPitch",
        )?;
        self.input = dptr;
        self.input_pitch = pitch;

        let mut reg = nv::NV_ENC_REGISTER_RESOURCE {
            version: nv::NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: nv::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            width: w as u32,
            height: h as u32,
            pitch: pitch as u32,
            resourceToRegister: dptr as *mut c_void,
            bufferFormat: nv::NV_ENC_BUFFER_FORMAT_ARGB,
            bufferUsage: nv::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        call!(self.api, nvEncRegisterResource, self.enc, &mut reg)?;
        self.registered = reg.registeredResource;

        let mut bs = nv::NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        call!(self.api, nvEncCreateBitstreamBuffer, self.enc, &mut bs)?;
        self.bitstream = bs.bitstreamBuffer;
        Ok(())
    }

    fn release_buffers(&mut self) {
        // SAFETY: handles were created by this session and are released once.
        unsafe {
            if !self.bitstream.is_null() {
                if let Some(f) = self.api.f.nvEncDestroyBitstreamBuffer {
                    f(self.enc, self.bitstream);
                }
                self.bitstream = ptr::null_mut();
            }
            if !self.registered.is_null() {
                if let Some(f) = self.api.f.nvEncUnregisterResource {
                    f(self.enc, self.registered);
                }
                self.registered = ptr::null_mut();
            }
            if self.input != 0 {
                cu::cuMemFree_v2(self.input);
                self.input = 0;
            }
            for (_, r) in self.external.drain() {
                if let Some(f) = self.api.f.nvEncUnregisterResource {
                    f(self.enc, r);
                }
            }
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.cfg.width, self.cfg.height)
    }

    /// Changes the encode size in place (no new session) and forces an IDR.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.ctx.bind_to_thread()?;
        self.release_buffers();
        self.cfg.width = width;
        self.cfg.height = height;
        let mut config = nv::NV_ENC_CONFIG::default();
        let mut init = self.init_params(&mut config)?;
        init.encodeConfig = &mut config;
        let mut re = nv::NV_ENC_RECONFIGURE_PARAMS {
            version: nv::NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: init,
            ..Default::default()
        };
        re.set_resetEncoder(1);
        re.set_forceIDR(1);
        call!(self.api, nvEncReconfigureEncoder, self.enc, &mut re)?;
        self.allocate()?;
        self.force_idr = true;
        debug!(width, height, "Encoder resized");
        Ok(())
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Switches between the low-latency CBR (`None`) and a constant QP for quality passes
    /// over a still image. The rate control changes in place: no new sequence, no IDR.
    pub fn set_const_qp(&mut self, qp: Option<u32>) -> Result<()> {
        if self.const_qp == qp {
            return Ok(());
        }
        self.ctx.bind_to_thread()?;
        self.const_qp = qp;
        let mut config = nv::NV_ENC_CONFIG::default();
        let mut init = self.init_params(&mut config)?;
        init.encodeConfig = &mut config;
        let mut re = nv::NV_ENC_RECONFIGURE_PARAMS {
            version: nv::NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: init,
            ..Default::default()
        };
        call!(self.api, nvEncReconfigureEncoder, self.enc, &mut re)
    }

    /// Encodes one BGRx frame from system memory.
    pub fn encode_host(&mut self, data: &[u8], stride: usize) -> Result<EncodedFrame> {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        if data.len() < stride * (h - 1) + w * 4 {
            bail!("frame buffer too small for {w}x{h}");
        }
        self.ctx.bind_to_thread()?;
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
            dstDevice: self.input,
            dstArray: ptr::null_mut(),
            dstPitch: self.input_pitch,
            WidthInBytes: w * 4,
            Height: h,
        };
        // SAFETY: source covers `h` rows of `stride` bytes; destination was allocated for it.
        cu_check(unsafe { cu::cuMemcpy2D_v2(&copy) }, "cuMemcpy2D")?;
        self.encode_mapped(
            self.registered,
            self.input_pitch,
            nv::NV_ENC_BUFFER_FORMAT_ARGB,
        )
    }

    /// Encodes a 32-bit frame that is already on this GPU (registered on first use).
    pub fn encode_device(&mut self, buf: &crate::gpu::DeviceBuffer) -> Result<EncodedFrame> {
        self.encode_external(
            buf.ptr,
            buf.pitch,
            buf.width,
            buf.height,
            buffer_format(buf.order),
        )
    }

    /// Encodes an NV12 frame (from [`crate::color::Converter`]).
    pub fn encode_nv12(&mut self, f: &crate::color::Nv12) -> Result<EncodedFrame> {
        self.encode_external(
            f.ptr,
            f.pitch,
            f.width,
            f.height,
            nv::NV_ENC_BUFFER_FORMAT_NV12,
        )
    }

    fn encode_external(
        &mut self,
        ptr: cu::CUdeviceptr,
        pitch: usize,
        width: u32,
        height: u32,
        format: nv::NV_ENC_BUFFER_FORMAT,
    ) -> Result<EncodedFrame> {
        if (width, height) != (self.cfg.width, self.cfg.height) {
            bail!(
                "frame is {width}x{height}, encoder {}x{}",
                self.cfg.width,
                self.cfg.height
            );
        }
        self.ctx.bind_to_thread()?;
        let registered = match self.external.get(&ptr) {
            Some(&r) => r,
            None => {
                let mut reg = nv::NV_ENC_REGISTER_RESOURCE {
                    version: nv::NV_ENC_REGISTER_RESOURCE_VER,
                    resourceType: nv::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                    width,
                    height,
                    pitch: pitch as u32,
                    resourceToRegister: ptr as *mut c_void,
                    bufferFormat: format,
                    bufferUsage: nv::NV_ENC_INPUT_IMAGE,
                    ..Default::default()
                };
                call!(self.api, nvEncRegisterResource, self.enc, &mut reg)?;
                self.external.insert(ptr, reg.registeredResource);
                reg.registeredResource
            }
        };
        self.encode_mapped(registered, pitch, format)
    }

    fn encode_mapped(
        &mut self,
        registered: nv::NV_ENC_REGISTERED_PTR,
        pitch: usize,
        format: nv::NV_ENC_BUFFER_FORMAT,
    ) -> Result<EncodedFrame> {
        let mut map = nv::NV_ENC_MAP_INPUT_RESOURCE {
            version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: registered,
            ..Default::default()
        };
        call!(self.api, nvEncMapInputResource, self.enc, &mut map)?;
        let mut pic = nv::NV_ENC_PIC_PARAMS {
            version: nv::NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.cfg.width,
            inputHeight: self.cfg.height,
            inputPitch: pitch as u32,
            inputBuffer: map.mappedResource,
            outputBitstream: self.bitstream,
            bufferFmt: format,
            pictureStruct: nv::NV_ENC_PIC_STRUCT_FRAME,
            frameIdx: self.frame_idx,
            inputTimeStamp: self.frame_idx as u64,
            ..Default::default()
        };
        if self.force_idr {
            pic.encodePicFlags = nv::NV_ENC_PIC_FLAG_FORCEIDR | nv::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }
        let r = call!(self.api, nvEncEncodePicture, self.enc, &mut pic);
        let out = r.and_then(|_| {
            let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
                version: nv::NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: self.bitstream,
                ..Default::default()
            };
            call!(self.api, nvEncLockBitstream, self.enc, &mut lock)?;
            // SAFETY: NVENC guarantees the locked pointer covers bitstreamSizeInBytes.
            let data = unsafe {
                std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr as *const u8,
                    lock.bitstreamSizeInBytes as usize,
                )
            }
            .to_vec();
            let keyframe = lock.pictureType == nv::NV_ENC_PIC_TYPE_IDR;
            let qp = lock.frameAvgQP;
            call!(self.api, nvEncUnlockBitstream, self.enc, self.bitstream)?;
            Ok(EncodedFrame { data, keyframe, qp })
        });
        call!(
            self.api,
            nvEncUnmapInputResource,
            self.enc,
            map.mappedResource
        )?;
        self.frame_idx = self.frame_idx.wrapping_add(1);
        if out.is_ok() {
            self.force_idr = false;
        }
        out
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.ctx.bind_to_thread();
        self.release_buffers();
        // SAFETY: the session is destroyed once, after its buffers.
        unsafe {
            if let Some(f) = self.api.f.nvEncDestroyEncoder {
                f(self.enc);
            }
        }
    }
}

fn buffer_format(order: crate::gpu::PixelOrder) -> nv::NV_ENC_BUFFER_FORMAT {
    match order {
        // A8R8G8B8 little-endian: B, G, R, A in memory.
        crate::gpu::PixelOrder::Bgrx => nv::NV_ENC_BUFFER_FORMAT_ARGB,
        // A8B8G8R8 little-endian: R, G, B, A in memory.
        crate::gpu::PixelOrder::Rgba => nv::NV_ENC_BUFFER_FORMAT_ABGR,
    }
}

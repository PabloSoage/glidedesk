//! Video decoding through FFmpeg: the GPU's decoder where there is one (D3D11VA on Windows:
//! NVIDIA, AMD and Intel alike; VA-API, then NVDEC, on Linux), FFmpeg's own software
//! decoders otherwise. H.264 and HEVC. Decoded pictures come back as NV12 in system memory
//! for the renderer.
//!
//! Low-latency settings: no frame threading (it holds pictures back), `LOW_DELAY`, one picture
//! out per access unit. If the GPU refuses a stream (an unsupported profile or size), FFmpeg
//! asks for a format again without the hardware one and decoding continues in software.

use std::ffi::{CStr, c_void};
use std::ptr;

use anyhow::{Result, bail};
use ffmpeg_sys_next as ff;
use glidedesk_client::gfx::VideoCodec;
use tracing::{debug, info};

use crate::video::Picture;

/// Hardware decoders to try, in order.
#[cfg(windows)]
const HARDWARE: &[(ff::AVHWDeviceType, ff::AVPixelFormat, &str)] = &[(
    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
    ff::AVPixelFormat::AV_PIX_FMT_D3D11,
    "D3D11VA",
)];
#[cfg(not(windows))]
const HARDWARE: &[(ff::AVHWDeviceType, ff::AVPixelFormat, &str)] = &[
    (
        ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        ff::AVPixelFormat::AV_PIX_FMT_VAAPI,
        "VA-API",
    ),
    (
        ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
        ff::AVPixelFormat::AV_PIX_FMT_CUDA,
        "NVDEC",
    ),
];

/// A keyframe of HEVC (256x144) to find out whether the GPU decodes HEVC.
const PROBE_HEVC: &[u8] = include_bytes!("probe.hevc");

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    sw: *mut ff::AVFrame,
    /// The hardware decoder set up for this stream, if any.
    hardware: Option<&'static str>,
}

// The FFmpeg objects are only used from the thread that owns the decoder.
unsafe impl Send for Decoder {}

fn averror(e: i32) -> String {
    let mut buf = [0 as std::ffi::c_char; 128];
    // SAFETY: the buffer is large enough and NUL-terminated by av_strerror.
    unsafe {
        ff::av_strerror(e, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// FFmpeg's format negotiation: the hardware format (kept in `opaque`) when offered, else
/// the first software one.
unsafe extern "C" fn get_format(
    ctx: *mut ff::AVCodecContext,
    formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    // SAFETY: FFmpeg passes its context and a list ended by AV_PIX_FMT_NONE; `opaque` holds
    // an AVPixelFormat as an integer (set in `Decoder::new`).
    unsafe {
        let wanted = (*ctx).opaque as usize as i32;
        let mut p = formats;
        let mut first_sw = ff::AVPixelFormat::AV_PIX_FMT_NONE;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p as i32 == wanted {
                return *p;
            }
            let desc = ff::av_pix_fmt_desc_get(*p);
            if first_sw == ff::AVPixelFormat::AV_PIX_FMT_NONE
                && !desc.is_null()
                && (*desc).flags & (ff::AV_PIX_FMT_FLAG_HWACCEL as u64) == 0
            {
                first_sw = *p;
            }
            p = p.add(1);
        }
        if wanted >= 0 {
            debug!("The GPU did not take the stream; decoding in software");
        }
        first_sw
    }
}

impl Decoder {
    /// `hardware`: try the GPU's decoder first.
    pub fn new(codec: VideoCodec, hardware: bool) -> Result<Self> {
        // GLIDEDESK_FFMPEG_LOG=1: FFmpeg's own messages on stderr (why the GPU refused).
        if std::env::var_os("GLIDEDESK_FFMPEG_LOG").is_some() {
            // SAFETY: sets a global level.
            unsafe { ff::av_log_set_level(ff::AV_LOG_VERBOSE) };
        }
        let id = match codec {
            VideoCodec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            VideoCodec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
        };
        // SAFETY: plain FFmpeg API use; every object is owned by `this` and freed in Drop.
        unsafe {
            let dec = ff::avcodec_find_decoder(id);
            if dec.is_null() {
                bail!("FFmpeg has no {codec:?} decoder");
            }
            let mut this = Self {
                ctx: ff::avcodec_alloc_context3(dec),
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                sw: ff::av_frame_alloc(),
                hardware: None,
            };
            let c = &mut *this.ctx;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            // (The constants are i32 or u32 depending on the platform headers.)
            c.flags2 |= ff::AV_CODEC_FLAG2_FAST as std::ffi::c_int;
            c.opaque = usize::MAX as *mut c_void; // no hardware format
            if hardware {
                for &(ty, fmt, name) in HARDWARE {
                    let mut device = ptr::null_mut();
                    let e = ff::av_hwdevice_ctx_create(
                        &mut device,
                        ty,
                        ptr::null(),
                        ptr::null_mut(),
                        0,
                    );
                    if e < 0 {
                        debug!(name, error = averror(e), "No hardware decoder");
                        continue;
                    }
                    c.hw_device_ctx = device;
                    c.opaque = fmt as i32 as usize as *mut c_void;
                    c.get_format = Some(get_format);
                    this.hardware = Some(name);
                    break;
                }
            }
            if this.hardware.is_some() {
                c.thread_count = 1;
            } else {
                // Slice threads only: frame threads would hold pictures back.
                c.thread_type = ff::FF_THREAD_SLICE as _;
                c.thread_count = 0;
            }
            let e = ff::avcodec_open2(this.ctx, dec, ptr::null_mut());
            if e < 0 {
                bail!("opening the {codec:?} decoder: {}", averror(e));
            }
            Ok(this)
        }
    }

    /// The hardware decoder in use, if any (it may still fall back to software per stream).
    pub fn hardware(&self) -> Option<&'static str> {
        self.hardware
    }

    /// Decodes one access unit (Annex B); returns its picture, and whether the GPU decoded it.
    pub fn decode(
        &mut self,
        data: &[u8],
        received: std::time::Instant,
    ) -> Result<Option<(Picture, bool)>> {
        // SAFETY: the packet borrows `data` only for the duration of send_packet (FFmpeg
        // copies unreferenced packets); frames are owned by `self`.
        unsafe {
            let pkt = &mut *self.pkt;
            pkt.data = data.as_ptr().cast_mut();
            pkt.size = data.len() as i32;
            let e = ff::avcodec_send_packet(self.ctx, self.pkt);
            pkt.data = ptr::null_mut();
            pkt.size = 0;
            if e < 0 && e != ff::AVERROR(libc::EAGAIN) {
                bail!("decoding: {}", averror(e));
            }
            self.receive(received)
        }
    }

    /// Takes the newest decoded picture.
    unsafe fn receive(&mut self, received: std::time::Instant) -> Result<Option<(Picture, bool)>> {
        let mut out = None;
        // SAFETY: see `decode`.
        unsafe {
            loop {
                let e = ff::avcodec_receive_frame(self.ctx, self.frame);
                if e == ff::AVERROR(libc::EAGAIN) || e == ff::AVERROR_EOF {
                    break;
                }
                if e < 0 {
                    bail!("decoding: {}", averror(e));
                }
                let hw = !(*self.frame).hw_frames_ctx.is_null();
                let src = if hw {
                    ff::av_frame_unref(self.sw);
                    (*self.sw).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                    let e = ff::av_hwframe_transfer_data(self.sw, self.frame, 0);
                    if e < 0 {
                        ff::av_frame_unref(self.frame);
                        bail!("copying the picture from the GPU: {}", averror(e));
                    }
                    self.sw
                } else {
                    self.frame
                };
                out = Some((to_picture(&*src, received)?, hw));
                ff::av_frame_unref(self.frame);
            }
        }
        Ok(out)
    }
}

/// A decoded frame (NV12, or planar 4:2:0 from the software decoders) as NV12.
fn to_picture(f: &ff::AVFrame, received: std::time::Instant) -> Result<Picture> {
    let (w, h) = (f.width as usize, f.height as usize);
    let ch = h.div_ceil(2);
    let plane = |i: usize, rows: usize| -> &[u8] {
        // SAFETY: FFmpeg planes hold `linesize * rows` bytes (positive strides here).
        unsafe { std::slice::from_raw_parts(f.data[i], f.linesize[i] as usize * rows) }
    };
    let y_stride = f.linesize[0] as usize;
    let fmt = f.format;
    let (uv, uv_stride) = if fmt == ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32 {
        (plane(1, ch).to_vec(), f.linesize[1] as usize)
    } else if fmt == ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32
        || fmt == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32
    {
        // Interleave U and V (software decoding only).
        let cw = w.div_ceil(2);
        let (us, vs) = (f.linesize[1] as usize, f.linesize[2] as usize);
        let (u, v) = (plane(1, ch), plane(2, ch));
        let mut uv = vec![0u8; cw * 2 * ch];
        for row in 0..ch {
            let dst = &mut uv[row * cw * 2..(row + 1) * cw * 2];
            for (x, pair) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                pair[0] = u[row * us + x];
                pair[1] = v[row * vs + x];
            }
        }
        (uv, cw * 2)
    } else {
        bail!("unexpected picture format {fmt}");
    };
    Ok(Picture {
        width: w as u32,
        height: h as u32,
        y: plane(0, h).to_vec(),
        uv,
        y_stride: y_stride as u32,
        uv_stride: uv_stride as u32,
        received,
    })
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is owned by this decoder (or null).
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_packet_free(&mut self.pkt);
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.sw);
        }
    }
}

/// Whether the GPU decodes HEVC here: decodes a small keyframe and looks at who did it.
pub fn hardware_hevc() -> bool {
    let Ok(mut dec) = Decoder::new(VideoCodec::Hevc, true) else {
        return false;
    };
    if dec.hardware().is_none() {
        return false;
    }
    let now = std::time::Instant::now();
    let mut got = dec.decode(PROBE_HEVC, now).ok().flatten();
    if got.is_none() {
        // Drain: the keyframe may still be inside the decoder.
        // SAFETY: a null packet starts draining; the decoder is dropped afterwards.
        unsafe {
            ff::avcodec_send_packet(dec.ctx, ptr::null());
            got = dec.receive(now).ok().flatten();
        }
    }
    let hw = got.is_some_and(|(_, hw)| hw);
    info!(hardware = hw, "HEVC decoding");
    hw
}

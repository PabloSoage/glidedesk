//! H.264/HEVC through FFmpeg: VA-API (AMD, Intel and others on Linux) and x264 in software.
//!
//! The same low-latency shape as the NVENC encoder: CBR with about one frame of VBV, no
//! B-frames, an endless GOP with IDRs only on request, full-range BT.709 signalled in the
//! VUI. Frames come in as NV12 in system memory ([`crate::csc`]); for VA-API they are
//! uploaded into a VA surface.

use std::ffi::{CStr, CString};
use std::ptr;

use anyhow::{Result, bail};
use ffmpeg_sys_next as ff;
use tracing::info;

use crate::csc::Nv12Frame;
use crate::encoder::{Codec, EncodedFrame, EncoderConfig};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Hardware encoder of the GPU behind a DRM render node.
    Vaapi,
    /// libx264 on the CPU: works anywhere, costs CPU time.
    Software,
}

pub struct FfEncoder {
    ctx: *mut ff::AVCodecContext,
    hw_device: *mut ff::AVBufferRef,
    hw_frames: *mut ff::AVBufferRef,
    sw_frame: *mut ff::AVFrame,
    hw_frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    kind: Kind,
    cfg: EncoderConfig,
    pts: i64,
    force_idr: bool,
}

// The FFmpeg objects are only used from the thread that owns the encoder.
unsafe impl Send for FfEncoder {}

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

fn set(dict: &mut *mut ff::AVDictionary, key: &str, value: &str) {
    let (k, v) = (CString::new(key).unwrap(), CString::new(value).unwrap());
    // SAFETY: av_dict_set copies both strings.
    unsafe {
        ff::av_dict_set(dict, k.as_ptr(), v.as_ptr(), 0);
    }
}

impl FfEncoder {
    /// `device`: DRM render node for VA-API (e.g. /dev/dri/renderD128).
    pub fn new(kind: Kind, device: &str, cfg: EncoderConfig) -> Result<Self> {
        let name = match (kind, cfg.codec) {
            (Kind::Vaapi, Codec::H264) => "h264_vaapi",
            (Kind::Vaapi, Codec::Hevc) => "hevc_vaapi",
            (Kind::Software, Codec::H264) => "libx264",
            (Kind::Software, Codec::Hevc) => "libx265",
        };
        let cname = CString::new(name).unwrap();
        // SAFETY: plain FFmpeg API use; every object is owned by `this` and freed in Drop.
        unsafe {
            let codec = ff::avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                bail!("FFmpeg has no {name} encoder");
            }
            let mut this = Self {
                ctx: ff::avcodec_alloc_context3(codec),
                hw_device: ptr::null_mut(),
                hw_frames: ptr::null_mut(),
                sw_frame: ff::av_frame_alloc(),
                hw_frame: ptr::null_mut(),
                pkt: ff::av_packet_alloc(),
                kind,
                cfg: cfg.clone(),
                pts: 0,
                force_idr: true,
            };
            let c = &mut *this.ctx;
            c.width = cfg.width as i32;
            c.height = cfg.height as i32;
            c.time_base = ff::AVRational {
                num: 1,
                den: cfg.fps as i32,
            };
            c.framerate = ff::AVRational {
                num: cfg.fps as i32,
                den: 1,
            };
            c.bit_rate = i64::from(cfg.bitrate);
            c.rc_max_rate = i64::from(cfg.bitrate);
            // One frame of VBV, as with NVENC.
            c.rc_buffer_size = (cfg.bitrate / cfg.fps.max(1)) as i32;
            c.max_b_frames = 0;
            c.gop_size = i32::MAX;
            c.refs = cfg.refs as i32;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_JPEG;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;

            let mut opts: *mut ff::AVDictionary = ptr::null_mut();
            match kind {
                Kind::Vaapi => {
                    let dev = CString::new(device).unwrap();
                    check(
                        ff::av_hwdevice_ctx_create(
                            &mut this.hw_device,
                            ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                            dev.as_ptr(),
                            ptr::null_mut(),
                            0,
                        ),
                        "opening the VA-API device",
                    )?;
                    this.hw_frames = ff::av_hwframe_ctx_alloc(this.hw_device);
                    let fc = &mut *((*this.hw_frames).data as *mut ff::AVHWFramesContext);
                    fc.format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                    fc.sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
                    fc.width = cfg.width as i32;
                    fc.height = cfg.height as i32;
                    fc.initial_pool_size = 4;
                    check(ff::av_hwframe_ctx_init(this.hw_frames), "VA-API frame pool")?;
                    c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                    c.hw_frames_ctx = ff::av_buffer_ref(this.hw_frames);
                    set(&mut opts, "rc_mode", "CBR");
                    // One frame in flight: the packet comes out of the same call.
                    set(&mut opts, "async_depth", "1");
                    set(&mut opts, "sei", "0");
                    set(&mut opts, "aud", "0");
                }
                Kind::Software => {
                    c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;
                    c.thread_count = 0;
                    set(&mut opts, "preset", "ultrafast");
                    set(&mut opts, "tune", "zerolatency");
                    set(&mut opts, "forced-idr", "1");
                    let params = match cfg.codec {
                        Codec::H264 => "keyint=infinite:scenecut=0:repeat-headers=1:nal-hrd=none",
                        Codec::Hevc => "keyint=-1:scenecut=0:repeat-headers=1",
                    };
                    set(
                        &mut opts,
                        if cfg.codec == Codec::H264 {
                            "x264-params"
                        } else {
                            "x265-params"
                        },
                        params,
                    );
                }
            }
            let r = ff::avcodec_open2(this.ctx, codec, &mut opts);
            ff::av_dict_free(&mut opts);
            check(r, &format!("opening {name}"))?;

            let f = &mut *this.sw_frame;
            f.format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
            f.width = cfg.width as i32;
            f.height = cfg.height as i32;
            check(ff::av_frame_get_buffer(this.sw_frame, 0), "frame buffer")?;
            if kind == Kind::Vaapi {
                this.hw_frame = ff::av_frame_alloc();
            }
            info!(encoder = name, ?cfg, "FFmpeg encoder ready");
            Ok(this)
        }
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Encodes one NV12 frame of the encoder's size.
    pub fn encode(&mut self, frame: &Nv12Frame) -> Result<EncodedFrame> {
        if (frame.width, frame.height) != (self.cfg.width, self.cfg.height) {
            bail!(
                "frame is {}x{}, encoder {}x{}",
                frame.width,
                frame.height,
                self.cfg.width,
                self.cfg.height
            );
        }
        // SAFETY: the frames were allocated for this size; copies stay within both buffers.
        unsafe {
            check(
                ff::av_frame_make_writable(self.sw_frame),
                "frame not writable",
            )?;
            let f = &mut *self.sw_frame;
            let (y, uv) = frame.planes();
            let (w, h) = (frame.width as usize, frame.height as usize);
            for (plane, src, rows) in [(0, y, h), (1, uv, h / 2)] {
                let dst = f.data[plane];
                let ls = f.linesize[plane] as usize;
                for r in 0..rows {
                    ptr::copy_nonoverlapping(src.as_ptr().add(r * frame.pitch), dst.add(r * ls), w);
                }
            }
            f.pts = self.pts;
            f.pict_type = if self.force_idr {
                ff::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ff::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let input = match self.kind {
                Kind::Software => self.sw_frame,
                Kind::Vaapi => {
                    ff::av_frame_unref(self.hw_frame);
                    check(
                        ff::av_hwframe_get_buffer(self.hw_frames, self.hw_frame, 0),
                        "VA surface",
                    )?;
                    check(
                        ff::av_hwframe_transfer_data(self.hw_frame, self.sw_frame, 0),
                        "uploading to the VA surface",
                    )?;
                    (*self.hw_frame).pts = self.pts;
                    (*self.hw_frame).pict_type = f.pict_type;
                    self.hw_frame
                }
            };
            self.pts += 1;
            check(ff::avcodec_send_frame(self.ctx, input), "encoding")?;
            let mut out = EncodedFrame::default();
            loop {
                let r = ff::avcodec_receive_packet(self.ctx, self.pkt);
                if r == ff::AVERROR(libc::EAGAIN) || r == ff::AVERROR_EOF {
                    break;
                }
                check(r, "reading the packet")?;
                let p = &*self.pkt;
                out.data
                    .extend_from_slice(std::slice::from_raw_parts(p.data, p.size as usize));
                out.keyframe |= p.flags & ff::AV_PKT_FLAG_KEY != 0;
                ff::av_packet_unref(self.pkt);
            }
            if !out.data.is_empty() {
                self.force_idr = false;
            }
            Ok(out)
        }
    }
}

impl Drop for FfEncoder {
    fn drop(&mut self) {
        // SAFETY: each object was allocated in `new` and is freed once.
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::av_frame_free(&mut self.sw_frame);
            ff::av_frame_free(&mut self.hw_frame);
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_buffer_unref(&mut self.hw_frames);
            ff::av_buffer_unref(&mut self.hw_device);
        }
    }
}

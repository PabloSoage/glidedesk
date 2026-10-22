//! Video encoding backends behind one interface.
//!
//! - NVENC: frames stay on the NVIDIA GPU (DMA-BUF import, CUDA colour conversion).
//! - FFmpeg: VA-API (AMD, Intel and other Mesa or Intel-media drivers) or x264 in
//!   software, fed by the CPU colour conversion; the capture goes through system memory.
//!
//! Each backend turns a captured frame into the H.264 pictures of one EGFX frame: the main
//! view (AVC420, or the first stream of AVC444v2) and optionally the auxiliary view.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use cudarc::driver::CudaContext;
use tracing::{info, warn};

use crate::color;
use crate::csc::{self, Nv12Frame};
use crate::ffenc::{FfEncoder, Kind};
use crate::frame::{Frame, FrameData};
use crate::nvenc::Encoder;

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
    /// Average QP of the picture, when the encoder reports it.
    pub qp: u32,
}

/// Which encoder to use (`--encoder`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Choice {
    /// NVENC if there is an NVIDIA GPU, else VA-API, else software.
    Auto,
    Nvenc,
    Vaapi,
    Software,
}

/// What one EGFX frame needs from the encoder.
pub struct Request {
    pub codec: Codec,
    pub avc444: bool,
    /// Encode the main view (not for an AVC444 chroma-only frame).
    pub main: bool,
    /// Encode the auxiliary view (AVC444 only).
    pub aux: bool,
    /// Constant QP for a quality pass, where supported.
    pub const_qp: Option<u32>,
}

pub trait Backend: Send {
    fn name(&self) -> &'static str;
    /// Whether AVC444 (both views through one encoder, two references) is available.
    fn supports_avc444(&self) -> bool;
    fn supports_hevc(&self) -> bool;
    /// `Request::const_qp` is honoured only where the rate control can switch in place
    /// (NVENC); elsewhere a quality pass is a plain re-encode.
    fn request_keyframe(&mut self);
    /// Encodes `frame`; empty pictures for views that were not asked for.
    fn encode(&mut self, frame: &Frame, req: &Request) -> Result<(EncodedFrame, EncodedFrame)>;
}

fn config(codec: Codec, w: u32, h: u32, avc444: bool, fps: u32, bitrate: u32) -> EncoderConfig {
    EncoderConfig {
        codec,
        width: w,
        height: h,
        // AVC444 sends two pictures per frame.
        fps: if avc444 { fps * 2 } else { fps },
        bitrate,
        refs: if avc444 { 2 } else { 1 },
    }
}

// ---------------------------------------------------------------------------------------

/// Codec and whether the two AVC444 views go through the encoder.
type EncoderKey = (Codec, bool);

pub struct Nvenc {
    ctx: Arc<CudaContext>,
    converter: color::Converter,
    /// Device buffers for frames that arrive in system memory.
    upload: crate::gpu::Pool,
    /// The encoder, what it was opened for (codec, AVC444) and its size.
    encoder: Option<(Encoder, EncoderKey, (u32, u32))>,
    fps: u32,
    bitrate: u32,
}

impl Nvenc {
    pub fn new(ctx: Arc<CudaContext>, fps: u32, bitrate: u32) -> Result<Self> {
        // Fail here, not on the first frame, when the driver has no NVENC.
        crate::nvenc::probe()?;
        Ok(Self {
            converter: color::Converter::new(Arc::clone(&ctx))?,
            upload: crate::gpu::Pool::new(Arc::clone(&ctx)),
            ctx,
            encoder: None,
            fps,
            bitrate,
        })
    }
}

impl Backend for Nvenc {
    fn name(&self) -> &'static str {
        "nvenc"
    }

    fn supports_avc444(&self) -> bool {
        true
    }

    fn supports_hevc(&self) -> bool {
        true
    }

    fn request_keyframe(&mut self) {
        if let Some((e, _, _)) = self.encoder.as_mut() {
            e.request_keyframe();
        }
    }

    fn encode(&mut self, frame: &Frame, req: &Request) -> Result<(EncodedFrame, EncodedFrame)> {
        let (w, h) = (frame.width, frame.height);
        let src = match &frame.data {
            FrameData::Gpu(b) => Arc::clone(b),
            FrameData::Cpu { data, stride } => {
                crate::gpu::upload(&mut self.upload, data, *stride as usize, w, h)?
            }
        };
        let size = color::coded_size(w, h);
        if self
            .encoder
            .as_ref()
            .is_none_or(|(_, k, s)| *s != size || *k != (req.codec, req.avc444))
        {
            let cfg = config(
                req.codec,
                size.0,
                size.1,
                req.avc444,
                self.fps,
                self.bitrate,
            );
            self.encoder = Some((
                Encoder::new(Arc::clone(&self.ctx), cfg)?,
                (req.codec, req.avc444),
                size,
            ));
            info!(w, h, codec = ?req.codec, avc444 = req.avc444, "NVENC encoder ready");
        }
        let (encoder, _, _) = self.encoder.as_mut().expect("created above");
        encoder.set_const_qp(req.const_qp)?;
        let (main, aux) = self.converter.convert(&src, req.aux)?;
        // Both views go through the same encoder as consecutive pictures: clients decode
        // AVC444 with a single H.264 decoder.
        let out_main = if req.main {
            encoder.encode_nv12(main)?
        } else {
            Default::default()
        };
        let out_aux = match aux {
            Some(aux) => encoder.encode_nv12(aux)?,
            None => Default::default(),
        };
        Ok((out_main, out_aux))
    }
}

// ---------------------------------------------------------------------------------------

pub struct Ffmpeg {
    kind: Kind,
    device: String,
    main: Option<Nv12Frame>,
    aux: Option<Nv12Frame>,
    encoder: Option<(FfEncoder, (Codec, bool))>,
    fps: u32,
    bitrate: u32,
    hevc: bool,
}

impl Ffmpeg {
    pub fn new(kind: Kind, device: String, fps: u32, bitrate: u32) -> Result<Self> {
        // Open a small encoder once so a missing driver or codec shows up at start.
        FfEncoder::new(
            kind,
            &device,
            config(Codec::H264, 256, 256, false, fps, bitrate),
        )
        .with_context(|| format!("{kind:?} encoder on {device}"))?;
        let hevc = FfEncoder::new(
            kind,
            &device,
            config(Codec::Hevc, 256, 256, false, fps, bitrate),
        )
        .is_ok();
        Ok(Self {
            hevc,
            kind,
            device,
            main: None,
            aux: None,
            encoder: None,
            fps,
            bitrate,
        })
    }
}

impl Backend for Ffmpeg {
    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Vaapi => "vaapi",
            Kind::Software => "software",
        }
    }

    fn supports_avc444(&self) -> bool {
        // VA-API drivers pick their own references; the interleaved views need each picture
        // to predict from the one two back, which only x264 can be told to search.
        self.kind == Kind::Software
    }

    fn supports_hevc(&self) -> bool {
        self.hevc
    }

    fn request_keyframe(&mut self) {
        if let Some((e, _)) = self.encoder.as_mut() {
            e.request_keyframe();
        }
    }

    fn encode(&mut self, frame: &Frame, req: &Request) -> Result<(EncodedFrame, EncodedFrame)> {
        let FrameData::Cpu { data, stride } = &frame.data else {
            bail!("the FFmpeg backend takes frames in system memory");
        };
        let (cw, ch) = color::coded_size(frame.width, frame.height);
        if self
            .main
            .as_ref()
            .is_none_or(|m| (m.width, m.height) != (cw, ch))
        {
            self.main = Some(Nv12Frame::new(cw, ch));
            self.aux = None;
            self.encoder = None;
        }
        if req.aux && self.aux.is_none() {
            self.aux = Some(Nv12Frame::new(cw, ch));
        }
        let key = (req.codec, req.avc444);
        if self.encoder.as_ref().is_none_or(|(_, k)| *k != key) {
            let cfg = config(req.codec, cw, ch, req.avc444, self.fps, self.bitrate);
            self.encoder = Some((FfEncoder::new(self.kind, &self.device, cfg)?, key));
        }
        let main = self.main.as_mut().expect("allocated above");
        let aux = if req.aux { self.aux.as_mut() } else { None };
        let t = std::time::Instant::now();
        csc::convert(data, *stride as usize, frame.width, frame.height, main, aux);
        let csc_us = t.elapsed().as_micros();
        let (encoder, _) = self.encoder.as_mut().expect("created above");
        let out_main = if req.main {
            encoder.encode(self.main.as_ref().expect("allocated above"))?
        } else {
            Default::default()
        };
        let out_aux = match (req.aux, self.aux.as_ref()) {
            (true, Some(aux)) => encoder.encode(aux)?,
            _ => Default::default(),
        };
        tracing::debug!(
            csc_us,
            encode_us = t.elapsed().as_micros() - csc_us,
            "FFmpeg frame"
        );
        Ok((out_main, out_aux))
    }
}

// ---------------------------------------------------------------------------------------

/// A backend and, for NVENC, the CUDA context the capture imports DMA-BUFs onto.
pub type Selected = (Box<dyn Backend>, Option<Arc<CudaContext>>);

/// First DRM render node, for VA-API.
pub fn default_render_node() -> String {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().display().to_string())
        .filter(|p| p.contains("renderD"))
        .collect();
    nodes.sort();
    nodes
        .into_iter()
        .next()
        .unwrap_or_else(|| "/dev/dri/renderD128".into())
}

/// The backend for `choice`, and the CUDA context when NVENC is used (the capture then
/// imports DMA-BUFs onto it).
pub fn select(
    choice: Choice,
    gpu: usize,
    render_node: Option<String>,
    fps: u32,
    bitrate: u32,
) -> Result<Selected> {
    let node = render_node.unwrap_or_else(default_render_node);
    let nvenc = || -> Result<Selected> {
        let ctx = CudaContext::new(gpu).context("creating the CUDA context")?;
        let b = Nvenc::new(Arc::clone(&ctx), fps, bitrate)?;
        Ok((Box::new(b), Some(ctx)))
    };
    let ffmpeg = |kind| -> Result<Selected> {
        Ok((
            Box::new(Ffmpeg::new(kind, node.clone(), fps, bitrate)?),
            None,
        ))
    };
    match choice {
        Choice::Nvenc => nvenc(),
        Choice::Vaapi => ffmpeg(Kind::Vaapi),
        Choice::Software => ffmpeg(Kind::Software),
        Choice::Auto => nvenc()
            .inspect_err(|e| warn!(%e, "No NVENC"))
            .or_else(|_| ffmpeg(Kind::Vaapi))
            .inspect_err(|e| warn!(%e, "No VA-API encoder, encoding on the CPU"))
            .or_else(|_| ffmpeg(Kind::Software)),
    }
}

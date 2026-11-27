//! Video encoding backends behind one interface.
//!
//! - NVENC: frames stay on the NVIDIA GPU (DMA-BUF import, CUDA colour conversion).
//! - FFmpeg: VA-API (AMD, Intel and other Mesa or Intel-media drivers) or x264 in
//!   software, fed by the CPU colour conversion; the capture goes through system memory,
//!   except VA-API with GNOME, which takes VA surfaces converted on the GPU
//!   ([`crate::vaimport`]), and AMF and QSV on Windows, which take D3D11 textures converted
//!   on the GPU ([`crate::wind3d`]).
//!
//! Each backend turns a captured frame into the H.264 pictures of one EGFX frame: the main
//! view (AVC420, or the first stream of AVC444v2) and optionally the auxiliary view; or into
//! one HEVC picture, 4:2:0 or (NVENC, native clients that ask) 4:4:4.

use std::sync::Arc;
use std::time::Duration;

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
    /// Keep extra pictures in the DPB so lost ones can be invalidated (NVENC, not AVC444).
    pub invalidation: bool,
    /// HEVC 4:4:4 (Range Extensions) from a planar YUV444 input (NVENC only).
    pub yuv444: bool,
    /// Take a QP offset map per picture ([`crate::region`], NVENC only).
    pub qp_map: bool,
}

#[derive(Default)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Average QP of the picture, when the encoder reports it.
    pub qp: u32,
    /// Encoder-wide picture number (never reused, even across encoders), for
    /// [`Backend::invalidate_from`]. Zero where references cannot be invalidated.
    pub index: u64,
    /// The first picture after an invalidation: it references only pictures before the
    /// lost ones, so a client that dropped them can decode it.
    pub recovery: bool,
}

/// Which encoder to use (`--encoder`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Choice {
    /// NVENC if there is an NVIDIA GPU, else VA-API/QSV/AMF, else software.
    Auto,
    Nvenc,
    Vaapi,
    Qsv,
    Amf,
    Software,
    /// Vulkan Video (experimental; built with the `vulkan-video` feature).
    #[cfg(feature = "vulkan-video")]
    Vulkan,
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
    /// HEVC 4:4:4 instead of 4:2:0, where [`Backend::supports_yuv444`].
    pub yuv444: bool,
    /// Quality by region from the frame's damage ([`crate::region`]); NVENC only, the same
    /// for the whole session. Not applied to quality passes.
    pub region_quality: bool,
}

pub trait Backend: Send {
    fn name(&self) -> &'static str;
    /// Whether AVC444 (both views through one encoder, two references) is available.
    fn supports_avc444(&self) -> bool;
    fn supports_hevc(&self) -> bool;
    /// Whether HEVC 4:4:4 may be asked for ([`Request::yuv444`]). The encoder still falls
    /// back to 4:2:0 when the GPU turns out not to have it.
    fn supports_yuv444(&self) -> bool {
        false
    }
    /// `Request::const_qp` is honoured only where the rate control can switch in place
    /// (NVENC); elsewhere a quality pass is a plain re-encode.
    fn request_keyframe(&mut self);
    /// The bitrate ceiling the link allows (bits per second), from the rate control.
    fn set_max_bitrate(&mut self, bps: u32) -> Result<()>;
    /// Encodes `frame`; empty pictures for views that were not asked for.
    fn encode(&mut self, frame: &Frame, req: &Request) -> Result<(EncodedFrame, EncodedFrame)>;
    /// The client lost picture `index` ([`EncodedFrame::index`]) and the ones after it:
    /// stop referencing them, so the next picture decodes without a keyframe. False when the
    /// encoder cannot (then a keyframe is needed).
    fn invalidate_from(&mut self, _index: u64) -> bool {
        false
    }
    /// The render node the capture should import DMA-BUFs onto as VA surfaces (VA-API
    /// encoders), if any.
    fn vaapi_import(&self) -> Option<String> {
        None
    }
    /// The encoder takes the desktop as Direct3D 11 textures (AMF, QSV on Windows).
    fn d3d11_input(&self) -> bool {
        false
    }
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
        invalidation: !avc444,
        yuv444: false,
        qp_map: false,
    }
}

// ---------------------------------------------------------------------------------------

/// Codec, whether the two AVC444 views go through the encoder, and HEVC 4:4:4.
type EncoderKey = (Codec, bool, bool);

pub struct Nvenc {
    ctx: Arc<CudaContext>,
    converter: color::Converter,
    /// Device buffers for frames that arrive in system memory.
    upload: crate::gpu::Pool,
    /// The encoder, what it was opened for (codec, AVC444) and its size.
    encoder: Option<(Encoder, EncoderKey, (u32, u32))>,
    /// The QP offsets of the picture being encoded (quality by region).
    qp_map: Vec<i8>,
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
            qp_map: Vec::new(),
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

    fn supports_yuv444(&self) -> bool {
        // Needs the 4:4:4 kernel in color.ptx.
        self.converter.has_yuv444()
    }

    fn request_keyframe(&mut self) {
        if let Some((e, _, _)) = self.encoder.as_mut() {
            e.request_keyframe();
        }
    }

    fn invalidate_from(&mut self, index: u64) -> bool {
        match self.encoder.as_mut() {
            Some((e, _, _)) => e.invalidate_from(index),
            None => false,
        }
    }

    fn set_max_bitrate(&mut self, bps: u32) -> Result<()> {
        self.bitrate = bps;
        match self.encoder.as_mut() {
            Some((e, _, _)) => e.set_max_bitrate(bps),
            None => Ok(()),
        }
    }

    fn encode(&mut self, frame: &Frame, req: &Request) -> Result<(EncodedFrame, EncodedFrame)> {
        let (w, h) = (frame.width, frame.height);
        let src = match &frame.data {
            FrameData::Gpu(b) => Arc::clone(b),
            FrameData::Cpu { data, stride } => {
                let t = std::time::Instant::now();
                let b = crate::gpu::upload(&mut self.upload, data, *stride as usize, w, h)?;
                tracing::debug!(upload_ms = t.elapsed().as_secs_f64() * 1e3, "Upload");
                b
            }
            #[cfg(feature = "gnome")]
            FrameData::Va(_) => bail!("NVENC takes CUDA or system-memory frames"),
            #[cfg(target_os = "windows")]
            FrameData::D3d(_) => bail!("NVENC takes CUDA or system-memory frames"),
        };
        let size = color::coded_size(w, h);
        let yuv444 =
            req.yuv444 && req.codec == Codec::Hevc && !req.avc444 && self.converter.has_yuv444();
        let key = (req.codec, req.avc444, yuv444);
        if self
            .encoder
            .as_ref()
            .is_none_or(|(_, k, s)| *s != size || *k != key)
        {
            let mut cfg = config(
                req.codec,
                size.0,
                size.1,
                req.avc444,
                self.fps,
                self.bitrate,
            );
            cfg.yuv444 = yuv444;
            cfg.qp_map = req.region_quality;
            // The old session goes first: the driver crashes (in a thread of its own) when a
            // session is destroyed after another one was opened on the same context.
            self.encoder = None;
            self.encoder = Some((Encoder::new(Arc::clone(&self.ctx), cfg)?, key, size));
            info!(w, h, codec = ?req.codec, avc444 = req.avc444, yuv444, "NVENC encoder ready");
        }
        let (encoder, _, _) = self.encoder.as_mut().expect("created above");
        encoder.set_const_qp(req.const_qp)?;
        if let Some(block) = encoder.qp_block() {
            // A quality pass re-encodes a still image: its damage is old news.
            let damage = frame.damage.as_deref().filter(|_| req.const_qp.is_none());
            crate::region::qp_map(size.0, size.1, block, damage, &mut self.qp_map);
            encoder.set_qp_map(&self.qp_map);
        }
        if encoder.yuv444() {
            // One full-chroma picture; never an AVC444 frame (see `yuv444` above).
            let picture = self.converter.convert_yuv444(&src)?;
            return Ok((encoder.encode_yuv444(picture)?, Default::default()));
        }
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
    /// The encoder and what it was opened for: codec, AVC444, and the frames context of the
    /// capture's VA surfaces (its address; 0 for frames from system memory).
    encoder: Option<(FfEncoder, (Codec, bool, usize))>,
    /// The encoder's D3D11 or QSV frames on the capture's device (AMF and QSV, Windows).
    #[cfg(target_os = "windows")]
    d3d: Option<crate::wind3d::D3dFrames>,
    fps: u32,
    bitrate: u32,
    hevc: bool,
}

impl Ffmpeg {
    pub fn new(kind: Kind, device: String, fps: u32, bitrate: u32) -> Result<Self> {
        // Encode a few small frames once so a missing driver or codec shows up at start.
        trial(
            kind,
            &device,
            config(Codec::H264, 256, 256, false, fps, bitrate),
        )
        .with_context(|| format!("{kind:?} encoder on {device}"))?;
        let hevc = match trial(
            kind,
            &device,
            config(Codec::Hevc, 256, 256, false, fps, bitrate),
        ) {
            Ok(()) => true,
            Err(e) => {
                info!(error = format!("{e:#}"), "No HEVC from this encoder");
                false
            }
        };
        Ok(Self {
            hevc,
            kind,
            device,
            main: None,
            aux: None,
            encoder: None,
            #[cfg(target_os = "windows")]
            d3d: None,
            fps,
            bitrate,
        })
    }
}

/// How long a trial may take: Mesa's d3d12 VA-API driver (WSL, 26.0) opens HEVC encoders
/// but never finishes the second frame. (Opening the first one can take a few seconds.)
const TRIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Opens an encoder and encodes a few frames on a thread of its own, within
/// [`TRIAL_TIMEOUT`]. A driver that hangs keeps that thread; the encoder is not used.
///
/// The thread never ends: VA drivers leave thread-local destructors behind, and closing the
/// encoder unloads the driver, so a thread exiting afterwards would jump into unmapped code.
fn trial(kind: Kind, device: &str, cfg: EncoderConfig) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let device = device.to_owned();
    std::thread::Builder::new()
        .name("encoder-trial".into())
        .spawn(move || {
            let r = (|| {
                let frame = Nv12Frame::new(cfg.width, cfg.height);
                let mut enc = FfEncoder::new(kind, &device, cfg)?;
                for _ in 0..4 {
                    enc.encode(&frame)?;
                }
                anyhow::Ok(())
            })();
            let _ = tx.send(r);
            loop {
                std::thread::park();
            }
        })?;
    match rx.recv_timeout(TRIAL_TIMEOUT) {
        Ok(r) => r,
        Err(_) => bail!("the driver did not finish a test frame in {TRIAL_TIMEOUT:?}"),
    }
}

impl Backend for Ffmpeg {
    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Vaapi => "vaapi",
            Kind::Qsv => "qsv",
            Kind::Amf => "amf",
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

    fn vaapi_import(&self) -> Option<String> {
        (self.kind == Kind::Vaapi).then(|| self.device.clone())
    }

    fn d3d11_input(&self) -> bool {
        cfg!(target_os = "windows") && matches!(self.kind, Kind::Amf | Kind::Qsv)
    }

    fn request_keyframe(&mut self) {
        match self.encoder.as_mut() {
            Some((e, _)) if e.idr_on_request() => e.request_keyframe(),
            // AMF on FFmpeg 6: the next frame opens a new encoder, which starts with an IDR.
            Some(_) => self.encoder = None,
            None => {}
        }
    }

    fn invalidate_from(&mut self, _index: u64) -> bool {
        // FFmpeg's AMF, QSV and VA-API encoders cannot stop referencing a picture (see
        // `ffenc`): the pipeline asks for a keyframe instead.
        false
    }

    fn set_max_bitrate(&mut self, bps: u32) -> Result<()> {
        // FFmpeg encoders take their rate when opened: reopen (a new IDR) only when the
        // ceiling moved a lot, not on every small step of the rate control.
        let ratio = f64::from(bps) / f64::from(self.bitrate.max(1));
        if !(0.67..=1.5).contains(&ratio) {
            self.bitrate = bps;
            self.encoder = None;
        }
        Ok(())
    }

    fn encode(&mut self, frame: &Frame, req: &Request) -> Result<(EncodedFrame, EncodedFrame)> {
        #[cfg(feature = "gnome")]
        if let FrameData::Va(va) = &frame.data {
            return self.encode_va(frame.width, frame.height, va, req);
        }
        #[cfg(target_os = "windows")]
        if let FrameData::D3d(texture) = &frame.data {
            return self.encode_d3d(texture, req).inspect_err(|e| {
                warn!(error = format!("{e:#}"), "Encoding from D3D11 failed");
                crate::wind3d::give_up();
            });
        }
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
        let key = (req.codec, req.avc444, 0);
        if self.encoder.as_ref().is_none_or(|(_, k)| *k != key) {
            let cfg = config(req.codec, cw, ch, req.avc444, self.fps, self.bitrate);
            self.encoder = None;
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

impl Ffmpeg {
    /// Encodes a VA surface the capture converted on the GPU. The encoder works at the
    /// capture size (SPS cropping): the surfaces are not padded to multiples of 16.
    #[cfg(feature = "gnome")]
    fn encode_va(
        &mut self,
        width: u32,
        height: u32,
        va: &crate::vaimport::VaFrame,
        req: &Request,
    ) -> Result<(EncodedFrame, EncodedFrame)> {
        if self.kind != Kind::Vaapi || req.avc444 {
            bail!("VA surfaces go to the VA-API encoder, 4:2:0 only");
        }
        let key = (req.codec, false, va.frames_data() as usize);
        if self.encoder.as_ref().is_none_or(|(_, k)| *k != key) {
            let cfg = config(req.codec, width, height, false, self.fps, self.bitrate);
            self.encoder = None;
            let encoder = FfEncoder::new_on(self.kind, &self.device, cfg, Some(va.frames_ctx()))?;
            self.encoder = Some((encoder, key));
            info!(width, height, "VA-API encoder on the capture's surfaces");
        }
        let (encoder, _) = self.encoder.as_mut().expect("created above");
        let t = std::time::Instant::now();
        let main = if req.main {
            encoder.encode_va(va)?
        } else {
            Default::default()
        };
        tracing::debug!(encode_us = t.elapsed().as_micros(), "VA-API frame");
        Ok((main, Default::default()))
    }

    /// Converts the desktop texture into an encoder frame on the GPU and encodes it (AMF,
    /// QSV). An error sends the capture back to system memory (the caller gives up).
    #[cfg(target_os = "windows")]
    fn encode_d3d(
        &mut self,
        texture: &crate::wind3d::D3dTexture,
        req: &Request,
    ) -> Result<(EncodedFrame, EncodedFrame)> {
        if !matches!(self.kind, Kind::Amf | Kind::Qsv) || req.avc444 {
            bail!("D3D11 textures go to AMF or QSV, 4:2:0 only");
        }
        let (cw, ch) = color::coded_size(texture.width, texture.height);
        if self
            .d3d
            .as_ref()
            .is_none_or(|d| !d.fits(&texture.texture, cw, ch))
        {
            self.encoder = None;
            self.d3d = None;
            self.d3d = Some(crate::wind3d::D3dFrames::new(
                self.kind == Kind::Qsv,
                &texture.texture,
                cw,
                ch,
            )?);
        }
        let d3d = self.d3d.as_mut().expect("made above");
        let key = (req.codec, false, d3d.frames_data() as usize);
        if self.encoder.as_ref().is_none_or(|(_, k)| *k != key) {
            let cfg = config(req.codec, cw, ch, false, self.fps, self.bitrate);
            self.encoder = None;
            let encoder = FfEncoder::new_on(self.kind, &self.device, cfg, Some(d3d.frames_ctx()))?;
            self.encoder = Some((encoder, key));
        }
        let t = std::time::Instant::now();
        let picture = d3d.convert(texture)?;
        let convert_us = t.elapsed().as_micros();
        let (encoder, _) = self.encoder.as_mut().expect("created above");
        let main = if req.main {
            // SAFETY: a frame of the context the encoder was opened on, alive until after.
            unsafe { encoder.encode_hw(picture.as_ptr())? }
        } else {
            Default::default()
        };
        tracing::debug!(
            convert_us,
            encode_us = t.elapsed().as_micros() - convert_us,
            "D3D11 frame"
        );
        Ok((main, Default::default()))
    }
}

// ---------------------------------------------------------------------------------------

/// A backend and, for NVENC, the CUDA context the capture imports DMA-BUFs onto.
pub type Selected = (Box<dyn Backend>, Option<Arc<CudaContext>>);

/// First DRM render node, for VA-API (Linux) or default device name.
pub fn default_render_node() -> String {
    #[cfg(unix)]
    {
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
    #[cfg(not(unix))]
    {
        "".into()
    }
}

/// One more backend of the kind [`select`] chose (`name` is its [`Backend::name`], `hevc`
/// its [`Backend::supports_hevc`]), for the stream of a further monitor. NVENC goes on the
/// same CUDA context, which the captures import onto; FFmpeg skips the start-up trial the
/// first backend already passed.
pub fn another(
    name: &str,
    hevc: bool,
    cuda: Option<&Arc<CudaContext>>,
    render_node: Option<String>,
    fps: u32,
    bitrate: u32,
) -> Result<Box<dyn Backend>> {
    let kind = match name {
        "nvenc" => {
            let ctx = cuda.context("NVENC without a CUDA context")?;
            return Ok(Box::new(Nvenc::new(Arc::clone(ctx), fps, bitrate)?));
        }
        "vaapi" => Kind::Vaapi,
        "qsv" => Kind::Qsv,
        "amf" => Kind::Amf,
        "software" => Kind::Software,
        other => bail!("no encoder named {other}"),
    };
    Ok(Box::new(Ffmpeg {
        hevc,
        kind,
        device: render_node.unwrap_or_else(default_render_node),
        main: None,
        aux: None,
        encoder: None,
        #[cfg(target_os = "windows")]
        d3d: None,
        fps,
        bitrate,
    }))
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
        Choice::Qsv => ffmpeg(Kind::Qsv),
        Choice::Amf => ffmpeg(Kind::Amf),
        Choice::Software => ffmpeg(Kind::Software),
        #[cfg(feature = "vulkan-video")]
        Choice::Vulkan => Ok((
            Box::new(crate::vkenc::Vulkan::new(gpu, fps, bitrate)?),
            None,
        )),
        Choice::Auto => {
            #[cfg(target_os = "windows")]
            {
                nvenc()
                    .inspect_err(|e| warn!(%e, "No NVENC"))
                    .or_else(|_| ffmpeg(Kind::Qsv))
                    .inspect_err(|e| warn!(%e, "No QSV"))
                    .or_else(|_| ffmpeg(Kind::Amf))
                    .inspect_err(|e| warn!(%e, "No AMF, encoding on the CPU"))
                    .or_else(|_| ffmpeg(Kind::Software))
            }
            #[cfg(not(target_os = "windows"))]
            {
                nvenc()
                    .inspect_err(|e| warn!(%e, "No NVENC"))
                    .or_else(|_| ffmpeg(Kind::Vaapi))
                    .inspect_err(|e| warn!(%e, "No VA-API encoder, encoding on the CPU"))
                    .or_else(|_| ffmpeg(Kind::Software))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes synthetic frames from system memory with each encoder this machine has:
    /// `cargo test -p glidedesk-host encoders_on_this_machine -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn encoders_on_this_machine() {
        let (w, h) = (1920u32, 1080u32);
        let choices = [
            Choice::Nvenc,
            Choice::Vaapi,
            Choice::Qsv,
            Choice::Amf,
            Choice::Software,
        ];
        for choice in choices {
            let mut backend = match select(choice, 0, None, 60, 20_000_000) {
                Ok((b, _)) => b,
                Err(e) => {
                    println!("{choice:?}: not available ({e:#})");
                    continue;
                }
            };
            for codec in [Codec::H264, Codec::Hevc] {
                if codec == Codec::Hevc && !backend.supports_hevc() {
                    println!("{choice:?} {codec:?}: not offered");
                    continue;
                }
                backend.request_keyframe();
                let start = std::time::Instant::now();
                let (mut bytes, mut first_key) = (0, None);
                for i in 0..60u32 {
                    // A gradient that moves: every frame differs.
                    let mut data = vec![0u8; (w * h * 4) as usize];
                    for (p, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let (x, y) = (p as u32 % w, p as u32 / w);
                        *px = [(x + i * 8) as u8, (y + i * 4) as u8, (x ^ y) as u8, 0];
                    }
                    let frame = Frame {
                        width: w,
                        height: h,
                        data: FrameData::Cpu {
                            data,
                            stride: w * 4,
                        },
                        damage: None,
                        captured: std::time::Instant::now(),
                        recycle: None,
                    };
                    let req = Request {
                        codec,
                        avc444: false,
                        main: true,
                        aux: false,
                        const_qp: None,
                        yuv444: false,
                        region_quality: false,
                    };
                    let (main, _) = backend.encode(&frame, &req).expect("encoding");
                    assert!(
                        !main.data.is_empty(),
                        "{choice:?} {codec:?}: empty picture {i}"
                    );
                    first_key.get_or_insert(main.keyframe);
                    bytes += main.data.len();
                }
                assert_eq!(
                    first_key,
                    Some(true),
                    "{choice:?} {codec:?}: first picture not a keyframe"
                );
                println!(
                    "{} {codec:?}: 60 frames 1080p in {:.0} ms (with the CPU colour conversion), {} kB",
                    backend.name(),
                    start.elapsed().as_secs_f64() * 1000.0,
                    bytes / 1000
                );
            }
        }
    }
}

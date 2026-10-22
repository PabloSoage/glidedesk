//! The encode loop shared by the front ends.
//!
//! One thread takes the newest captured frame, decides what to send (H-002 views, quality
//! passes over a still image), encodes it through the [`encoder::Backend`] and hands the
//! pictures to the [`Output`] of whichever front end has the client: RDP (EGFX) or the
//! native protocol. The last client to connect, on either, gets the session.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::encoder::{self, Codec, EncodedFrame};
use crate::frame::{Frame, FrameData, FrameReceiver, Source};

/// State shared by the front ends and the pipeline thread.
#[derive(Default)]
pub struct Shared {
    /// Size a client asked for (initial size or a resize).
    pub wanted_size: Mutex<Option<(u32, u32)>>,
    /// Size currently being captured.
    pub current_size: Mutex<(u32, u32)>,
    pub stop: AtomicBool,
    /// Bumped when a client takes over the session; the pipeline then starts over (new
    /// surface, IDR) on the output in `active`.
    generation: AtomicU64,
    active: AtomicUsize,
}

impl Shared {
    /// A client of output `index` is ready for frames.
    pub fn take_over(&self, index: usize) {
        self.active.store(index, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Asks for a new desktop size (even, within what the encoders and RDP accept).
    pub fn request_size(&self, w: u32, h: u32) -> (u32, u32) {
        let size = clamp_size(w, h);
        *self.wanted_size.lock().unwrap() = Some(size);
        size
    }
}

/// Even size within what NVENC and MS-RDPEDISP accept.
pub fn clamp_size(w: u32, h: u32) -> (u32, u32) {
    let w = w.clamp(200, 4096) & !1;
    let h = h.clamp(200, 4096) & !1;
    (w, h)
}

/// What an AVC444v2 frame carries (MS-RDPEGFX 2.2.4.5, the LC field). Other outputs only
/// ever get `Full`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Both views (LC=0), or the only picture of a 4:2:0 stream.
    Full,
    /// Main view only (LC=1): luma and 4:2:0 chroma, half the encoding work.
    LumaOnly,
    /// Auxiliary view only (LC=2): the rest of the chroma, for a still image.
    ChromaOnly,
}

pub enum Prepared {
    /// No client on this output.
    Absent,
    /// The client has as many frames as it may have in flight.
    Busy,
    Ready {
        /// A new surface (first frame, new size): the next picture must be an IDR.
        new_surface: bool,
        /// The client takes AVC444v2.
        avc444: bool,
        codec: Codec,
    },
}

/// A front end's side of the pipeline.
pub trait Output: Send {
    /// Gets ready for a frame of `w`×`h`.
    fn prepare(&mut self, w: u32, h: u32, max_in_flight: u32) -> Result<Prepared>;
    /// Sends the pictures of one frame; false if the client could not take it after all.
    fn send(&mut self, mode: Mode, main: &EncodedFrame, aux: &EncodedFrame) -> Result<bool>;
    /// Frames the client acknowledged since the last call.
    fn take_acked(&mut self) -> u64;
    /// The client changed: forget its surface.
    fn reset(&mut self);
    /// The client needs the current image again (it lost a frame and asked for an IDR),
    /// even if nothing changed on screen.
    fn wants_refresh(&self) -> bool {
        false
    }
}

pub struct Options {
    /// Send AVC444v2 (4:4:4) when the client supports it.
    pub avc444: bool,
    /// H-002: luma-only frames while things move, chroma once still.
    pub hybrid: bool,
    /// Re-encode the still image after motion until its quality settles.
    pub refine: bool,
    /// Frames sent and not yet acknowledged; beyond this the newest frame waits.
    pub max_in_flight: u32,
}

pub struct Pipeline {
    shared: std::sync::Arc<Shared>,
    source: Box<dyn Source>,
    frames: FrameReceiver,
    backend: Box<dyn encoder::Backend>,
    outputs: Vec<Box<dyn Output>>,
    opts: Options,
    generation: u64,
    last_frame: Option<Frame>,
    /// The newest frame could not be sent (back pressure) and is waiting for a slot.
    pending: bool,
    /// When the last new frame arrived from the compositor.
    last_new_frame: Instant,
    /// Frames went out luma-only (AVC444 LC=1) while things moved; the chroma of the
    /// still image is owed (LC=2).
    chroma_owed: bool,
    /// The next AVC444 frame must carry both views (after a new surface).
    need_full: bool,
    /// Quality passes left over the still image (see `REFINE_QPS`).
    refine_left: u32,
    /// QP of the quality pass being sent; `None` for normal frames.
    refine_qp: Option<u32>,
    stats: Stats,
}

struct Stats {
    at: Instant,
    frames: u32,
    encode_us: u128,
    bytes: usize,
    refines: u32,
}

impl Pipeline {
    pub fn new(
        shared: std::sync::Arc<Shared>,
        source: Box<dyn Source>,
        frames: FrameReceiver,
        backend: Box<dyn encoder::Backend>,
        outputs: Vec<Box<dyn Output>>,
        opts: Options,
    ) -> Self {
        Self {
            shared,
            source,
            frames,
            backend,
            outputs,
            opts,
            generation: 0,
            last_frame: None,
            pending: false,
            last_new_frame: Instant::now(),
            chroma_owed: false,
            need_full: true,
            refine_left: 0,
            refine_qp: None,
            stats: Stats {
                at: Instant::now(),
                frames: 0,
                encode_us: 0,
                bytes: 0,
                refines: 0,
            },
        }
    }

    pub fn run(mut self) -> Result<()> {
        while !self.shared.stop.load(Ordering::Relaxed) {
            if let Some((w, h)) = self.shared.wanted_size.lock().unwrap().take() {
                let cur = *self.shared.current_size.lock().unwrap();
                if self.last_frame.as_ref().map(|f| (f.width, f.height)) != Some((w, h))
                    || cur != (w, h)
                {
                    self.source.resize(w, h);
                    *self.shared.current_size.lock().unwrap() = (w, h);
                }
            }

            let generation = self.shared.generation.load(Ordering::SeqCst);
            let new_client = generation != self.generation;
            let wait = if self.pending {
                2
            } else if self.refine_left > 0 {
                REFINE_INTERVAL.as_millis() as u64
            } else {
                50
            };
            let frame = match self.frames.recv_timeout(Duration::from_millis(wait)) {
                Ok(f) => Some(f),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let mut mode = Mode::Full;
            if let Some(f) = frame {
                // H-002: frames closer than this are motion; they go luma-only and the
                // full chroma follows once the image is still.
                if self.opts.hybrid && self.last_new_frame.elapsed() < MOTION_GAP {
                    mode = Mode::LumaOnly;
                }
                self.last_new_frame = Instant::now();
                debug_dump_frame(&f);
                self.last_frame = Some(f);
                self.refine_left = if self.opts.refine {
                    REFINE_QPS.len() as u32
                } else {
                    0
                };
            } else if self.pending {
                // Retrying a frame held back by back pressure: same choice as when it came in.
                if self.opts.hybrid && self.last_new_frame.elapsed() < MOTION_GAP {
                    mode = Mode::LumaOnly;
                }
            } else if self.chroma_owed && self.last_new_frame.elapsed() >= STILL_AFTER {
                mode = Mode::ChromaOnly;
            } else if self.refine_left > 0 && self.last_new_frame.elapsed() >= REFINE_AFTER {
                // The image is still: frames encoded during motion got one frame's worth of
                // bits each and the compositor sends nothing new, so the client would keep
                // that quality. Encode the same image again at fixed, finer QPs.
                self.refine_left -= 1;
                self.refine_qp = Some(REFINE_QPS[REFINE_QPS.len() - self.refine_left as usize - 1]);
            } else if !new_client && !self.active_wants_refresh() {
                continue;
            }
            // Mutter only sends frames on damage: a client that connects to an idle
            // desktop gets the last frame again.
            let Some(frame) = self.last_frame.take() else {
                continue;
            };
            if new_client {
                self.generation = generation;
                for o in &mut self.outputs {
                    o.reset();
                }
                mode = Mode::Full;
            }
            if let Err(e) = self.send(&frame, mode) {
                warn!(%e, "Sending a frame failed");
            }
            self.refine_qp = None;
            self.last_frame = Some(frame);
        }
        Ok(())
    }

    fn active_wants_refresh(&self) -> bool {
        let index = self.shared.active.load(Ordering::SeqCst);
        self.outputs.get(index).is_some_and(|o| o.wants_refresh())
    }

    fn send(&mut self, frame: &Frame, mode: Mode) -> Result<()> {
        let index = self.shared.active.load(Ordering::SeqCst);
        let Some(output) = self.outputs.get_mut(index) else {
            return Ok(());
        };
        let (avc444, codec) =
            match output.prepare(frame.width, frame.height, self.opts.max_in_flight)? {
                Prepared::Absent => return Ok(()),
                Prepared::Busy => {
                    // Skip before encoding: a P-frame that is encoded but never sent would
                    // break the reference chain on the client. The run loop retries this
                    // frame every few milliseconds until an acknowledgement frees a slot.
                    self.pending = true;
                    return Ok(());
                }
                Prepared::Ready {
                    new_surface,
                    avc444,
                    codec,
                } => {
                    if new_surface {
                        self.need_full = true;
                        self.backend.request_keyframe();
                    }
                    (
                        avc444 && self.opts.avc444 && self.backend.supports_avc444(),
                        codec,
                    )
                }
            };
        // Which AVC444 views this frame carries (H-002): luma-only while things move,
        // chroma-only once still, both otherwise. The first frame on a surface has both.
        let mode = if !avc444 || self.need_full {
            Mode::Full
        } else {
            mode
        };
        let t = Instant::now();
        let (main, aux) = self.backend.encode(
            frame,
            &encoder::Request {
                codec,
                avc444,
                main: mode != Mode::ChromaOnly,
                aux: avc444 && mode != Mode::LumaOnly,
                const_qp: self.refine_qp,
            },
        )?;
        let encode_us = t.elapsed().as_micros();
        debug!(bytes = main.data.len(), qp = main.qp, ?mode, "Frame");
        if !output.send(mode, &main, &aux)? {
            return Ok(());
        }
        self.need_full = false;
        self.chroma_owed = avc444 && mode == Mode::LumaOnly;
        self.pending = false;

        let bytes = main.data.len() + aux.data.len();
        let s = &mut self.stats;
        s.frames += 1;
        s.encode_us += encode_us;
        s.bytes += bytes;
        if let Some(qp) = self.refine_qp {
            debug!(bytes, qp, used_qp = main.qp, "Quality pass");
            s.refines += 1;
        }
        if s.at.elapsed() >= Duration::from_secs(5) {
            let secs = s.at.elapsed().as_secs_f64();
            let acked = output.take_acked();
            info!(
                fps = format!("{:.1}", f64::from(s.frames) / secs),
                client_fps = format!("{:.1}", acked as f64 / secs),
                mbps = format!("{:.1}", s.bytes as f64 * 8.0 / secs / 1e6),
                encode_ms = format!(
                    "{:.2}",
                    s.encode_us as f64 / f64::from(s.frames.max(1)) / 1000.0
                ),
                capture_to_queue_ms =
                    format!("{:.2}", frame.captured.elapsed().as_secs_f64() * 1000.0),
                refines = s.refines,
                ?codec,
                "Stream"
            );
            *s = Stats {
                at: Instant::now(),
                frames: 0,
                encode_us: 0,
                bytes: 0,
                refines: 0,
            };
        }
        Ok(())
    }
}

/// GLIDEDESK_DEBUG_FRAME=PATH: keep the newest captured frame (system-memory captures only)
/// in PATH as raw BGRx, the lossless reference for image quality tests.
fn debug_dump_frame(f: &Frame) {
    static PATH: std::sync::LazyLock<Option<String>> =
        std::sync::LazyLock::new(|| std::env::var("GLIDEDESK_DEBUG_FRAME").ok());
    let (Some(path), FrameData::Cpu { data, stride }) = (PATH.as_ref(), &f.data) else {
        return;
    };
    let row = f.width as usize * 4;
    let mut out = Vec::with_capacity(row * f.height as usize);
    for y in 0..f.height as usize {
        let start = y * *stride as usize;
        out.extend_from_slice(&data[start..start + row]);
    }
    let _ = std::fs::write(path, out);
}

/// Frames that arrive closer together than this count as motion (H-002).
const MOTION_GAP: Duration = Duration::from_millis(50);
/// After this long without a new frame, the image counts as still and its full chroma is
/// sent.
const STILL_AFTER: Duration = Duration::from_millis(60);
/// Quality passes over a still image: once nothing new has arrived for `REFINE_AFTER`, the
/// image is encoded again at these constant QPs, one pass every `REFINE_INTERVAL`.
/// The low-latency rate control cannot do it: it gives a static image the same coarse QP.
const REFINE_QPS: [u32; 2] = [20, 14];
const REFINE_AFTER: Duration = Duration::from_millis(120);
const REFINE_INTERVAL: Duration = Duration::from_millis(16);

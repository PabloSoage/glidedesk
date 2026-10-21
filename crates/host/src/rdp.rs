//! RDP front end: IronRDP server, EGFX (H.264 AVC420) fed by NVENC, Display Control for
//! resizing.
//!
//! Threads: the tokio runtime runs IronRDP and the input task; the PipeWire thread captures;
//! the pipeline thread encodes the newest frame and queues it on the EGFX channel of the
//! current connection.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet, Encoding};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_server::{
    ConnectionHandler, ConnectionInfo, Credentials, DesktopSize, DisplayContext, DisplayUpdate,
    EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle, LargePointer,
    PostConnectionAction, RGBAPointer, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates,
    ServerEvent, ServerEventSender, ServerResult, TlsIdentityCtx,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use cudarc::driver::CudaContext;

use crate::capture::{Capture, CursorEvent, Frame, FrameData, Sinks};
use crate::color;
use crate::input::InputForwarder;
use crate::mutter;
use crate::nvenc::{Codec, Encoder, EncoderConfig};

pub struct ServeOptions {
    /// Use AVC444v2 (full chroma) when the client supports it.
    pub avc444: bool,
    /// AVC444 sends luma-only frames during motion and the chroma once the image is still.
    pub hybrid: bool,
    /// Unacknowledged frames allowed; more frames than the client can show only queue up.
    pub max_in_flight: u32,
    pub bind: SocketAddr,
    pub gpu: usize,
    pub bitrate: u32,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub username: String,
    pub password: String,
    pub state_dir: PathBuf,
}

/// State shared by the IronRDP callbacks and the pipeline thread.
#[derive(Default)]
struct Shared {
    /// EGFX server of the current connection, replaced on every new connection.
    gfx: Mutex<Option<GfxServerHandle>>,
    /// Bumped when a new EGFX channel becomes ready; the pipeline then starts over with a
    /// new surface and an IDR.
    gfx_generation: AtomicU64,
    /// Frames the client acknowledged as decoded (MS-RDPEGFX frame acknowledge).
    acked: AtomicU64,
    events: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// Size the client asked for (initial size or Display Control layout).
    wanted_size: Mutex<Option<(u32, u32)>>,
    /// Size currently being captured.
    current_size: Mutex<(u32, u32)>,
    stop: AtomicBool,
}

// ---------------------------------------------------------------------------------------
// EGFX

struct GfxFactory(Arc<Shared>);

impl ServerEventSender for GfxFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.0.events.lock().unwrap() = Some(sender);
    }
}

impl GfxServerFactory for GfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GfxHandler(Arc::clone(&self.0)))
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server = Arc::new(Mutex::new(GraphicsPipelineServer::new(
            self.build_gfx_handler(),
        )));
        *self.0.gfx.lock().unwrap() = Some(Arc::clone(&server));
        Some((GfxDvcBridge::new(Arc::clone(&server)), server))
    }
}

struct GfxHandler(Arc<Shared>);

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        debug!(?pdu, "Client EGFX capabilities");
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        info!(?negotiated, "EGFX channel ready");
        self.0.gfx_generation.fetch_add(1, Ordering::SeqCst);
    }

    fn on_frame_ack(&mut self, _frame_id: u32, _queue_depth: u32, _total_frames_decoded: u32) {
        self.0.acked.fetch_add(1, Ordering::Relaxed);
    }

    fn on_close(&mut self) {
        info!("EGFX channel closed");
    }
}

// ---------------------------------------------------------------------------------------
// Display

/// Even size within what NVENC and MS-RDPEDISP accept.
fn clamp_size(w: u32, h: u32) -> (u32, u32) {
    let w = w.clamp(200, 4096) & !1;
    let h = h.clamp(200, 4096) & !1;
    (w, h)
}

struct Display {
    shared: Arc<Shared>,
    cursor: tokio::sync::watch::Receiver<Option<CursorEvent>>,
}

/// Graphics go through EGFX; the classic update stream only carries the pointer, which
/// the client then draws itself with no round trip.
struct PointerUpdates {
    cursor: tokio::sync::watch::Receiver<Option<CursorEvent>>,
    /// Send the current shape first on every new connection.
    first: bool,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for PointerUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        loop {
            if !std::mem::take(&mut self.first) && self.cursor.changed().await.is_err() {
                return Ok(None);
            }
            let ev = self.cursor.borrow_and_update().clone();
            let update = match ev {
                None => continue,
                Some(CursorEvent::Hidden) => DisplayUpdate::HidePointer,
                Some(CursorEvent::Shape(s)) if s.width <= 96 && s.height <= 96 => {
                    DisplayUpdate::RGBAPointer(RGBAPointer {
                        cache_index: 0,
                        width: s.width,
                        height: s.height,
                        hot_x: s.hot_x,
                        hot_y: s.hot_y,
                        data: s.bgra_bottom_up,
                    })
                }
                Some(CursorEvent::Shape(s)) => DisplayUpdate::LargePointer(LargePointer {
                    cache_index: 0,
                    width: s.width,
                    height: s.height,
                    hot_x: s.hot_x,
                    hot_y: s.hot_y,
                    data: s.bgra_bottom_up,
                }),
            };
            return Ok(Some(update));
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        let (w, h) = *self.shared.current_size.lock().unwrap();
        DesktopSize {
            width: w as u16,
            height: h as u16,
        }
    }

    async fn request_initial_size(&mut self, client: DesktopSize) -> DesktopSize {
        let (w, h) = clamp_size(u32::from(client.width), u32::from(client.height));
        info!(w, h, "Client initial size");
        *self.shared.wanted_size.lock().unwrap() = Some((w, h));
        *self.shared.current_size.lock().unwrap() = (w, h);
        DesktopSize {
            width: w as u16,
            height: h as u16,
        }
    }

    async fn updates(
        &mut self,
        _ctx: DisplayContext,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(PointerUpdates {
            cursor: self.cursor.clone(),
            first: true,
        }))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let Some(m) = layout
            .monitors()
            .iter()
            .find(|m| m.is_primary())
            .or(layout.monitors().first())
        else {
            return;
        };
        let (w, h) = m.dimensions();
        let (w, h) = clamp_size(w, h);
        info!(w, h, "Client asked for a new layout");
        *self.shared.wanted_size.lock().unwrap() = Some((w, h));
    }
}

// ---------------------------------------------------------------------------------------
// Pipeline

/// The H.264 encoder of one connection (both AVC444 views go through it).
struct Encoders {
    encoder: Encoder,
    avc444: bool,
    size: (u32, u32),
}

struct Pipeline {
    shared: Arc<Shared>,
    capture: Capture,
    frames: mpsc::Receiver<Frame>,
    encoders: Option<Encoders>,
    converter: color::Converter,
    /// Device buffers for frames that arrive in system memory.
    upload: crate::gpu::Pool,
    /// Send AVC444v2 (4:4:4) when the client supports it.
    avc444: bool,
    /// H-002: luma-only frames while things move, chroma once still.
    hybrid: bool,
    /// Frames sent and not yet acknowledged; beyond this the newest frame waits.
    max_in_flight: u32,
    ctx: Arc<CudaContext>,
    bitrate: u32,
    fps: u32,
    generation: u64,
    surface: Option<(u16, u32, u32)>,
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
    start: Instant,
    sent: u64,
    stats_at: Instant,
    stats_frames: u32,
    stats_encode_us: u128,
    stats_bytes: usize,
}

impl Pipeline {
    fn run(mut self) -> Result<()> {
        while !self.shared.stop.load(Ordering::Relaxed) {
            if let Some((w, h)) = self.shared.wanted_size.lock().unwrap().take() {
                let cur = *self.shared.current_size.lock().unwrap();
                if self.last_frame.as_ref().map(|f| (f.width, f.height)) != Some((w, h))
                    || cur != (w, h)
                {
                    self.capture.resize(w, h);
                    *self.shared.current_size.lock().unwrap() = (w, h);
                }
            }

            let generation = self.shared.gfx_generation.load(Ordering::SeqCst);
            let new_client = generation != self.generation;
            let wait = if self.pending { 2 } else { 50 };
            let frame = match self.frames.recv_timeout(Duration::from_millis(wait)) {
                Ok(f) => Some(f),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let mut mode = Mode::Full;
            if let Some(f) = frame {
                // H-002: frames closer than this are motion; they go luma-only and the
                // full chroma follows once the image is still.
                if self.hybrid && self.last_new_frame.elapsed() < MOTION_GAP {
                    mode = Mode::LumaOnly;
                }
                self.last_new_frame = Instant::now();
                self.last_frame = Some(f);
            } else if self.pending {
                // Retrying a frame held back by back pressure: same choice as when it came in.
                if self.hybrid && self.last_new_frame.elapsed() < MOTION_GAP {
                    mode = Mode::LumaOnly;
                }
            } else if self.chroma_owed && self.last_new_frame.elapsed() >= STILL_AFTER {
                mode = Mode::ChromaOnly;
            } else if !new_client {
                continue;
            }
            // Mutter only sends frames on damage: a client that connects to an idle
            // desktop gets the last frame again.
            let Some(frame) = self.last_frame.take() else {
                continue;
            };
            if new_client {
                self.generation = generation;
                self.surface = None;
                mode = Mode::Full;
            }
            if let Err(e) = self.send(&frame, mode) {
                warn!(%e, "Sending a frame failed");
            }
            self.last_frame = Some(frame);
        }
        Ok(())
    }

    fn send(&mut self, frame: &Frame, mode: Mode) -> Result<()> {
        let Some(gfx) = self.shared.gfx.lock().unwrap().clone() else {
            return Ok(());
        };
        let (w, h) = (frame.width, frame.height);

        // Surface for this size (ResetGraphics on resize).
        {
            let mut g = gfx.lock().unwrap();
            if !g.is_ready() {
                return Ok(());
            }
            if !g.supports_avc420() {
                warn!("Client does not support AVC420");
                return Ok(());
            }
            if !matches!(self.surface, Some((_, sw, sh)) if (sw, sh) == (w, h)) {
                // ResetGraphics with one primary monitor (FreeRDP rejects an empty list),
                // which also deletes the old surface.
                g.resize_with_monitors(
                    w as u16,
                    h as u16,
                    vec![Monitor {
                        left: 0,
                        top: 0,
                        right: w as i32 - 1,
                        bottom: h as i32 - 1,
                        flags: MonitorFlags::PRIMARY,
                    }],
                );
                self.surface = None;
            }
            if self.surface.is_none() {
                g.set_max_frames_in_flight(self.max_in_flight);
                let id = g
                    .create_surface(w as u16, h as u16)
                    .context("creating the EGFX surface")?;
                g.map_surface_to_output(id, 0, 0);
                self.surface = Some((id, w, h));
                self.need_full = true;
                if let Some(e) = self.encoders.as_mut() {
                    e.encoder.request_keyframe();
                }
                info!(w, h, surface = id, "EGFX surface ready");
            }
            if g.should_backpressure() {
                // Skip before encoding: a P-frame that is encoded but never sent would
                // break the reference chain on the client. The run loop retries this
                // frame every few milliseconds until an acknowledgement frees a slot.
                self.pending = true;
                return Ok(());
            }
        }

        let avc444 = self.avc444 && gfx.lock().unwrap().supports_avc444();
        // Pixels on the GPU; frames from system memory are uploaded first.
        let src = match &frame.data {
            FrameData::Gpu(b) => Arc::clone(b),
            FrameData::Cpu { data, stride } => {
                crate::gpu::upload(&mut self.upload, data, *stride as usize, w, h)?
            }
        };
        let (cw, ch) = color::coded_size(w, h);
        if self
            .encoders
            .as_ref()
            .is_none_or(|e| e.size != (cw, ch) || e.avc444 != avc444)
        {
            let cfg = EncoderConfig {
                codec: Codec::H264,
                width: cw,
                height: ch,
                // AVC444 sends two pictures per frame.
                fps: if avc444 { self.fps * 2 } else { self.fps },
                bitrate: self.bitrate,
                refs: if avc444 { 2 } else { 1 },
            };
            self.encoders = Some(Encoders {
                encoder: Encoder::new(Arc::clone(&self.ctx), cfg)?,
                avc444,
                size: (cw, ch),
            });
            info!(w, h, cw, ch, avc444, "Encoder ready");
        }
        // Which AVC444 views this frame carries (H-002): luma-only while things move,
        // chroma-only once still, both otherwise. The first frame on a surface has both.
        let mode = if !avc444 || self.need_full {
            Mode::Full
        } else {
            mode
        };
        let encoders = self.encoders.as_mut().expect("created above");
        let t = Instant::now();
        let (main, aux) = self
            .converter
            .convert(&src, avc444 && mode != Mode::LumaOnly)?;
        // Both views go through the same encoder as consecutive pictures: clients decode
        // AVC444 with a single H.264 decoder (FreeRDP does; so does its server).
        let out_main = if mode == Mode::ChromaOnly {
            Default::default()
        } else {
            encoders.encoder.encode_nv12(main)?
        };
        let out_aux = match aux {
            Some(aux) => encoders.encoder.encode_nv12(aux)?,
            None => Default::default(),
        };
        let encode_us = t.elapsed().as_micros();

        let (surface_id, _, _) = self.surface.expect("surface created above");
        let region = Avc420Region {
            left: 0,
            top: 0,
            right: w as u16,
            bottom: h as u16,
            quantization_parameter: 22,
            quality: 100,
        };
        let regions = [region];
        let ts = self.start.elapsed().as_millis() as u32;
        let bytes = out_main.data.len() + out_aux.data.len();
        let messages = {
            let mut g = gfx.lock().unwrap();
            let queued = if avc444 {
                match mode {
                    Mode::Full => g.send_avc444v2_frame(
                        surface_id,
                        Encoding::LUMA_AND_CHROMA,
                        &out_main.data,
                        &regions,
                        Some(&out_aux.data),
                        Some(&regions),
                        ts,
                    ),
                    Mode::LumaOnly => g.send_avc444v2_frame(
                        surface_id,
                        Encoding::LUMA,
                        &out_main.data,
                        &regions,
                        None,
                        None,
                        ts,
                    ),
                    Mode::ChromaOnly => g.send_avc444v2_frame(
                        surface_id,
                        Encoding::CHROMA,
                        &out_aux.data,
                        &regions,
                        None,
                        None,
                        ts,
                    ),
                }
            } else {
                g.send_avc420_frame(surface_id, &out_main.data, &regions, ts)
            };
            if queued.is_none() {
                return Ok(());
            }
            self.need_full = false;
            self.chroma_owed = avc444 && mode == Mode::LumaOnly;
            let channel_id = g.channel_id().context("EGFX channel has no id")?;
            ironrdp_dvc::encode_dvc_messages(
                channel_id,
                g.drain_output(),
                ironrdp_svc::ChannelFlags::empty(),
            )
            .map_err(|e| anyhow::anyhow!("encoding EGFX messages: {e}"))?
        };
        if let Some(tx) = self.shared.events.lock().unwrap().as_ref() {
            let _ = tx.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                messages,
            }));
        }

        self.sent += 1;
        self.pending = false;
        self.stats_frames += 1;
        self.stats_encode_us += encode_us;
        self.stats_bytes += bytes;
        if self.stats_at.elapsed() >= Duration::from_secs(5) {
            let secs = self.stats_at.elapsed().as_secs_f64();
            let acked = self.shared.acked.swap(0, Ordering::Relaxed);
            info!(
                fps = format!("{:.1}", f64::from(self.stats_frames) / secs),
                client_fps = format!("{:.1}", acked as f64 / secs),
                mbps = format!("{:.1}", self.stats_bytes as f64 * 8.0 / secs / 1e6),
                encode_ms = format!(
                    "{:.2}",
                    self.stats_encode_us as f64 / f64::from(self.stats_frames.max(1)) / 1000.0
                ),
                capture_to_queue_ms =
                    format!("{:.2}", frame.captured.elapsed().as_secs_f64() * 1000.0),
                "Stream"
            );
            self.stats_at = Instant::now();
            self.stats_frames = 0;
            self.stats_encode_us = 0;
            self.stats_bytes = 0;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// TLS

fn tls_identity(dir: &Path) -> Result<TlsIdentityCtx> {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    if !cert.exists() || !key.exists() {
        std::fs::create_dir_all(dir)?;
        let host = std::fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "glidedesk".into());
        let ck = rcgen::generate_simple_self_signed(vec![host.trim().to_owned()])?;
        std::fs::write(&cert, ck.cert.pem())?;
        std::fs::write(&key, ck.signing_key.serialize_pem())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        }
        info!(path = %cert.display(), "Created a self-signed certificate");
    }
    TlsIdentityCtx::init_from_paths(&cert, &key)
        .map_err(|e| anyhow::anyhow!("loading the TLS identity: {e}"))
}

// ---------------------------------------------------------------------------------------

pub async fn serve(opts: ServeOptions) -> Result<()> {
    let conn = zbus::Connection::session().await?;
    let session = mutter::Session::start(&conn, mutter::CursorMode::Metadata).await?;
    info!(node = session.node_id, "Virtual monitor ready");
    {
        let conn = conn.clone();
        tokio::spawn(async move { mutter::leave_startup_overview(&conn).await });
    }

    let shared = Arc::new(Shared::default());
    *shared.current_size.lock().unwrap() = clamp_size(opts.width, opts.height);
    let (w, h) = *shared.current_size.lock().unwrap();
    let ctx = CudaContext::new(opts.gpu).context("creating the CUDA context")?;
    let (tx, rx) = mpsc::sync_channel(1);
    let (cursor_tx, cursor_rx) = tokio::sync::watch::channel(None);
    let capture = Capture::start(
        session.node_id,
        w,
        h,
        Sinks {
            frames: tx,
            cursor: cursor_tx,
        },
        Some(Arc::clone(&ctx)),
    )?;

    let pipeline = Pipeline {
        shared: Arc::clone(&shared),
        capture,
        frames: rx,
        encoders: None,
        converter: color::Converter::new(Arc::clone(&ctx))?,
        upload: crate::gpu::Pool::new(Arc::clone(&ctx)),
        avc444: opts.avc444,
        hybrid: opts.hybrid,
        max_in_flight: opts.max_in_flight,
        ctx: Arc::clone(&ctx),
        bitrate: opts.bitrate,
        fps: opts.fps,
        generation: 0,
        surface: None,
        last_frame: None,
        pending: false,
        last_new_frame: Instant::now(),
        chroma_owed: false,
        need_full: true,
        start: Instant::now(),
        sent: 0,
        stats_at: Instant::now(),
        stats_frames: 0,
        stats_encode_us: 0,
        stats_bytes: 0,
    };
    let pipeline_thread = std::thread::Builder::new()
        .name("pipeline".into())
        .spawn(move || pipeline.run())?;

    let identity = tls_identity(&opts.state_dir)?;
    let acceptor = identity
        .make_acceptor()
        .map_err(|e| anyhow::anyhow!("building the TLS acceptor: {e}"))?;
    let input = InputForwarder::spawn(session.remote.clone(), session.stream_path.clone());

    let mut server = RdpServer::builder()
        .with_addr(opts.bind)
        .with_hybrid(acceptor, identity.pub_key)
        .with_input_handler(input)
        .with_display_handler(Display {
            shared: Arc::clone(&shared),
            cursor: cursor_rx,
        })
        .with_gfx_factory(Some(Box::new(GfxFactory(Arc::clone(&shared)))))
        .with_connection_handler(Some(Box::new(Connections)))
        // RDP-UDP on the same port, as Windows does; TCP stays as the fallback.
        .with_udp_transport(opts.bind)
        .build();
    server.set_credentials(Some(Credentials {
        username: opts.username,
        password: opts.password,
        domain: None,
    }));
    info!(bind = %opts.bind, "Listening for RDP clients");
    let r = server.run().await;
    shared.stop.store(true, Ordering::Relaxed);
    let _ = pipeline_thread.join();
    session.remote.stop().await.ok();
    r.map_err(|e| anyhow::anyhow!("RDP server: {e}"))
}

/// Logs connections: who, with which keyboard layout, for how long.
struct Connections;

impl ConnectionHandler for Connections {
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        info!(%peer, "Client connecting");
        true
    }

    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        info!(
            keyboard_layout = format!("0x{:08x}", info.keyboard_layout),
            monitors = info.client_monitors.len(),
            reconnect = info.auto_reconnect,
            "Client session"
        );
    }

    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&ironrdp_server::ServerError>,
    ) -> PostConnectionAction {
        info!(%peer, seconds = duration.as_secs(), error = ?error.map(|e| e.to_string()), "Client disconnected");
        PostConnectionAction::Continue
    }
}

/// Frames that arrive closer together than this count as motion (H-002).
const MOTION_GAP: Duration = Duration::from_millis(50);
/// After this long without a new frame, the image counts as still and its full chroma is
/// sent.
const STILL_AFTER: Duration = Duration::from_millis(60);

/// What an AVC444v2 frame carries (MS-RDPEGFX 2.2.4.5, the LC field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Both views (LC=0).
    Full,
    /// Main view only (LC=1): luma and 4:2:0 chroma, half the encoding work.
    LumaOnly,
    /// Auxiliary view only (LC=2): the rest of the chroma, for a still image.
    ChromaOnly,
}

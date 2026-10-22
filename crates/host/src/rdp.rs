//! RDP front end: IronRDP server, EGFX (H.264 AVC420 / AVC444v2) fed by the pipeline,
//! Display Control for resizing, the pointer drawn by the client, rdpsnd and clipboard.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

use crate::capture::CursorEvent;
use crate::encoder::{Codec, EncodedFrame};
use crate::input::InputForwarder;
use crate::pipeline::{self, Mode, Output, Prepared};

/// State shared by the IronRDP callbacks and the RDP output of the pipeline.
struct Shared {
    session: Arc<pipeline::Shared>,
    /// Index of the RDP output in the pipeline.
    index: usize,
    /// EGFX server of the current connection, replaced on every new connection.
    gfx: Mutex<Option<GfxServerHandle>>,
    /// Frames the client acknowledged as decoded (MS-RDPEGFX frame acknowledge).
    acked: AtomicU64,
    events: Mutex<Option<UnboundedSender<ServerEvent>>>,
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
        self.0.session.take_over(self.0.index);
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
        let (w, h) = *self.shared.session.current_size.lock().unwrap();
        DesktopSize {
            width: w as u16,
            height: h as u16,
        }
    }

    async fn request_initial_size(&mut self, client: DesktopSize) -> DesktopSize {
        let s = &self.shared.session;
        let (w, h) = s.request_size(u32::from(client.width), u32::from(client.height));
        info!(w, h, "Client initial size");
        *s.current_size.lock().unwrap() = (w, h);
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
        let (w, h) = self.shared.session.request_size(w, h);
        info!(w, h, "Client asked for a new layout");
    }
}

// ---------------------------------------------------------------------------------------
// Pipeline output

/// The pipeline's side of EGFX: one surface the size of the desktop, H.264 frames on it.
pub struct RdpOutput {
    shared: Arc<Shared>,
    surface: Option<(u16, u32, u32)>,
    start: Instant,
}

impl RdpOutput {
    fn gfx(&self) -> Option<GfxServerHandle> {
        self.shared.gfx.lock().unwrap().clone()
    }
}

impl Output for RdpOutput {
    fn prepare(&mut self, w: u32, h: u32, max_in_flight: u32) -> Result<Prepared> {
        let Some(gfx) = self.gfx() else {
            return Ok(Prepared::Absent);
        };
        let mut g = gfx.lock().unwrap();
        if !g.is_ready() {
            return Ok(Prepared::Absent);
        }
        if !g.supports_avc420() {
            warn!("Client does not support AVC420");
            return Ok(Prepared::Absent);
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
        let mut new_surface = false;
        if self.surface.is_none() {
            g.set_max_frames_in_flight(max_in_flight);
            let id = g
                .create_surface(w as u16, h as u16)
                .context("creating the EGFX surface")?;
            g.map_surface_to_output(id, 0, 0);
            self.surface = Some((id, w, h));
            new_surface = true;
            info!(w, h, surface = id, "EGFX surface ready");
        }
        if g.should_backpressure() {
            return Ok(Prepared::Busy);
        }
        Ok(Prepared::Ready {
            new_surface,
            avc444: g.supports_avc444(),
            codec: Codec::H264,
        })
    }

    fn send(&mut self, mode: Mode, main: &EncodedFrame, aux: &EncodedFrame) -> Result<bool> {
        let (Some(gfx), Some((surface_id, w, h))) = (self.gfx(), self.surface) else {
            return Ok(false);
        };
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
        // AVC444 frames carry an auxiliary view, or are one of the single-view kinds.
        let avc444 = !aux.data.is_empty() || mode != Mode::Full;
        let messages = {
            let mut g = gfx.lock().unwrap();
            let queued = match (avc444, mode) {
                (false, _) => g.send_avc420_frame(surface_id, &main.data, &regions, ts),
                (true, Mode::Full) => g.send_avc444v2_frame(
                    surface_id,
                    Encoding::LUMA_AND_CHROMA,
                    &main.data,
                    &regions,
                    Some(&aux.data),
                    Some(&regions),
                    ts,
                ),
                (true, Mode::LumaOnly) => g.send_avc444v2_frame(
                    surface_id,
                    Encoding::LUMA,
                    &main.data,
                    &regions,
                    None,
                    None,
                    ts,
                ),
                (true, Mode::ChromaOnly) => g.send_avc444v2_frame(
                    surface_id,
                    Encoding::CHROMA,
                    &aux.data,
                    &regions,
                    None,
                    None,
                    ts,
                ),
            };
            if queued.is_none() {
                return Ok(false);
            }
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
        Ok(true)
    }

    fn take_acked(&mut self) -> u64 {
        self.shared.acked.swap(0, Ordering::Relaxed)
    }

    fn reset(&mut self) {
        self.surface = None;
    }
}

// ---------------------------------------------------------------------------------------
// TLS

/// Creates the self-signed TLS identity (both front ends use it) unless it exists.
pub fn ensure_identity(dir: &Path) -> Result<()> {
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
    Ok(())
}

fn tls_identity(dir: &Path) -> Result<TlsIdentityCtx> {
    ensure_identity(dir)?;
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    TlsIdentityCtx::init_from_paths(&cert, &key)
        .map_err(|e| anyhow::anyhow!("loading the TLS identity: {e}"))
}

// ---------------------------------------------------------------------------------------

/// What the RDP front end needs from the session.
pub struct Front {
    pub bind: SocketAddr,
    pub username: String,
    pub password: String,
    pub state_dir: std::path::PathBuf,
    pub cursor: tokio::sync::watch::Receiver<Option<CursorEvent>>,
    pub audio: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
    pub input: InputForwarder,
    pub remote: crate::mutter::RemoteDesktopSessionProxy<'static>,
}

/// The RDP front end as output `index` of the pipeline: the output, and the server to run.
pub fn new(session: Arc<pipeline::Shared>, index: usize) -> (RdpOutput, Server) {
    let shared = Arc::new(Shared {
        session,
        index,
        gfx: Mutex::new(None),
        acked: AtomicU64::new(0),
        events: Mutex::new(None),
    });
    (
        RdpOutput {
            shared: Arc::clone(&shared),
            surface: None,
            start: Instant::now(),
        },
        Server { shared },
    )
}

pub struct Server {
    shared: Arc<Shared>,
}

impl Server {
    pub async fn run(self, front: Front) -> Result<()> {
        let identity = tls_identity(&front.state_dir)?;
        let acceptor = identity
            .make_acceptor()
            .map_err(|e| anyhow::anyhow!("building the TLS acceptor: {e}"))?;
        let mut server = RdpServer::builder()
            .with_addr(front.bind)
            .with_hybrid(acceptor, identity.pub_key)
            .with_input_handler(front.input)
            .with_display_handler(Display {
                shared: Arc::clone(&self.shared),
                cursor: front.cursor,
            })
            .with_gfx_factory(Some(Box::new(GfxFactory(Arc::clone(&self.shared)))))
            .with_sound_factory(front.audio.map(|a| {
                Box::new(crate::audio::SoundFactory::new(a))
                    as Box<dyn ironrdp_server::SoundServerFactory>
            }))
            .with_cliprdr_factory(Some(Box::new(crate::clipboard::ClipboardFactory::spawn(
                front.remote,
            ))))
            .with_connection_handler(Some(Box::new(Connections)))
            // RDP-UDP on the same port, as Windows does; TCP stays as the fallback.
            .with_udp_transport(front.bind)
            .build();
        server.set_credentials(Some(Credentials {
            username: front.username,
            password: front.password,
            domain: None,
        }));
        info!(bind = %front.bind, "Listening for RDP clients");
        server
            .run()
            .await
            .map_err(|e| anyhow::anyhow!("RDP server: {e}"))
    }
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

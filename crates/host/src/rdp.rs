//! RDP front end: IronRDP server, EGFX (H.264 AVC420 / AVC444v2) fed by the pipeline,
//! Display Control for resizing, the pointer drawn by the client, rdpsnd and clipboard.
//!
//! Several monitors (`mstsc /multimon`, or a Display Control layout of several): the client's
//! monitors from its GCC Client Monitor Data, or from Display Control, become the session's
//! layout ([`crate::layout`]); `ResetGraphics` lists them all and each monitor's pipeline has
//! a surface of its own, mapped at the monitor's position. What the pinned IronRDP does not
//! do: its acceptor's Monitor Layout PDU always describes one monitor of the desktop size
//! known before the client's GCC data, and its Display Control capabilities cap each
//! monitor at 3840×2400 pixels.

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

use crate::encoder::{Codec, EncodedFrame};
use crate::frame::CursorEvent;
use crate::input::InputForwarder;
use crate::pipeline::{self, Mode, Output, Prepared};
use crate::rate::DelayMeter;

/// The graphics output as last reset (`ResetGraphics` deletes every surface).
#[derive(Default)]
struct Graphics {
    /// The reset was sent on the current EGFX channel for the current client.
    valid: bool,
    size: (u32, u32),
    monitors: Vec<Monitor>,
    /// Bumped on every reset: the surfaces of an older one are gone.
    generation: u64,
}

/// State shared by the IronRDP callbacks and the RDP output of the pipeline.
struct Shared {
    session: Arc<pipeline::Shared>,
    /// Index of the RDP output in the pipeline.
    index: usize,
    /// EGFX server of the current connection, replaced on every new connection.
    gfx: Mutex<Option<GfxServerHandle>>,
    /// Frames the client acknowledged as decoded (MS-RDPEGFX frame acknowledge).
    acked: AtomicU64,
    /// Send-to-acknowledge delays, for the rate control.
    meter: Mutex<DelayMeter>,
    events: Mutex<Option<UnboundedSender<ServerEvent>>>,
    graphics: Mutex<Graphics>,
    /// The client's monitors (GCC, then Display Control), monitor 0 its primary.
    client_monitors: Arc<Mutex<Vec<glidedesk_proto::Monitor>>>,
}

/// The client's monitors as a layout to ask for: `(left, top, width, height, primary)` each,
/// the primary as monitor 0 and the others numbered from 1.
fn client_layout(
    monitors: impl Iterator<Item = (i32, i32, u32, u32, bool)>,
) -> Vec<glidedesk_proto::Monitor> {
    let mut next = 1u8;
    let mut primary_seen = false;
    let mut out = Vec::new();
    for (x, y, w, h, primary) in monitors {
        let id = if primary && !primary_seen {
            primary_seen = true;
            0
        } else {
            let id = next;
            next = next.saturating_add(1);
            id
        };
        out.push(glidedesk_proto::Monitor {
            id,
            x,
            y,
            width: w.min(u32::from(u16::MAX)) as u16,
            height: h.min(u32::from(u16::MAX)) as u16,
            refresh: 60,
            primary,
        });
    }
    out
}

/// The monitors of a GCC Client Monitor Data block (inclusive bounds).
fn from_gcc(monitors: &[Monitor]) -> Vec<glidedesk_proto::Monitor> {
    client_layout(monitors.iter().map(|m| {
        (
            m.left,
            m.top,
            (m.right - m.left + 1).max(0) as u32,
            (m.bottom - m.top + 1).max(0) as u32,
            m.flags.contains(MonitorFlags::PRIMARY),
        )
    }))
}

/// A monitor of the layout for `ResetGraphics`.
fn gcc_monitor(m: &glidedesk_proto::Monitor) -> Monitor {
    Monitor {
        left: m.x,
        top: m.y,
        right: m.x + i32::from(m.width) - 1,
        bottom: m.y + i32::from(m.height) - 1,
        flags: if m.id == 0 {
            MonitorFlags::PRIMARY
        } else {
            MonitorFlags::empty()
        },
    }
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
        self.0.meter.lock().unwrap().clear();
        // A new channel: its graphics output starts with a reset.
        self.0.graphics.lock().unwrap().valid = false;
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

    fn on_frame_ack(&mut self, frame_id: u32, _queue_depth: u32, _total_frames_decoded: u32) {
        self.0.acked.fetch_add(1, Ordering::Relaxed);
        self.0.meter.lock().unwrap().acked(frame_id);
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
    /// Most monitors the session shows.
    monitors: usize,
}

impl Display {
    /// Asks for the client's monitors when there are several and the session can show
    /// them; returns the desktop size, or `None` for a single monitor.
    fn several(&self, asked: Vec<glidedesk_proto::Monitor>) -> Option<DesktopSize> {
        let s = &self.shared.session;
        if self.monitors < 2 || asked.len() < 2 {
            s.request_layout(Vec::new());
            return None;
        }
        let layout = crate::layout::normalize(&asked, self.monitors);
        let (w, h) = s.request_size(u32::from(layout[0].width), u32::from(layout[0].height));
        *s.current_size.lock().unwrap() = (w, h);
        let (dw, dh) = crate::layout::bounds(&layout);
        info!(monitors = layout.len(), w, h, dw, dh, "Client monitors");
        s.request_layout(layout);
        Some(DesktopSize {
            width: dw.min(u32::from(u16::MAX)) as u16,
            height: dh.min(u32::from(u16::MAX)) as u16,
        })
    }
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
        let layout = self.shared.session.layout.borrow().clone();
        let (w, h) = if layout.len() > 1 {
            crate::layout::bounds(&layout)
        } else {
            *self.shared.session.current_size.lock().unwrap()
        };
        DesktopSize {
            width: w as u16,
            height: h as u16,
        }
    }

    async fn request_initial_size(&mut self, client: DesktopSize) -> DesktopSize {
        // The client's GCC monitors (mstsc /multimon), or its Display Control layout on a
        // reactivation.
        let asked = self.shared.client_monitors.lock().unwrap().clone();
        if let Some(size) = self.several(asked) {
            return size;
        }
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

    async fn monitor_count(&mut self) -> u32 {
        self.monitors as u32
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let asked = client_layout(layout.monitors().iter().map(|m| {
            let (x, y) = m.position().unwrap_or((0, 0));
            let (w, h) = m.dimensions();
            (x, y, w, h, m.is_primary())
        }));
        *self.shared.client_monitors.lock().unwrap() = asked.clone();
        if self.several(asked).is_some() {
            return;
        }
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

/// The pipeline's side of EGFX: a surface the size of the monitor (with one monitor, the
/// desktop), H.264 frames on it.
pub struct RdpOutput {
    shared: Arc<Shared>,
    /// The monitor of the layout it shows.
    monitor: u8,
    /// Surface id, size, and the graphics reset it was created after.
    surface: Option<(u16, u32, u32, u64)>,
    start: Instant,
}

impl RdpOutput {
    fn new(shared: Arc<Shared>, monitor: u8) -> Self {
        Self {
            shared,
            monitor,
            surface: None,
            start: Instant::now(),
        }
    }

    fn gfx(&self) -> Option<GfxServerHandle> {
        self.shared.gfx.lock().unwrap().clone()
    }
}

impl Output for RdpOutput {
    fn prepare(&mut self, w: u32, h: u32, max_in_flight: u32) -> Result<Prepared> {
        // The graphics output: the layout's monitors, or one of this frame's size (FreeRDP
        // rejects an empty list).
        let layout = self.shared.session.layout.borrow().clone();
        let (size, monitors, origin) = if layout.len() > 1 {
            let Some(me) = layout.iter().find(|m| m.id == self.monitor) else {
                return Ok(Prepared::Absent);
            };
            (
                crate::layout::bounds(&layout),
                layout.iter().map(gcc_monitor).collect::<Vec<_>>(),
                (me.x.max(0) as u32, me.y.max(0) as u32),
            )
        } else if self.monitor == 0 {
            let primary = Monitor {
                left: 0,
                top: 0,
                right: w as i32 - 1,
                bottom: h as i32 - 1,
                flags: MonitorFlags::PRIMARY,
            };
            ((w, h), vec![primary], (0, 0))
        } else {
            return Ok(Prepared::Absent);
        };
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
        let generation = {
            let mut graphics = self.shared.graphics.lock().unwrap();
            if !graphics.valid || graphics.size != size || graphics.monitors != monitors {
                // ResetGraphics, which also deletes every surface.
                g.resize_with_monitors(size.0 as u16, size.1 as u16, monitors.clone());
                *graphics = Graphics {
                    valid: true,
                    size,
                    monitors,
                    generation: graphics.generation.wrapping_add(1),
                };
            }
            graphics.generation
        };
        if let Some((id, sw, sh, made)) = self.surface
            && (made != generation || (sw, sh) != (w, h))
        {
            // This monitor alone changed size: its old surface goes.
            if made == generation {
                g.delete_surface(id);
            }
            self.surface = None;
        }
        let mut new_surface = false;
        if self.surface.is_none() {
            g.set_max_frames_in_flight(max_in_flight);
            let id = g
                .create_surface(w as u16, h as u16)
                .context("creating the EGFX surface")?;
            g.map_surface_to_output(id, origin.0, origin.1);
            self.surface = Some((id, w, h, generation));
            new_surface = true;
            info!(
                monitor = self.monitor,
                w,
                h,
                x = origin.0,
                y = origin.1,
                surface = id,
                "EGFX surface ready"
            );
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
        let (Some(gfx), Some((surface_id, w, h, _))) = (self.gfx(), self.surface) else {
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
            let Some(frame_id) = queued else {
                return Ok(false);
            };
            self.shared
                .meter
                .lock()
                .unwrap()
                .sent(frame_id, main.data.len() + aux.data.len());
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

    // The connection's frame acknowledgements cover every monitor's surface: monitor 0's
    // output takes them (for the statistics and the rate control).
    fn take_acked(&mut self) -> u64 {
        if self.monitor == 0 {
            self.shared.acked.swap(0, Ordering::Relaxed)
        } else {
            0
        }
    }

    fn take_delays(&mut self) -> Vec<(std::time::Duration, usize)> {
        if self.monitor == 0 {
            self.shared.meter.lock().unwrap().take()
        } else {
            Vec::new()
        }
    }

    fn reset(&mut self) {
        self.surface = None;
        self.shared.graphics.lock().unwrap().valid = false;
    }
}

/// The RDP front end's outputs for the monitors after the first.
struct RdpMonitors(Arc<Shared>);

impl pipeline::MonitorOutputs for RdpMonitors {
    fn output(&self, monitor: u8) -> Box<dyn Output> {
        Box::new(RdpOutput::new(Arc::clone(&self.0), monitor))
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

/// Puts the given certificate chain and key in place of the self-signed identity (RDP, the
/// native protocol and the web page all read `cert.pem` and `key.pem` from `dir`). Both are
/// checked to parse first, so a wrong file does not replace a working identity.
pub fn install_identity(dir: &Path, cert: &Path, key: &Path) -> Result<()> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let chain = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("reading {}", cert.display()))?;
    anyhow::ensure!(!chain.is_empty(), "{} has no certificate", cert.display());
    PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading {}", key.display()))?;
    std::fs::create_dir_all(dir)?;
    std::fs::copy(cert, dir.join("cert.pem")).context("copying the certificate")?;
    let key_dst = dir.join("key.pem");
    std::fs::copy(key, &key_dst).context("copying the key")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_dst, std::fs::Permissions::from_mode(0o600))?;
    }
    info!(cert = %cert.display(), certificates = chain.len(), "Using the given TLS certificate");
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
    /// Clipboard bridge, when the session has one.
    pub clipboard: Option<Box<dyn ironrdp_server::CliprdrServerFactory>>,
    /// Failed logins per address, shared with the other front ends.
    pub auth_limiter: Arc<crate::auth::RateLimiter>,
    /// Most monitors the session shows (offered to Display Control, taken from GCC).
    pub monitors: usize,
}

/// The RDP front end as output `index` of the pipeline: the output, and the server to run.
pub fn new(session: Arc<pipeline::Shared>, index: usize) -> (RdpOutput, Server) {
    let shared = Arc::new(Shared {
        session,
        index,
        gfx: Mutex::new(None),
        acked: AtomicU64::new(0),
        meter: Mutex::default(),
        events: Mutex::new(None),
        graphics: Mutex::default(),
        client_monitors: Arc::default(),
    });
    (RdpOutput::new(Arc::clone(&shared), 0), Server { shared })
}

pub struct Server {
    shared: Arc<Shared>,
}

impl Server {
    /// The outputs for the pipelines of further monitors.
    pub fn monitors(&self) -> Arc<dyn pipeline::MonitorOutputs> {
        Arc::new(RdpMonitors(Arc::clone(&self.shared)))
    }

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
                monitors: front.monitors.max(1),
            })
            .with_gfx_factory(Some(Box::new(GfxFactory(Arc::clone(&self.shared)))))
            .with_sound_factory(front.audio.map(|a| {
                Box::new(crate::sound::SoundFactory::new(a))
                    as Box<dyn ironrdp_server::SoundServerFactory>
            }))
            .with_cliprdr_factory(front.clipboard)
            // No drive redirection (`with_rdpdr_factory`) at this IronRDP revision: its
            // RdpdrServer never sends PAKID_CORE_USER_LOGGEDON, which FreeRDP (and, per
            // MS-RDPEFS, mstsc) waits for before announcing drives, and keeps the device
            // type private. IronRDP has both from 2e65d08 (#2440) on.
            .with_connection_handler(Some(Box::new(Connections {
                monitors: Some(Arc::clone(&self.shared.client_monitors)),
                ..Connections::new(front.auth_limiter)
            })))
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

/// Logs connections (who, with which keyboard layout, for how long) and limits failed
/// logins per address, like the native and web front ends.
///
/// NLA runs inside IronRDP, which does not say why a connection failed. A connection that
/// ends in an error before the session starts ([`ConnectionHandler::on_connection_info`]
/// is called only after the credentials were accepted) counts as a failed login: a wrong
/// password, or a scanner that gives up during the handshake. The first two are free, as
/// elsewhere, so a client that probes once (mstsc does, before showing a certificate
/// warning) is not held back.
struct Connections {
    limiter: Arc<crate::auth::RateLimiter>,
    /// The connection being served (IronRDP serves one at a time): its address, and
    /// whether it got past the login.
    current: Option<(SocketAddr, bool)>,
    /// Where the client's monitors go, for the display handler.
    monitors: Option<Arc<Mutex<Vec<glidedesk_proto::Monitor>>>>,
}

impl Connections {
    fn new(limiter: Arc<crate::auth::RateLimiter>) -> Self {
        Self {
            limiter,
            current: None,
            monitors: None,
        }
    }
}

impl ConnectionHandler for Connections {
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        if let Some(wait) = self.limiter.check(peer.ip()) {
            warn!(%peer, wait_secs = wait.as_secs_f64().ceil() as u64, "Too many failed logins: connection refused");
            return false;
        }
        info!(%peer, "Client connecting");
        self.current = Some((peer, false));
        true
    }

    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        if let Some((peer, logged_in)) = self.current.as_mut() {
            *logged_in = true;
            self.limiter.record_success(peer.ip());
        }
        info!(
            keyboard_layout = format!("0x{:08x}", info.keyboard_layout),
            monitors = info.client_monitors.len(),
            reconnect = info.auto_reconnect,
            "Client session"
        );
        // Called before the display handler's initial size, which uses them.
        if let Some(slot) = &self.monitors {
            *slot.lock().unwrap() = from_gcc(&info.client_monitors);
        }
    }

    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&ironrdp_server::ServerError>,
    ) -> PostConnectionAction {
        let logged_in = self
            .current
            .take()
            .is_some_and(|(p, logged_in)| p == peer && logged_in);
        if !logged_in && error.is_some() {
            match self.limiter.record_failure(peer.ip()) {
                Some(wait) => {
                    warn!(%peer, lockout_secs = wait.as_secs(), "Login failed; backoff applied");
                }
                None => warn!(%peer, "Login failed (or the client gave up)"),
            }
        }
        info!(%peer, seconds = duration.as_secs(), error = ?error.map(|e| e.to_string()), "Client disconnected");
        PostConnectionAction::Continue
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    use ironrdp_server::ServerErrorExt as _;

    #[test]
    fn failed_logins_lock_the_address_out() {
        let limiter = Arc::new(crate::auth::RateLimiter::new());
        let mut c = Connections::new(Arc::clone(&limiter));
        let peer: SocketAddr = "192.0.2.5:50000".parse().unwrap();
        let err = || ironrdp_server::ServerError::reason("test", "handshake failed");
        for _ in 0..3 {
            assert!(c.on_accept(peer));
            let e = err();
            c.on_disconnected(peer, Duration::from_secs(1), Some(&e));
        }
        // The third failure imposes a wait: the next connection is refused at once.
        assert!(!c.on_accept(peer));
        assert!(limiter.check(peer.ip()).is_some());
    }

    #[test]
    fn a_session_that_started_is_not_a_failure() {
        let limiter = Arc::new(crate::auth::RateLimiter::new());
        let mut c = Connections::new(Arc::clone(&limiter));
        let peer: SocketAddr = "192.0.2.6:50000".parse().unwrap();
        for _ in 0..5 {
            assert!(c.on_accept(peer));
            c.on_connection_info(&ConnectionInfo::new(
                0x409,
                ironrdp_pdu::gcc::KeyboardType::IBM_ENHANCED,
                String::new(),
            ));
            let e = err_any();
            c.on_disconnected(peer, Duration::from_secs(600), Some(&e));
        }
        assert!(c.on_accept(peer));
    }

    fn err_any() -> ironrdp_server::ServerError {
        ironrdp_server::ServerError::reason("test", "connection reset")
    }

    #[test]
    fn client_monitors_put_the_primary_first() {
        // mstsc /multimon: a screen left of the primary, which is at 0,0.
        let gcc = [
            Monitor {
                left: -1280,
                top: 0,
                right: -1,
                bottom: 1023,
                flags: MonitorFlags::empty(),
            },
            Monitor {
                left: 0,
                top: 0,
                right: 1919,
                bottom: 1079,
                flags: MonitorFlags::PRIMARY,
            },
        ];
        let got = from_gcc(&gcc);
        assert_eq!(
            got.iter()
                .map(|m| (m.id, m.x, m.width, m.height, m.primary))
                .collect::<Vec<_>>(),
            [(1, -1280, 1280, 1024, false), (0, 0, 1920, 1080, true)]
        );
        let layout = crate::layout::normalize(&got, 4);
        assert_eq!((layout[0].id, layout[0].x), (0, 1280));
        assert_eq!(
            gcc_monitor(&layout[0]),
            Monitor {
                left: 1280,
                top: 0,
                right: 3199,
                bottom: 1079,
                flags: MonitorFlags::PRIMARY,
            }
        );
        assert_eq!(crate::layout::bounds(&layout), (3200, 1080));
    }
}

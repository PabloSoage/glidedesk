//! Native front end: the low-latency protocol of `glidedesk-proto` (ADR-0004).
//!
//! TCP + TLS carries the control messages (login, input, resize, cursor, acknowledgements);
//! UDP on the same port carries the video, in Reed-Solomon protected shards, and the audio,
//! every datagram sealed with the session key. The pipeline thread sends video straight
//! from [`NativeOutput`]; a tokio task per connection runs the control channel.
//!
//! Under the system daemon ([`Listen::Handover`]) clients arrive already logged in over a
//! Unix socket instead, and the UDP socket is on the loopback, where the daemon relays the
//! (still sealed) datagrams.
//!
//! With several monitors (protocol version 6) each monitor's pipeline has a
//! [`NativeOutput`] of its own ([`NativeMonitors`]); their video shares the link, the
//! session key and the packet numbers, and has its own frames and acknowledgements.

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use glidedesk_proto::crypto::{Direction, PacketKey};
use glidedesk_proto::{
    ClientMsg, Codec as WireCodec, Cursor, InputEvent, ServerMsg, VideoMsg, fec, files, link,
};
use ironrdp_server::{KeyboardEvent, MouseButton, MouseEvent, RdpServerInputHandler};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::encoder::{Codec, EncodedFrame};
use crate::frame::CursorEvent;
use crate::input::InputForwarder;
use crate::pipeline::{self, Mode, Output, Prepared};
use crate::rate::DelayMeter;

// Parity per video block and the pacing of large frames follow the link: see
// `glidedesk_proto::link` (clients from version 4 report their losses; older ones get the
// starting parity and pacing to the bitrate ceiling).

/// The current client, as the pipeline and the UDP task see it.
struct Link {
    key: PacketKey,
    token: [u8; 16],
    /// Where its UDP hello came from; no video before it.
    peer: Option<SocketAddr>,
    codec: Codec,
    control: mpsc::UnboundedSender<ServerMsg>,
    seq: u64,
    /// The client speaks protocol version 2: lost frames are invalidated, not answered
    /// with a keyframe.
    invalidation: bool,
    /// The client asked for several monitors (version 6): the video of the monitors after
    /// the first goes to it.
    layout: bool,
    /// What it was sent of each monitor's video, by monitor.
    video: HashMap<u8, Sent>,
    /// The client asked for HEVC 4:4:4 (version 5) and the session is HEVC.
    yuv444: bool,
}

/// What the current client was sent of one monitor's video.
#[derive(Default)]
struct Sent {
    /// Size the client was last told about.
    size: Option<(u32, u32)>,
    frame_id: u32,
    /// Recent frames and their encoder picture index, oldest first.
    recent: VecDeque<(u32, u64)>,
}

/// Frames remembered for invalidation requests (a loss further back needs a keyframe).
const SENT_KEPT: usize = 32;

/// The video of one monitor, as its pipeline output and the control task share it.
struct Video {
    /// Newest frame the client acknowledged.
    acked_id: AtomicU32,
    acked: AtomicU64,
    keyframe_requested: AtomicBool,
    /// Encoder picture index of the oldest frame the client lost and asked to invalidate.
    lost: Mutex<Option<u64>>,
    /// Send-to-acknowledge delays, for the rate control.
    meter: Mutex<DelayMeter>,
    /// Bitrate ceiling (this stream's share of the link's), for pacing.
    ceiling: AtomicU32,
}

impl Video {
    fn new() -> Self {
        Self {
            acked_id: AtomicU32::new(0),
            acked: AtomicU64::new(0),
            keyframe_requested: AtomicBool::new(false),
            lost: Mutex::new(None),
            meter: Mutex::default(),
            ceiling: AtomicU32::new(crate::rate::START_BPS),
        }
    }

    /// A new client: its frames are numbered from the start.
    fn reset(&self) {
        *self.lost.lock().unwrap() = None;
        self.acked_id.store(0, Ordering::SeqCst);
        self.meter.lock().unwrap().clear();
    }
}

struct Shared {
    session: Arc<pipeline::Shared>,
    index: usize,
    udp: Arc<UdpSocket>,
    link: Mutex<Option<Link>>,
    /// The video of monitor 0, and of the monitors set up after it, by monitor.
    video: Mutex<HashMap<u8, Arc<Video>>>,
    /// Parity from the client's loss reports.
    fec: Mutex<link::FecControl>,
    /// Share lost in the newest report not yet taken by the rate control.
    loss: Mutex<Option<f32>>,
}

impl Shared {
    fn seal_and_send(link: &mut Link, udp: &UdpSocket, peer: SocketAddr, plain: &[u8]) {
        link.seq += 1;
        let packet = link.key.seal(Direction::ToClient, link.seq, plain);
        if let Err(e) = udp.send_to(&packet, peer) {
            debug!(%e, "UDP send failed");
        }
    }
}

// ---------------------------------------------------------------------------------------
// Pipeline output

pub struct NativeOutput {
    shared: Arc<Shared>,
    monitor: u8,
    video: Arc<Video>,
    /// Since when frames have been held back for lack of acknowledgements.
    busy_since: Option<std::time::Instant>,
    /// Last SYNC sent while held back.
    synced: Option<std::time::Instant>,
}

/// While held back, how often the client is told which frame was the last (so it can tell
/// the last frames were lost instead of waiting for more).
const SYNC_EVERY: std::time::Duration = std::time::Duration::from_millis(20);

/// Longest wait for acknowledgements before starting over from a keyframe.
const STALL: std::time::Duration = std::time::Duration::from_millis(500);

impl Output for NativeOutput {
    fn prepare(&mut self, w: u32, h: u32, max_in_flight: u32) -> Result<Prepared> {
        let mut guard = self.shared.link.lock().unwrap();
        let Some(link) = guard.as_mut() else {
            return Ok(Prepared::Absent);
        };
        let Some(peer) = link.peer else {
            return Ok(Prepared::Absent);
        };
        // The monitors after the first only go to a client that asked for them.
        if self.monitor != 0 && !link.layout {
            return Ok(Prepared::Absent);
        }
        let mut new_surface = self.video.keyframe_requested.swap(false, Ordering::SeqCst);
        let sent = link.video.entry(self.monitor).or_default();
        if sent.size != Some((w, h)) {
            sent.size = Some((w, h));
            let (width, height) = (w as u16, h as u16);
            let _ = link.control.send(if self.monitor == 0 {
                ServerMsg::Size { width, height }
            } else {
                ServerMsg::MonitorSize {
                    monitor: self.monitor,
                    width,
                    height,
                }
            });
            new_surface = true;
        }
        let last = sent.frame_id;
        let acked = self.video.acked_id.load(Ordering::SeqCst);
        let in_flight = last.wrapping_sub(acked);
        if !new_surface && in_flight >= max_in_flight {
            let since = *self.busy_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() < STALL {
                if since.elapsed() >= SYNC_EVERY
                    && self.synced.is_none_or(|t| t.elapsed() >= SYNC_EVERY)
                {
                    self.synced = Some(std::time::Instant::now());
                    let sync = fec::sync_packet_monitor(self.monitor, last);
                    Shared::seal_and_send(link, &self.shared.udp, peer, &sync);
                }
                return Ok(Prepared::Busy);
            }
            // No acknowledgement for a long time while frames wait: whatever got lost,
            // start over from a keyframe rather than freeze the picture.
            warn!(
                monitor = self.monitor,
                sent = last,
                acked,
                ms = since.elapsed().as_millis() as u64,
                "No acknowledgements: resending a keyframe"
            );
            self.video.acked_id.store(last, Ordering::SeqCst);
            new_surface = true;
        }
        self.busy_since = None;
        self.synced = None;
        Ok(Prepared::Ready {
            new_surface,
            avc444: false,
            codec: link.codec,
        })
    }

    fn send(&mut self, _mode: Mode, main: &EncodedFrame, _aux: &EncodedFrame) -> Result<bool> {
        let mut guard = self.shared.link.lock().unwrap();
        let Some(link) = guard.as_mut() else {
            return Ok(false);
        };
        let Some(peer) = link.peer else {
            return Ok(false);
        };
        let recovery = main.recovery && link.invalidation;
        let sent = link.video.entry(self.monitor).or_default();
        sent.frame_id = sent.frame_id.wrapping_add(1);
        let frame_id = sent.frame_id;
        self.video
            .meter
            .lock()
            .unwrap()
            .sent(frame_id, main.data.len());
        if main.index != 0 {
            if sent.recent.len() >= SENT_KEPT {
                sent.recent.pop_front();
            }
            sent.recent.push_back((frame_id, main.index));
        }
        let start = std::time::Instant::now();
        let parity = self.shared.fec.lock().unwrap().percent();
        let (pace_bps, burst) =
            link::pacing(self.video.ceiling.load(Ordering::Relaxed), fec::SHARD);
        let packets = fec::packetize_monitor(
            self.monitor,
            frame_id,
            main.keyframe,
            recovery,
            &main.data,
            parity,
        );
        for (i, p) in packets.iter().enumerate() {
            Shared::seal_and_send(link, &self.shared.udp, peer, p);
            // Pacing: past the first burst, keep to a few times the bitrate ceiling so a
            // large frame (an IDR is hundreds of packets) does not overflow the queue of a
            // Wi-Fi access point or a mobile link.
            if i >= burst && i % 8 == 0 {
                let due = std::time::Duration::from_secs_f64(
                    ((i - burst) * fec::SHARD * 8) as f64 / pace_bps,
                );
                if let Some(wait) = due.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
        }
        Ok(true)
    }

    fn take_acked(&mut self) -> u64 {
        self.video.acked.swap(0, Ordering::Relaxed)
    }

    fn take_delays(&mut self) -> Vec<(std::time::Duration, usize)> {
        self.video.meter.lock().unwrap().take()
    }

    fn reset(&mut self) {
        if let Some(link) = self.shared.link.lock().unwrap().as_mut()
            && let Some(sent) = link.video.get_mut(&self.monitor)
        {
            sent.size = None;
        }
    }

    fn wants_refresh(&self) -> bool {
        self.video.keyframe_requested.load(Ordering::SeqCst)
            || self.video.lost.lock().unwrap().is_some()
    }

    fn take_lost(&mut self) -> Option<u64> {
        self.video.lost.lock().unwrap().take()
    }

    fn take_loss(&mut self) -> Option<f32> {
        // The link's losses, for monitor 0's rate control.
        if self.monitor == 0 {
            self.shared.loss.lock().unwrap().take()
        } else {
            None
        }
    }

    fn set_ceiling(&mut self, bps: u32) {
        self.video.ceiling.store(bps, Ordering::Relaxed);
    }
}

/// The native front end's outputs for the monitors after the first (protocol version 6).
pub struct NativeMonitors(Arc<Shared>);

impl pipeline::MonitorOutputs for NativeMonitors {
    fn output(&self, monitor: u8) -> Box<dyn Output> {
        let video = Arc::clone(
            self.0
                .video
                .lock()
                .unwrap()
                .entry(monitor)
                .or_insert_with(|| Arc::new(Video::new())),
        );
        Box::new(NativeOutput {
            shared: Arc::clone(&self.0),
            monitor,
            video,
            busy_since: None,
            synced: None,
        })
    }

    fn yuv444(&self) -> bool {
        self.shared
            .link
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|l| l.yuv444)
    }
}

/// Clipboard of the session, as the native front end uses it: text, and files (protocol
/// version 4) where the session supports them.
pub trait TextClipboard: Send + Sync {
    /// The client copied `text`.
    fn set(&self, text: String);
    /// Text copied in the session.
    fn subscribe(&self) -> broadcast::Receiver<String>;
    /// The client copied files, now fetched to these local paths: the session's next paste
    /// gets them.
    fn set_files(&self, _paths: Vec<std::path::PathBuf>) {}
    /// Files copied in the session, as local paths.
    fn subscribe_files(&self) -> Option<broadcast::Receiver<Vec<std::path::PathBuf>>> {
        None
    }
}

/// Where the client's copied files are fetched to (one fresh folder per copy).
fn files_cache() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".cache")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join("glidedesk")
        .join("clipboard")
}

/// Copies larger than this are not fetched (the copy is not offered for pasting).
const FILES_LIMIT: u64 = 4 << 30;

// ---------------------------------------------------------------------------------------
// Server

pub struct Front {
    pub bind: SocketAddr,
    pub username: String,
    pub password: String,
    pub state_dir: std::path::PathBuf,
    /// The encoder can produce HEVC.
    pub hevc: bool,
    pub cursor: watch::Receiver<Option<CursorEvent>>,
    pub audio: Option<broadcast::Sender<Vec<u8>>>,
    pub input: InputForwarder,
    pub clipboard: Option<Arc<dyn TextClipboard>>,
    pub auth_limiter: Arc<crate::auth::RateLimiter>,
    /// Apps clients can open (protocol version 3).
    pub apps: Arc<crate::apps::Runner>,
}

/// How native clients reach the host.
#[derive(Clone, Debug)]
pub enum Listen {
    /// TLS over TCP on the bind address, UDP on the same port.
    Tcp,
    /// Clients the system daemon logged in, over this Unix socket ([`crate::handover`]);
    /// UDP on a loopback port the daemon relays to.
    #[cfg(target_os = "linux")]
    Handover(std::path::PathBuf),
}

enum Listener {
    Tcp(TcpListener),
    #[cfg(target_os = "linux")]
    Handover(std::path::PathBuf),
}

pub struct Server {
    shared: Arc<Shared>,
    listener: Listener,
}

/// The native front end as output `index` of the pipeline: the output, and the server to
/// run. Binds the TCP and UDP ports right away (with [`Listen::Handover`], only UDP, on
/// the loopback).
pub async fn new(
    session: Arc<pipeline::Shared>,
    index: usize,
    bind: SocketAddr,
    listen: Listen,
) -> Result<(NativeOutput, Server)> {
    let (listener, bind) = match listen {
        Listen::Tcp => {
            let tcp = TcpListener::bind(bind)
                .await
                .with_context(|| format!("binding TCP {bind}"))?;
            (Listener::Tcp(tcp), bind)
        }
        #[cfg(target_os = "linux")]
        Listen::Handover(path) => (
            Listener::Handover(path),
            SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        ),
    };
    let udp = {
        let s = socket2::Socket::new(
            socket2::Domain::for_address(bind),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        // A large frame leaves as a burst of hundreds of packets.
        let _ = s.set_send_buffer_size(8 << 20);
        let _ = s.set_recv_buffer_size(1 << 20);
        s.bind(&bind.into())
            .with_context(|| format!("binding UDP {bind}"))?;
        UdpSocket::from(s)
    };
    let video = Arc::new(Video::new());
    let shared = Arc::new(Shared {
        session,
        index,
        udp: Arc::new(udp),
        link: Mutex::new(None),
        video: Mutex::new(HashMap::from([(0, Arc::clone(&video))])),
        fec: Mutex::default(),
        loss: Mutex::new(None),
    });
    Ok((
        NativeOutput {
            shared: Arc::clone(&shared),
            monitor: 0,
            video,
            busy_since: None,
            synced: None,
        },
        Server { shared, listener },
    ))
}

pub(crate) fn tls_config(dir: &Path) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    // The RDP front end creates the self-signed identity; both use it.
    let certs = CertificateDer::pem_file_iter(dir.join("cert.pem"))
        .context("reading cert.pem")?
        .collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(dir.join("key.pem")).context("reading key.pem")?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(Arc::new(config))
}

impl Server {
    /// The outputs for the pipelines of further monitors.
    pub fn monitors(&self) -> Arc<dyn pipeline::MonitorOutputs> {
        Arc::new(NativeMonitors(Arc::clone(&self.shared)))
    }

    pub async fn run(self, front: Front) -> Result<()> {
        let front = Arc::new(front);
        {
            let shared = Arc::clone(&self.shared);
            std::thread::Builder::new()
                .name("native-udp".into())
                .spawn(move || udp_loop(&shared))?;
        }
        if let Some(audio) = front.audio.clone() {
            let shared = Arc::clone(&self.shared);
            tokio::spawn(audio_task(shared, audio));
        }
        let tcp = match self.listener {
            Listener::Tcp(tcp) => tcp,
            #[cfg(target_os = "linux")]
            Listener::Handover(path) => return run_handover(&path, self.shared, front).await,
        };
        crate::rdp::ensure_identity(&front.state_dir)?;
        let acceptor = tokio_rustls::TlsAcceptor::from(tls_config(&front.state_dir)?);
        info!(bind = %front.bind, "Listening for native clients");
        loop {
            let (tcp, peer) = tcp.accept().await?;
            let _ = tcp.set_nodelay(true);
            let (acceptor, shared, front) = (
                acceptor.clone(),
                Arc::clone(&self.shared),
                Arc::clone(&front),
            );
            tokio::spawn(async move {
                info!(%peer, "Native client connecting");
                let r = async {
                    let tls = acceptor.accept(tcp).await.context("TLS handshake")?;
                    connection(tls, peer, &shared, &front, false).await
                }
                .await;
                info!(%peer, error = ?r.err().map(|e| format!("{e:#}")), "Native client disconnected");
            });
        }
    }
}

/// Takes the clients the system daemon hands over on the Unix socket at `path`.
#[cfg(target_os = "linux")]
async fn run_handover(path: &Path, shared: Arc<Shared>, front: Arc<Front>) -> Result<()> {
    let listener = crate::handover::listen(path)?;
    let udp_port = shared.udp.local_addr()?.port();
    info!(socket = %path.display(), udp_port, "Waiting for clients from the system daemon");
    loop {
        let (mut stream, _) = listener.accept().await?;
        let (shared, front) = (Arc::clone(&shared), Arc::clone(&front));
        tokio::spawn(async move {
            let peer = match crate::handover::accept(&mut stream, udp_port).await {
                Ok(peer) => peer,
                Err(e) => {
                    warn!(error = format!("{e:#}"), "Handover refused");
                    return;
                }
            };
            info!(%peer, "Native client handed over by the daemon");
            let r = connection(stream, peer, &shared, &front, true).await;
            info!(%peer, error = ?r.err().map(|e| format!("{e:#}")), "Native client disconnected");
        });
    }
}

/// UDP from clients: the hello that tells the server where to send.
fn udp_loop(shared: &Shared) {
    let mut buf = vec![0u8; 2048];
    loop {
        let Ok((n, from)) = shared.udp.recv_from(&mut buf) else {
            continue;
        };
        let mut guard = shared.link.lock().unwrap();
        let Some(link) = guard.as_mut() else { continue };
        let Some((_, plain)) = link.key.open(Direction::ToServer, &buf[..n]) else {
            continue;
        };
        if fec::parse_hello(&plain) == Some(link.token) && link.peer != Some(from) {
            info!(%from, "Native client media path open");
            let first = link.peer.is_none();
            link.peer = Some(from);
            drop(guard);
            if first {
                shared.session.take_over(shared.index);
            }
        }
    }
}

/// Session audio to the current client: the 20 ms blocks in 5 ms packets.
async fn audio_task(shared: Arc<Shared>, audio: broadcast::Sender<Vec<u8>>) {
    let mut rx = audio.subscribe();
    let mut seq: u32 = 0;
    loop {
        let block = match rx.recv().await {
            Ok(b) => b,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let mut guard = shared.link.lock().unwrap();
        let Some(link) = guard.as_mut() else { continue };
        let Some(peer) = link.peer else { continue };
        for chunk in block.chunks(960) {
            seq = seq.wrapping_add(1);
            Shared::seal_and_send(link, &shared.udp, peer, &fec::audio_packet(seq, chunk));
        }
    }
}

fn cursor_msg(ev: &CursorEvent) -> ServerMsg {
    match ev {
        CursorEvent::Hidden => ServerMsg::Cursor(Cursor::Hidden),
        CursorEvent::Shape(s) => {
            // BGRA bottom-up (the RDP layout) to RGBA top-down.
            let row = s.width as usize * 4;
            let mut rgba = Vec::with_capacity(s.bgra_bottom_up.len());
            for line in s.bgra_bottom_up.chunks(row).rev() {
                for p in line.chunks(4) {
                    rgba.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
                }
            }
            ServerMsg::Cursor(Cursor::Shape {
                width: s.width,
                height: s.height,
                hot_x: s.hot_x,
                hot_y: s.hot_y,
                rgba,
            })
        }
    }
}

/// One client. `logged_in`: the system daemon already checked its password (a handed-over
/// client), so it is not checked again.
async fn connection<S>(
    stream: S,
    peer: SocketAddr,
    shared: &Arc<Shared>,
    front: &Front,
    logged_in: bool,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let hello: ClientMsg = glidedesk_proto::read_msg(&mut rd).await?;
    let ClientMsg::Hello {
        version,
        user,
        password,
        width,
        height,
        codecs,
        refresh,
    } = hello
    else {
        bail!("expected Hello");
    };
    let ip = peer.ip();
    if !logged_in && let Some(wait) = front.auth_limiter.check(ip) {
        let reason = format!(
            "too many failed attempts; try again in {:.0}s",
            wait.as_secs_f64().max(1.0)
        );
        glidedesk_proto::write_msg(&mut wr, &ServerMsg::Denied(reason.clone())).await?;
        bail!("denied for {peer}: {reason}");
    }
    let valid_creds = logged_in
        || crate::auth::verify_credentials(&front.username, &front.password, &user, &password);
    let deny = if !(glidedesk_proto::MIN_VERSION..=glidedesk_proto::VERSION).contains(&version) {
        Some(format!(
            "protocol version {version}, server speaks {} to {}",
            glidedesk_proto::MIN_VERSION,
            glidedesk_proto::VERSION
        ))
    } else if !valid_creds {
        let lockout = front.auth_limiter.record_failure(ip);
        if let Some(wait) = lockout {
            tracing::warn!(%peer, lockout_secs = wait.as_secs(), "Authentication failed; backoff applied");
        } else {
            tracing::warn!(%peer, "Authentication failed");
        }
        Some("wrong user name or password".to_owned())
    } else {
        front.auth_limiter.record_success(ip);
        None
    };
    if let Some(reason) = deny {
        glidedesk_proto::write_msg(&mut wr, &ServerMsg::Denied(reason.clone())).await?;
        bail!("denied: {reason}");
    }
    let codec = codecs
        .iter()
        .find_map(|c| match c {
            WireCodec::Hevc if front.hevc => Some((Codec::Hevc, WireCodec::Hevc)),
            WireCodec::H264 => Some((Codec::H264, WireCodec::H264)),
            _ => None,
        })
        .context("no codec in common")?;
    let (w, h) = shared
        .session
        .request_size(u32::from(width), u32::from(height));
    *shared.session.wanted_refresh.lock().unwrap() = Some(refresh);
    info!(%peer, user, w, h, refresh, codec = ?codec.0, "Native client logged in");

    let key: [u8; 32] = glidedesk_proto::random();
    let token: [u8; 16] = glidedesk_proto::random();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
    // This client replaces any previous one, which is told why (its control task ends when
    // its sender drops).
    let previous = shared.link.lock().unwrap().replace(Link {
        key: PacketKey::new(&key),
        token,
        peer: None,
        codec: codec.0,
        control: ctl_tx.clone(),
        seq: 0,
        invalidation: version >= 2,
        yuv444: false,
        layout: false,
        video: HashMap::new(),
    });
    for v in shared.video.lock().unwrap().values() {
        v.reset();
    }
    // One monitor until this client asks for more (a previous client may have had several).
    shared.session.request_layout(Vec::new());
    if let Some(old) = previous {
        let _ = old.control.send(ServerMsg::Denied(format!(
            "another client ({peer}) took over the session"
        )));
    }
    // A new client, maybe on another link: its losses are learned from the start.
    *shared.fec.lock().unwrap() = link::FecControl::default();
    *shared.loss.lock().unwrap() = None;
    glidedesk_proto::write_msg(
        &mut wr,
        &ServerMsg::Welcome {
            key,
            token,
            codec: codec.1,
            width: w as u16,
            height: h as u16,
            audio: front.audio.is_some(),
        },
    )
    .await?;
    if version >= 3 {
        let names = front.apps.list().into_iter().map(|a| a.name).collect();
        glidedesk_proto::write_msg(&mut wr, &ServerMsg::Apps(names)).await?;
    }

    // Outgoing: queued messages, cursor changes, and the end of the app this client opened.
    let mut cursor = front.cursor.clone();
    cursor.mark_changed();
    let mut clip = front.clipboard.as_ref().map(|c| c.subscribe());
    // Files copied in the session, offered to clients from version 4 (older ones would not
    // understand the message).
    let mut session_files = front
        .clipboard
        .as_ref()
        .filter(|_| version >= 4)
        .and_then(|c| c.subscribe_files());
    let offered: Arc<Mutex<Option<(u32, Arc<files::Offer>)>>> = Arc::default();
    let offering = Arc::clone(&offered);
    let opened: Arc<Mutex<Option<String>>> = Arc::default();
    let mut app_exits = front.apps.subscribe();
    let watched = Arc::clone(&opened);
    // The monitors set up, sent to a client that asked for several (version 6).
    let mut layout = shared.session.layout.subscribe();
    let asked_layout = Arc::new(AtomicBool::new(false));
    let wants_layout = Arc::clone(&asked_layout);
    let writer = tokio::spawn(async move {
        let mut next_id = 0u32;
        loop {
            let msg = tokio::select! {
                m = ctl_rx.recv() => match m { Some(m) => m, None => break },
                l = layout.changed() => {
                    if l.is_err() { break }
                    let monitors = layout.borrow_and_update().clone();
                    if monitors.is_empty() || !wants_layout.load(Ordering::Relaxed) { continue }
                    ServerMsg::Layout(monitors)
                }
                f = async { match session_files.as_mut() { Some(f) => f.recv().await, None => std::future::pending().await } } => match f {
                    Ok(paths) => {
                        let offer = match tokio::task::spawn_blocking(move || files::Offer::new(&paths)).await {
                            Ok(Ok(o)) => o,
                            Ok(Err(e)) => { warn!(%e, "Cannot offer the copied files"); continue }
                            Err(_) => continue,
                        };
                        next_id = next_id.wrapping_add(1);
                        let entries = offer.entries.clone();
                        debug!(files = entries.len(), bytes = offer.bytes(), "Offering the session's files");
                        *offering.lock().unwrap() = Some((next_id, Arc::new(offer)));
                        ServerMsg::Files { id: next_id, entries }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => { session_files = None; continue }
                },
                e = app_exits.recv() => match e {
                    Ok(e) if watched.lock().unwrap().as_deref() == Some(e.name.as_str()) => {
                        ServerMsg::AppExited { name: e.name, reason: e.reason }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                t = async { match clip.as_mut() { Some(c) => c.recv().await, None => std::future::pending().await } } => match t {
                    Ok(text) => ServerMsg::Clipboard(text),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => { clip = None; continue }
                },
                c = cursor.changed() => {
                    if c.is_err() { break }
                    // The watch guard must not live across the write below.
                    let msg = cursor.borrow_and_update().as_ref().map(cursor_msg);
                    match msg {
                        Some(m) => m,
                        None => continue,
                    }
                }
            };
            if glidedesk_proto::write_msg(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    let mut input = front.input.clone();
    let mut pos = (0u16, 0u16);
    // The client's copied files being fetched.
    let mut download: Option<files::Download> = None;
    let result = loop {
        let msg: ClientMsg = match glidedesk_proto::read_msg(&mut rd).await {
            Ok(m) => m,
            // The client closed the connection between messages: it left.
            Err(glidedesk_proto::Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break Ok(());
            }
            Err(e) => break Err(anyhow::Error::from(e)),
        };
        // A newer client took over: stop serving this one.
        if !shared
            .link
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|l| l.token == token)
        {
            break Ok(());
        }
        match msg {
            ClientMsg::Input(ev) => forward_input(&mut input, &mut pos, ev),
            ClientMsg::Resize { width, height } => {
                shared
                    .session
                    .request_size(u32::from(width), u32::from(height));
            }
            ClientMsg::FrameAck { frame_id } => {
                on_video(shared, 0, VideoMsg::FrameAck { frame_id });
            }
            ClientMsg::RequestKeyframe => on_video(shared, 0, VideoMsg::RequestKeyframe),
            ClientMsg::InvalidateFrames { first } => {
                on_video(shared, 0, VideoMsg::InvalidateFrames { first });
            }
            ClientMsg::MonitorVideo { monitor, msg } => on_video(shared, monitor, msg),
            ClientMsg::Monitors(asked) => {
                info!(%peer, monitors = asked.len(), "Client asked for monitors");
                if let Some(l) = shared.link.lock().unwrap().as_mut() {
                    l.layout = asked.len() > 1;
                }
                asked_layout.store(true, Ordering::Relaxed);
                shared.session.request_layout(asked);
            }
            ClientMsg::Ping { t_us } => {
                let _ = ctl_tx.send(ServerMsg::Pong { t_us });
            }
            ClientMsg::Clipboard(text) => {
                if let Some(c) = &front.clipboard {
                    c.set(text);
                }
            }
            ClientMsg::OpenApp(name) => {
                let reply = match front.apps.open(&name) {
                    Ok(()) => {
                        *opened.lock().unwrap() = Some(name.clone());
                        ServerMsg::AppStarted(name)
                    }
                    Err(e) => {
                        warn!(%peer, app = name, error = format!("{e:#}"), "Could not open the app");
                        ServerMsg::AppExited {
                            name,
                            reason: Some(format!("{e:#}")),
                        }
                    }
                };
                let _ = ctl_tx.send(reply);
            }
            ClientMsg::LinkReport { received, lost } => {
                let sample = link::LossSample { received, lost };
                if let Some(percent) = shared.fec.lock().unwrap().report(sample) {
                    debug!(percent, loss = sample.fraction(), "Video parity");
                }
                *shared.loss.lock().unwrap() = Some(sample.fraction());
            }
            ClientMsg::Files { id, entries } => {
                download = None;
                let Some(clipboard) = &front.clipboard else {
                    continue;
                };
                let bytes: u64 = entries.iter().map(|e| e.size).sum();
                if bytes > FILES_LIMIT {
                    warn!(bytes, "The client's copied files are too large to fetch");
                    continue;
                }
                match files::new_folder(&files_cache())
                    .and_then(|dir| files::Download::new(&dir, id, entries))
                {
                    Ok(d) => {
                        info!(%peer, bytes, "Fetching the client's copied files");
                        download = Some(d);
                        step_download(&mut download, &ctl_tx, clipboard.as_ref());
                    }
                    Err(e) => warn!(%e, "Cannot take the client's files"),
                }
            }
            ClientMsg::FileData(data) => {
                let Some(clipboard) = &front.clipboard else {
                    continue;
                };
                let Some(d) = download.as_mut().filter(|d| d.id == data.id) else {
                    continue;
                };
                let r = match &data.error {
                    Some(e) => Err(std::io::Error::other(e.clone())),
                    None => d.on_data(data.index, data.offset, &data.data),
                };
                if let Err(e) = r {
                    warn!(%e, "Fetching the client's files failed");
                    download = None;
                    continue;
                }
                step_download(&mut download, &ctl_tx, clipboard.as_ref());
            }
            ClientMsg::FileRead(r) => {
                let offer = offered
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|(id, _)| *id == r.id)
                    .map(|(_, o)| Arc::clone(o));
                let ctl = ctl_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = match offer {
                        Some(o) => o.read(r.index, r.offset, r.len),
                        None => Err(std::io::Error::other("the files are no longer copied")),
                    };
                    let (data, error) = match result {
                        Ok(d) => (d, None),
                        Err(e) => (Vec::new(), Some(e.to_string())),
                    };
                    let _ = ctl.send(ServerMsg::FileData(glidedesk_proto::FileData {
                        id: r.id,
                        index: r.index,
                        offset: r.offset,
                        data,
                        error,
                    }));
                });
            }
            ClientMsg::Formats { yuv444 } => {
                let changed = shared
                    .link
                    .lock()
                    .unwrap()
                    .as_mut()
                    .filter(|l| l.token == token)
                    .is_some_and(|l| {
                        let wanted = yuv444 && l.codec == Codec::Hevc;
                        std::mem::replace(&mut l.yuv444, wanted) != wanted
                    });
                if changed {
                    info!(%peer, yuv444, "Client picture formats");
                    // Pictures sent so far were 4:2:0: start over (the pipeline decides
                    // whether 4:4:4 is possible and opens the encoder for it), on every
                    // monitor.
                    for v in shared.video.lock().unwrap().values() {
                        v.keyframe_requested.store(true, Ordering::SeqCst);
                    }
                }
            }
            ClientMsg::Hello { .. } => warn!("Second Hello ignored"),
        }
    };
    input.keyboard(KeyboardEvent::Synchronize(
        ironrdp_pdu::input::fast_path::SynchronizeFlags::empty(),
    ));
    {
        let mut link = shared.link.lock().unwrap();
        if link.as_ref().is_some_and(|l| l.token == token) {
            *link = None;
        }
    }
    writer.abort();
    result
}

/// An acknowledgement, keyframe request or invalidation for the video of `monitor`.
fn on_video(shared: &Shared, monitor: u8, msg: VideoMsg) {
    let Some(video) = shared.video.lock().unwrap().get(&monitor).cloned() else {
        debug!(monitor, "Video message for a monitor not set up");
        return;
    };
    match msg {
        VideoMsg::FrameAck { frame_id } => {
            video.acked_id.store(frame_id, Ordering::SeqCst);
            video.acked.fetch_add(1, Ordering::Relaxed);
            video.meter.lock().unwrap().acked(frame_id);
        }
        VideoMsg::RequestKeyframe => {
            video.keyframe_requested.store(true, Ordering::SeqCst);
        }
        VideoMsg::InvalidateFrames { first } => {
            let index = shared
                .link
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|l| l.video.get(&monitor))
                .and_then(|s| {
                    s.recent
                        .iter()
                        .find(|&&(id, _)| id == first)
                        .map(|&(_, index)| index)
                });
            debug!(monitor, first, ?index, "Client lost frames");
            match index {
                Some(index) => {
                    let mut lost = video.lost.lock().unwrap();
                    *lost = Some(lost.map_or(index, |l| l.min(index)));
                }
                // Too far back, or not from an encoder that can invalidate.
                None => video.keyframe_requested.store(true, Ordering::SeqCst),
            }
        }
    }
}

/// Asks the client for the next range of its files; once they have all arrived, puts them
/// on the session's clipboard.
fn step_download(
    download: &mut Option<files::Download>,
    ctl: &mpsc::UnboundedSender<ServerMsg>,
    clipboard: &dyn TextClipboard,
) {
    let Some(d) = download.as_mut() else { return };
    match d.next_request() {
        Ok(Some((index, offset, len))) => {
            let _ = ctl.send(ServerMsg::FileRead(glidedesk_proto::FileRead {
                id: d.id,
                index,
                offset,
                len,
            }));
        }
        Ok(None) if d.done() => {
            info!(bytes = d.bytes, "The client's files are here");
            clipboard.set_files(d.top_paths());
            *download = None;
        }
        Ok(None) => {}
        Err(e) => {
            warn!(%e, "Fetching the client's files failed");
            *download = None;
        }
    }
}

/// Native input through the same translation as RDP input.
pub(crate) fn forward_input(input: &mut InputForwarder, pos: &mut (u16, u16), ev: InputEvent) {
    match ev {
        InputEvent::MouseMove { x, y } => {
            *pos = (x, y);
            input.mouse(MouseEvent::Move { x, y });
        }
        InputEvent::MouseButton { button, down } => {
            let button = match button {
                1 => MouseButton::Left,
                2 => MouseButton::Right,
                3 => MouseButton::Middle,
                4 => MouseButton::X1,
                5 => MouseButton::X2,
                _ => return,
            };
            input.mouse(MouseEvent::Button {
                x: pos.0,
                y: pos.1,
                button,
                pressed: down,
            });
        }
        InputEvent::Wheel { vertical, units } => input.mouse(if vertical {
            MouseEvent::VerticalScroll { value: units }
        } else {
            MouseEvent::HorizontalScroll { value: units }
        }),
        InputEvent::Key {
            scancode,
            extended,
            down,
        } => input.keyboard(if down {
            KeyboardEvent::Pressed {
                code: scancode,
                extended,
            }
        } else {
            KeyboardEvent::Released {
                code: scancode,
                extended,
            }
        }),
        InputEvent::Unicode { ch, down } => {
            let mut units = [0u16; 2];
            for u in ch.encode_utf16(&mut units) {
                input.keyboard(if down {
                    KeyboardEvent::UnicodePressed(*u)
                } else {
                    KeyboardEvent::UnicodeReleased(*u)
                });
            }
        }
        InputEvent::ReleaseAll => input.keyboard(KeyboardEvent::Synchronize(
            ironrdp_pdu::input::fast_path::SynchronizeFlags::empty(),
        )),
    }
}

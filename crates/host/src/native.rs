//! Native front end: the low-latency protocol of `glidedesk-proto` (ADR-0004).
//!
//! TCP + TLS carries the control messages (login, input, resize, cursor, acknowledgements);
//! UDP on the same port carries the video, in Reed-Solomon protected shards, and the audio,
//! every datagram sealed with the session key. The pipeline thread sends video straight
//! from [`NativeOutput`]; a tokio task per connection runs the control channel.

use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use glidedesk_proto::crypto::{Direction, PacketKey};
use glidedesk_proto::{ClientMsg, Codec as WireCodec, Cursor, InputEvent, ServerMsg, fec};
use ironrdp_server::{KeyboardEvent, MouseButton, MouseEvent, RdpServerInputHandler};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::encoder::{Codec, EncodedFrame};
use crate::frame::CursorEvent;
use crate::input::InputForwarder;
use crate::pipeline::{self, Mode, Output, Prepared};
use crate::rate::DelayMeter;

/// Parity added to every video block, in percent of its data shards.
const FEC_PERCENT: u32 = 20;
/// Packets of a frame sent at full speed before pacing starts (a typical P-frame fits).
const PACE_BURST: usize = 64;
/// Sending rate for the rest of a large frame.
const PACE_BITS_PER_SEC: f64 = 300e6;

/// The current client, as the pipeline and the UDP task see it.
struct Link {
    key: PacketKey,
    token: [u8; 16],
    /// Where its UDP hello came from; no video before it.
    peer: Option<SocketAddr>,
    codec: Codec,
    /// Size the client was last told about.
    size: Option<(u32, u32)>,
    control: mpsc::UnboundedSender<ServerMsg>,
    seq: u64,
    frame_id: u32,
}

struct Shared {
    session: Arc<pipeline::Shared>,
    index: usize,
    udp: Arc<UdpSocket>,
    link: Mutex<Option<Link>>,
    /// Newest frame the client acknowledged.
    acked_id: AtomicU32,
    acked: AtomicU64,
    keyframe_requested: AtomicBool,
    /// Send-to-acknowledge delays, for the rate control.
    meter: Mutex<DelayMeter>,
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
    /// Since when frames have been held back for lack of acknowledgements.
    busy_since: Option<std::time::Instant>,
}

/// Longest wait for acknowledgements before starting over from a keyframe.
const STALL: std::time::Duration = std::time::Duration::from_millis(500);

impl Output for NativeOutput {
    fn prepare(&mut self, w: u32, h: u32, max_in_flight: u32) -> Result<Prepared> {
        let mut guard = self.shared.link.lock().unwrap();
        let Some(link) = guard.as_mut() else {
            return Ok(Prepared::Absent);
        };
        if link.peer.is_none() {
            return Ok(Prepared::Absent);
        }
        let mut new_surface = self.shared.keyframe_requested.swap(false, Ordering::SeqCst);
        if link.size != Some((w, h)) {
            link.size = Some((w, h));
            let _ = link.control.send(ServerMsg::Size {
                width: w as u16,
                height: h as u16,
            });
            new_surface = true;
        }
        let acked = self.shared.acked_id.load(Ordering::SeqCst);
        let in_flight = link.frame_id.wrapping_sub(acked);
        if !new_surface && in_flight >= max_in_flight {
            let since = *self.busy_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() < STALL {
                return Ok(Prepared::Busy);
            }
            // No acknowledgement for a long time while frames wait: whatever got lost,
            // start over from a keyframe rather than freeze the picture.
            warn!(
                sent = link.frame_id,
                acked,
                ms = since.elapsed().as_millis() as u64,
                "No acknowledgements: resending a keyframe"
            );
            self.shared.acked_id.store(link.frame_id, Ordering::SeqCst);
            new_surface = true;
        }
        self.busy_since = None;
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
        link.frame_id = link.frame_id.wrapping_add(1);
        self.shared
            .meter
            .lock()
            .unwrap()
            .sent(link.frame_id, main.data.len());
        let start = std::time::Instant::now();
        let packets = fec::packetize(link.frame_id, main.keyframe, &main.data, FEC_PERCENT);
        for (i, p) in packets.iter().enumerate() {
            Shared::seal_and_send(link, &self.shared.udp, peer, p);
            // Pacing: past the first burst, keep to PACE_BITS_PER_SEC so a large frame (an
            // IDR is hundreds of packets) does not overflow a Wi-Fi access point's queue.
            if i >= PACE_BURST && i % 16 == 0 {
                let due = std::time::Duration::from_secs_f64(
                    ((i - PACE_BURST) * fec::SHARD * 8) as f64 / PACE_BITS_PER_SEC,
                );
                if let Some(wait) = due.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
        }
        Ok(true)
    }

    fn take_acked(&mut self) -> u64 {
        self.shared.acked.swap(0, Ordering::Relaxed)
    }

    fn take_delays(&mut self) -> Vec<(std::time::Duration, usize)> {
        self.shared.meter.lock().unwrap().take()
    }

    fn reset(&mut self) {
        if let Some(link) = self.shared.link.lock().unwrap().as_mut() {
            link.size = None;
        }
    }

    fn wants_refresh(&self) -> bool {
        self.shared.keyframe_requested.load(Ordering::SeqCst)
    }
}

/// Text clipboard of the session, as the native front end uses it.
pub trait TextClipboard: Send + Sync {
    /// The client copied `text`.
    fn set(&self, text: String);
    /// Text copied in the session.
    fn subscribe(&self) -> broadcast::Receiver<String>;
}

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
}

pub struct Server {
    shared: Arc<Shared>,
    tcp: TcpListener,
}

/// The native front end as output `index` of the pipeline: the output, and the server to
/// run. Binds the TCP and UDP ports right away.
pub async fn new(
    session: Arc<pipeline::Shared>,
    index: usize,
    bind: SocketAddr,
) -> Result<(NativeOutput, Server)> {
    let tcp = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding TCP {bind}"))?;
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
    let shared = Arc::new(Shared {
        session,
        index,
        udp: Arc::new(udp),
        link: Mutex::new(None),
        acked_id: AtomicU32::new(0),
        acked: AtomicU64::new(0),
        meter: Mutex::default(),
        keyframe_requested: AtomicBool::new(false),
    });
    Ok((
        NativeOutput {
            shared: Arc::clone(&shared),
            busy_since: None,
        },
        Server { shared, tcp },
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
    pub async fn run(self, front: Front) -> Result<()> {
        crate::rdp::ensure_identity(&front.state_dir)?;
        let acceptor = tokio_rustls::TlsAcceptor::from(tls_config(&front.state_dir)?);
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
        info!(bind = %front.bind, "Listening for native clients");
        loop {
            let (tcp, peer) = self.tcp.accept().await?;
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
                    connection(tls, peer, &shared, &front).await
                }
                .await;
                info!(%peer, error = ?r.err().map(|e| format!("{e:#}")), "Native client disconnected");
            });
        }
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

async fn connection<S>(
    stream: S,
    peer: SocketAddr,
    shared: &Arc<Shared>,
    front: &Front,
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
    let deny = if version != glidedesk_proto::VERSION {
        Some(format!(
            "protocol version {version}, server speaks {}",
            glidedesk_proto::VERSION
        ))
    } else if user != front.username || password != front.password {
        Some("wrong user name or password".to_owned())
    } else {
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
        size: None,
        control: ctl_tx.clone(),
        seq: 0,
        frame_id: 0,
    });
    if let Some(old) = previous {
        let _ = old.control.send(ServerMsg::Denied(format!(
            "another client ({peer}) took over the session"
        )));
    }
    shared.acked_id.store(0, Ordering::SeqCst);
    shared.meter.lock().unwrap().clear();
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

    // Outgoing: queued messages and cursor changes.
    let mut cursor = front.cursor.clone();
    cursor.mark_changed();
    let mut clip = front.clipboard.as_ref().map(|c| c.subscribe());
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                m = ctl_rx.recv() => match m { Some(m) => m, None => break },
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
    let result = loop {
        let msg: ClientMsg = match glidedesk_proto::read_msg(&mut rd).await {
            Ok(m) => m,
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
                shared.acked_id.store(frame_id, Ordering::SeqCst);
                shared.acked.fetch_add(1, Ordering::Relaxed);
                shared.meter.lock().unwrap().acked(frame_id);
            }
            ClientMsg::RequestKeyframe => {
                shared.keyframe_requested.store(true, Ordering::SeqCst);
            }
            ClientMsg::Ping { t_us } => {
                let _ = ctl_tx.send(ServerMsg::Pong { t_us });
            }
            ClientMsg::Clipboard(text) => {
                if let Some(c) = &front.clipboard {
                    c.set(text);
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

//! Web front end: the session in a browser, over WebRTC (ADR-0005).
//!
//! HTTPS, with the host's TLS identity, serves the page (`web/index.html`) and takes the
//! browser's SDP offer (`POST /offer`, with the user name and password). Media goes over one
//! UDP port through str0m (ICE-lite: the browser does the checks): the encoder's H.264 or
//! HEVC straight into RTP (nothing is re-encoded), the session's sound as Opus, and on a data
//! channel the input one way and the pointer shapes the other. The bitrate ceiling follows
//! WebRTC's own congestion control (the TWCC estimate) instead of frame acknowledgements.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use glidedesk_proto::InputEvent;
use ironrdp_server::{KeyboardEvent, RdpServerInputHandler};
use serde::Deserialize;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec as RtpCodec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output as RtcOutput, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::encoder::{Codec, EncodedFrame};
use crate::frame::CursorEvent;
use crate::input::InputForwarder;
use crate::pipeline::{self, Mode, Output, Prepared};

const PAGE: &str = include_str!("web/index.html");
/// What the congestion control may probe up to without a user limit.
const DESIRED_BPS: u64 = 200_000_000;
/// Where its estimate starts.
const INITIAL_BPS: u64 = 20_000_000;

/// The browser being served.
struct Peer {
    rtc: Rtc,
    id: u64,
    video: Option<(Mid, Pt)>,
    audio: Option<(Mid, Pt)>,
    channel: Option<ChannelId>,
    codec: Codec,
    /// Size the browser last got a keyframe for.
    size: Option<(u32, u32)>,
    /// RTP timestamps count from here.
    epoch: Instant,
    /// The host candidate it was given: where its packets arrive.
    local: SocketAddr,
    connected: bool,
}

/// What the network side hands to the async side.
enum Note {
    /// The data channel opened (send the pointer).
    ChannelOpen,
    Input(InputMsg),
    Closed,
}

struct Shared {
    session: Arc<pipeline::Shared>,
    index: usize,
    udp: UdpSocket,
    local_port: u16,
    peer: Mutex<Option<Peer>>,
    next_id: AtomicU64,
    keyframe: AtomicBool,
    /// Newest congestion-control estimate (bits/s), 0 when none yet.
    estimate: AtomicU32,
    sent: AtomicU64,
    notes: mpsc::UnboundedSender<Note>,
    /// The encoder can produce HEVC (offered to browsers that decode it).
    hevc: AtomicBool,
}

impl Shared {
    /// Runs str0m until it only waits: sends what it has, handles its events. Returns when
    /// it wants to be woken up.
    fn drive(&self, p: &mut Peer) -> Instant {
        loop {
            let out = match p.rtc.poll_output() {
                Ok(o) => o,
                Err(e) => {
                    debug!(%e, "WebRTC error");
                    p.rtc.disconnect();
                    return Instant::now() + Duration::from_millis(100);
                }
            };
            match out {
                RtcOutput::Timeout(t) => return t,
                RtcOutput::Transmit(t) => {
                    if let Err(e) = self.udp.send_to(&t.contents, t.destination) {
                        debug!(%e, "UDP send failed");
                    }
                }
                RtcOutput::Event(e) => self.event(p, e),
            }
        }
    }

    fn event(&self, p: &mut Peer, e: Event) {
        match e {
            Event::Connected => {
                info!(id = p.id, "Browser connected");
                p.connected = true;
                self.keyframe.store(true, Ordering::SeqCst);
                self.session.take_over(self.index);
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                p.rtc.disconnect();
            }
            Event::MediaAdded(m) => {
                let Some(writer) = p.rtc.writer(m.mid) else {
                    return;
                };
                match m.kind {
                    MediaKind::Video => {
                        let choice = pick_video(
                            writer.payload_params().map(|pp| {
                                (pp.pt(), pp.spec().codec, pp.spec().format.profile_level_id)
                            }),
                            self.hevc_allowed(),
                        );
                        if let Some((pt, codec)) = choice {
                            info!(?codec, "Browser video");
                            p.video = Some((m.mid, pt));
                            p.codec = codec;
                        } else {
                            warn!("The browser offers no codec we send");
                        }
                    }
                    MediaKind::Audio => {
                        p.audio = writer
                            .payload_params()
                            .find(|pp| pp.spec().codec == RtpCodec::Opus)
                            .map(|pp| (m.mid, pp.pt()));
                    }
                }
            }
            Event::KeyframeRequest(_) => self.keyframe.store(true, Ordering::SeqCst),
            Event::ChannelOpen(id, label) => {
                debug!(label, "Data channel open");
                p.channel = Some(id);
                let _ = self.notes.send(Note::ChannelOpen);
            }
            Event::ChannelData(d) => match serde_json::from_slice::<InputMsg>(&d.data) {
                Ok(m) => {
                    let _ = self.notes.send(Note::Input(m));
                }
                Err(e) => debug!(%e, "Bad input message"),
            },
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                self.estimate.store(
                    estimate.as_u64().min(u64::from(u32::MAX)) as u32,
                    Ordering::Relaxed,
                );
            }
            _ => {}
        }
    }

    fn hevc_allowed(&self) -> bool {
        self.hevc.load(Ordering::Relaxed)
    }
}

/// Video payload type: HEVC when both sides can, else H.264 (High, then Main, then any
/// profile in packetization mode 1; browsers decode High whatever was negotiated).
fn pick_video(
    params: impl Iterator<Item = (Pt, RtpCodec, Option<u32>)>,
    hevc: bool,
) -> Option<(Pt, Codec)> {
    let mut best: Option<(u32, Pt, Codec)> = None;
    for (pt, codec, profile) in params {
        let score = match (codec, profile.map(|p| p >> 16)) {
            (RtpCodec::H265, _) if hevc => 10,
            (RtpCodec::H264, Some(0x64)) => 5,
            (RtpCodec::H264, Some(0x4d)) => 4,
            (RtpCodec::H264, _) => 3,
            _ => continue,
        };
        let codec = if codec == RtpCodec::H265 {
            Codec::Hevc
        } else {
            Codec::H264
        };
        if best.is_none_or(|(s, _, _)| score > s) {
            best = Some((score, pt, codec));
        }
    }
    best.map(|(_, pt, c)| (pt, c))
}

// ---------------------------------------------------------------------------------------
// Pipeline output

pub struct WebOutput {
    shared: Arc<Shared>,
}

impl Output for WebOutput {
    fn prepare(&mut self, w: u32, h: u32, _max_in_flight: u32) -> Result<Prepared> {
        let mut guard = self.shared.peer.lock().unwrap();
        let Some(p) = guard.as_mut() else {
            return Ok(Prepared::Absent);
        };
        if !p.connected || p.video.is_none() {
            return Ok(Prepared::Absent);
        }
        let mut new_surface = self.shared.keyframe.swap(false, Ordering::SeqCst);
        if p.size != Some((w, h)) {
            p.size = Some((w, h));
            new_surface = true;
        }
        // No acknowledgements over WebRTC: the pacer and the congestion control keep the
        // queue short, the ceiling follows their estimate.
        Ok(Prepared::Ready {
            new_surface,
            avc444: false,
            codec: p.codec,
        })
    }

    fn send(&mut self, _mode: Mode, main: &EncodedFrame, _aux: &EncodedFrame) -> Result<bool> {
        let mut guard = self.shared.peer.lock().unwrap();
        let Some(p) = guard.as_mut() else {
            return Ok(false);
        };
        let Some((mid, pt)) = p.video else {
            return Ok(false);
        };
        let now = Instant::now();
        let ticks = now.duration_since(p.epoch).as_micros() as u64 * 9 / 100;
        let Some(writer) = p.rtc.writer(mid) else {
            return Ok(false);
        };
        writer
            .write(
                pt,
                now,
                MediaTime::new(ticks, Frequency::NINETY_KHZ),
                main.data.clone(),
            )
            .context("writing video")?;
        self.shared.sent.fetch_add(1, Ordering::Relaxed);
        self.shared.drive(p);
        Ok(true)
    }

    fn take_acked(&mut self) -> u64 {
        self.shared.sent.swap(0, Ordering::Relaxed)
    }

    fn take_delays(&mut self) -> Vec<(Duration, usize)> {
        Vec::new()
    }

    fn reset(&mut self) {
        if let Some(p) = self.shared.peer.lock().unwrap().as_mut() {
            p.size = None;
        }
    }

    fn wants_refresh(&self) -> bool {
        self.shared.keyframe.load(Ordering::SeqCst)
    }

    fn link_estimate(&mut self) -> Option<u32> {
        let e = self.shared.estimate.load(Ordering::Relaxed);
        (e > 0).then_some(e)
    }
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
    pub max_bitrate: Option<u32>,
    pub cursor: watch::Receiver<Option<CursorEvent>>,
    pub audio: Option<broadcast::Sender<Vec<u8>>>,
    pub input: InputForwarder,
    pub clipboard: Option<Arc<dyn crate::native::TextClipboard>>,
}

pub struct Server {
    shared: Arc<Shared>,
    tcp: TcpListener,
    notes: mpsc::UnboundedReceiver<Note>,
}

/// The web front end as output `index` of the pipeline. Binds TCP (HTTPS) and UDP (media)
/// on the same port right away.
pub async fn new(
    session: Arc<pipeline::Shared>,
    index: usize,
    bind: SocketAddr,
) -> Result<(WebOutput, Server)> {
    let tcp = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding TCP {bind}"))?;
    let udp = {
        let s = socket2::Socket::new(
            socket2::Domain::for_address(bind),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        let _ = s.set_send_buffer_size(8 << 20);
        let _ = s.set_recv_buffer_size(1 << 20);
        s.bind(&bind.into())
            .with_context(|| format!("binding UDP {bind}"))?;
        UdpSocket::from(s)
    };
    let local_port = udp.local_addr()?.port();
    let (notes_tx, notes) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        session,
        index,
        udp,
        local_port,
        peer: Mutex::new(None),
        next_id: AtomicU64::new(1),
        keyframe: AtomicBool::new(false),
        estimate: AtomicU32::new(0),
        sent: AtomicU64::new(0),
        notes: notes_tx,
        hevc: AtomicBool::new(false),
    });
    Ok((
        WebOutput {
            shared: Arc::clone(&shared),
        },
        Server { shared, tcp, notes },
    ))
}

impl Server {
    pub async fn run(self, front: Front) -> Result<()> {
        crate::rdp::ensure_identity(&front.state_dir)?;
        let acceptor =
            tokio_rustls::TlsAcceptor::from(crate::native::tls_config(&front.state_dir)?);
        self.shared.hevc.store(front.hevc, Ordering::Relaxed);
        let front = Arc::new(front);
        {
            let shared = Arc::clone(&self.shared);
            std::thread::Builder::new()
                .name("web-udp".into())
                .spawn(move || udp_loop(&shared))?;
        }
        if let Some(audio) = front.audio.clone() {
            let shared = Arc::clone(&self.shared);
            std::thread::Builder::new()
                .name("web-audio".into())
                .spawn(move || audio_loop(&shared, &audio))?;
        }
        tokio::spawn(notes_task(
            Arc::clone(&self.shared),
            self.notes,
            Arc::clone(&front),
        ));
        info!(bind = %front.bind, "Listening for browsers (https://<host>:{})", front.bind.port());
        loop {
            let (tcp, peer) = self.tcp.accept().await?;
            let local = tcp.local_addr()?;
            let (acceptor, shared, front) = (
                acceptor.clone(),
                Arc::clone(&self.shared),
                Arc::clone(&front),
            );
            tokio::spawn(async move {
                let r = async {
                    let tls = acceptor.accept(tcp).await.context("TLS handshake")?;
                    http(tls, peer, local.ip(), &shared, &front).await
                }
                .await;
                if let Err(e) = r {
                    debug!(%peer, error = format!("{e:#}"), "Web request failed");
                }
            });
        }
    }
}

/// Media from browsers: STUN, DTLS, RTCP and the data channel.
fn udp_loop(shared: &Shared) {
    let mut buf = vec![0u8; 2000];
    loop {
        let wake = {
            let mut guard = shared.peer.lock().unwrap();
            match guard.as_mut() {
                Some(p) => {
                    let wake = shared.drive(p);
                    if !p.rtc.is_alive() {
                        info!(id = p.id, "Browser disconnected");
                        let _ = shared.notes.send(Note::Closed);
                        *guard = None;
                    }
                    wake
                }
                None => Instant::now() + Duration::from_millis(50),
            }
        };
        let wait = wake
            .saturating_duration_since(Instant::now())
            .clamp(Duration::from_millis(1), Duration::from_millis(50));
        let _ = shared.udp.set_read_timeout(Some(wait));
        let received = shared.udp.recv_from(&mut buf);
        let mut guard = shared.peer.lock().unwrap();
        let Some(p) = guard.as_mut() else { continue };
        let now = Instant::now();
        let input = match received {
            Ok((n, source)) => {
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                let destination = p.local;
                let input = Input::Receive(
                    now,
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination,
                        contents,
                    },
                );
                if !p.rtc.accepts(&input) {
                    continue;
                }
                input
            }
            Err(_) => Input::Timeout(now),
        };
        if let Err(e) = p.rtc.handle_input(input) {
            debug!(%e, "WebRTC input");
        }
    }
}

/// Session sound as Opus, 20 ms per packet.
fn audio_loop(shared: &Shared, audio: &broadcast::Sender<Vec<u8>>) {
    let mut rx = audio.subscribe();
    let mut enc =
        match opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::LowDelay) {
            Ok(e) => e,
            Err(e) => {
                warn!(%e, "No Opus encoder: browsers get no sound");
                return;
            }
        };
    let _ = enc.set_bitrate(opus::Bitrate::Bits(128_000));
    let mut out = vec![0u8; 4000];
    let mut samples: u64 = 0;
    loop {
        let block = match rx.blocking_recv() {
            Ok(b) => b,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let pcm: Vec<i16> = block
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        // 20 ms of stereo: 960 frames.
        for frame in pcm.as_chunks::<1920>().0 {
            let Ok(n) = enc.encode(frame, &mut out) else {
                continue;
            };
            samples += 960;
            let mut guard = shared.peer.lock().unwrap();
            let Some(p) = guard.as_mut() else { continue };
            let Some((mid, pt)) = p.audio.filter(|_| p.connected) else {
                continue;
            };
            if let Some(w) = p.rtc.writer(mid) {
                let t = MediaTime::new(samples, Frequency::FORTY_EIGHT_KHZ);
                if let Err(e) = w.write(pt, Instant::now(), t, out[..n].to_vec()) {
                    debug!(%e, "Writing audio");
                }
            }
            shared.drive(p);
        }
    }
}

/// Input and pointer shapes: the async side of the data channel.
async fn notes_task(
    shared: Arc<Shared>,
    mut notes: mpsc::UnboundedReceiver<Note>,
    front: Arc<Front>,
) {
    let mut input = front.input.clone();
    let mut pos = (0u16, 0u16);
    let mut cursor = front.cursor.clone();
    let mut clip = front.clipboard.as_ref().map(|c| c.subscribe());
    let send_bytes = |shared: &Shared, msg: &[u8]| {
        let mut guard = shared.peer.lock().unwrap();
        let Some(p) = guard.as_mut() else { return };
        let Some(id) = p.channel else { return };
        if let Some(mut ch) = p.rtc.channel(id) {
            let _ = ch.write(true, msg);
        }
        shared.drive(p);
    };
    let send_cursor = |shared: &Shared, ev: Option<&CursorEvent>| {
        let Some(ev) = ev else { return };
        let msg = cursor_bytes(ev);
        let mut guard = shared.peer.lock().unwrap();
        let Some(p) = guard.as_mut() else { return };
        let Some(id) = p.channel else { return };
        if let Some(mut ch) = p.rtc.channel(id) {
            let _ = ch.write(true, &msg);
        }
        shared.drive(p);
    };
    loop {
        tokio::select! {
            n = notes.recv() => {
                let Some(n) = n else { return };
                match n {
                    Note::ChannelOpen => send_cursor(&shared, cursor.borrow().as_ref()),
                    Note::Input(m) => {
                        tracing::trace!(?m, "Browser input");
                        match m {
                        InputMsg::Size { w, h } => {
                            shared.session.request_size(w, h);
                        }
                        InputMsg::Clip { text } => {
                            if let Some(c) = &front.clipboard {
                                c.set(text);
                            }
                        }
                        InputMsg::Text { ch } => {
                            for down in [true, false] {
                                crate::native::forward_input(&mut input, &mut pos, InputEvent::Unicode { ch, down });
                            }
                        }
                        other => {
                            if let Some(ev) = other.into_event() {
                                crate::native::forward_input(&mut input, &mut pos, ev);
                            }
                        }
                    }
                    }
                    Note::Closed => input.keyboard(KeyboardEvent::Synchronize(
                        ironrdp_pdu::input::fast_path::SynchronizeFlags::empty(),
                    )),
                }
            }
            c = cursor.changed() => {
                if c.is_err() { return }
                let ev = cursor.borrow_and_update().clone();
                send_cursor(&shared, ev.as_ref());
            }
            t = async { match clip.as_mut() { Some(c) => c.recv().await, None => std::future::pending().await } } => match t {
                Ok(text) => {
                    // [2, UTF-8]: text copied in the session.
                    let mut msg = vec![2];
                    msg.extend_from_slice(text.as_bytes());
                    send_bytes(&shared, &msg);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => clip = None,
            },
        }
    }
}

/// Pointer shape for the page: `[0]` hidden, or `[1, w, h, hot_x, hot_y (u16 LE), RGBA
/// top-down]`.
fn cursor_bytes(ev: &CursorEvent) -> Vec<u8> {
    match ev {
        CursorEvent::Hidden => vec![0],
        CursorEvent::Shape(s) => {
            let mut out = vec![1];
            for v in [s.width, s.height, s.hot_x, s.hot_y] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            let row = s.width as usize * 4;
            for line in s.bgra_bottom_up.chunks(row).rev() {
                for p in line.as_chunks::<4>().0 {
                    out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
                }
            }
            out
        }
    }
}

/// Messages from the page's data channel.
#[derive(Deserialize, Debug)]
#[serde(tag = "t", rename_all = "lowercase")]
enum InputMsg {
    Move {
        x: u16,
        y: u16,
    },
    Button {
        b: u8,
        down: bool,
    },
    Wheel {
        v: bool,
        units: i16,
    },
    Key {
        sc: u8,
        ext: bool,
        down: bool,
    },
    Text {
        ch: char,
    },
    /// Text copied in the browser.
    Clip {
        text: String,
    },
    Release,
    Size {
        w: u32,
        h: u32,
    },
}

impl InputMsg {
    fn into_event(self) -> Option<InputEvent> {
        Some(match self {
            Self::Move { x, y } => InputEvent::MouseMove { x, y },
            Self::Button { b, down } => InputEvent::MouseButton { button: b, down },
            Self::Wheel { v, units } => InputEvent::Wheel { vertical: v, units },
            Self::Key { sc, ext, down } => InputEvent::Key {
                scancode: sc,
                extended: ext,
                down,
            },
            Self::Release => InputEvent::ReleaseAll,
            Self::Text { .. } | Self::Clip { .. } | Self::Size { .. } => return None,
        })
    }
}

// ---------------------------------------------------------------------------------------
// HTTPS: the page and the offer

#[derive(Deserialize)]
struct OfferMsg {
    user: String,
    password: String,
    sdp: String,
    width: u32,
    height: u32,
    /// The browser's screen refresh rate, measured by the page.
    #[serde(default)]
    refresh: Option<u32>,
}

/// One request per connection: `GET /` or `POST /offer`.
async fn http<S>(
    mut s: S,
    peer: SocketAddr,
    local_ip: IpAddr,
    shared: &Arc<Shared>,
    front: &Front,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        let mut chunk = [0u8; 4096];
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed before the request");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 16 << 10 {
            bail!("request head too large");
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (
        first.next().unwrap_or_default(),
        first.next().unwrap_or_default(),
    );
    let length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    if length > 256 << 10 {
        bail!("request body too large");
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let mut chunk = vec![0u8; length - body.len()];
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed in the body");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let (status, kind, out) = match (method, path) {
        ("GET", "/" | "/index.html") => (
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes().to_vec(),
        ),
        ("POST", "/offer") => match offer(&body, peer, local_ip, shared, front) {
            Ok(answer) => ("200 OK", "application/json", answer.into_bytes()),
            Err(e) => {
                let msg = format!("{e:#}");
                warn!(%peer, error = msg, "Browser refused");
                let status = if msg.contains("user name or password") {
                    "403 Forbidden"
                } else {
                    "400 Bad Request"
                };
                (status, "text/plain; charset=utf-8", msg.into_bytes())
            }
        },
        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        out.len()
    );
    s.write_all(header.as_bytes()).await?;
    s.write_all(&out).await?;
    s.shutdown().await.ok();
    Ok(())
}

/// Checks the login, sets up the WebRTC session and returns the SDP answer (JSON).
fn offer(
    body: &[u8],
    peer: SocketAddr,
    local_ip: IpAddr,
    shared: &Arc<Shared>,
    front: &Front,
) -> Result<String> {
    let m: OfferMsg = serde_json::from_slice(body).context("reading the offer")?;
    if m.user != front.username || m.password != front.password {
        bail!("wrong user name or password");
    }
    let offer = SdpOffer::from_sdp_string(&m.sdp).context("parsing the SDP offer")?;
    let now = Instant::now();
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_h264(true)
        .enable_h265(front.hevc)
        .enable_opus(true, false)
        .enable_bwe(Some(Bitrate::from(INITIAL_BPS)))
        .build(now);
    let desired = front
        .max_bitrate
        .map_or(DESIRED_BPS, |b| u64::from(b).min(DESIRED_BPS));
    rtc.bwe().set_desired_bitrate(Bitrate::from(desired));
    // The address the browser reached us on is one it can send media to.
    let local = SocketAddr::new(local_ip, shared.local_port);
    let candidate = Candidate::host(local, "udp").context("host candidate")?;
    rtc.add_local_candidate(candidate);
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .context("accepting the offer")?;
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    shared.session.request_size(m.width, m.height);
    if let Some(hz) = m.refresh.filter(|hz| (24..=500).contains(hz)) {
        *shared.session.wanted_refresh.lock().unwrap() = Some(hz);
    }
    shared.estimate.store(0, Ordering::Relaxed);
    let previous = shared.peer.lock().unwrap().replace(Peer {
        rtc,
        id,
        video: None,
        audio: None,
        channel: None,
        codec: Codec::H264,
        size: None,
        epoch: now,
        local,
        connected: false,
    });
    if previous.is_some() {
        info!("A new browser takes over from the previous one");
    }
    info!(%peer, user = m.user, w = m.width, h = m.height, refresh = m.refresh, "Browser logged in");
    Ok(serde_json::json!({ "sdp": answer.to_sdp_string() }).to_string())
}

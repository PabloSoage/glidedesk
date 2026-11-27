//! Client of the native low-latency protocol (`glidedesk-proto`, ADR-0004).
//!
//! The control channel is TLS over TCP; video and audio arrive over UDP, sealed with the
//! session key the server sends at login. Video frames are rebuilt from their Reed-Solomon
//! shards and go to the same [`VideoSink`] as RDP's; a frame that cannot be rebuilt makes
//! the client ask for an IDR and skip frames until it arrives (decoding P-frames without
//! their reference would only show garbage).
//!
//! With several monitors (protocol version 6, [`NativeOptions::monitors`]) the video of each
//! monitor is a stream of its own: its own frames, losses and acknowledgements.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use glidedesk_proto::crypto::{Direction, PacketKey, ReplayWindow};
use glidedesk_proto::{ClientMsg, Codec, InputEvent, ServerMsg, VideoMsg, fec, files, link};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::gfx::SharedSink;
use crate::session::Input;

/// While waiting for an IDR, ask again after this long.
const KEYFRAME_RETRY: Duration = Duration::from_millis(150);
/// A frame with no packet for this long when the server says it sent it is not coming.
const SYNC_QUIET: Duration = Duration::from_millis(15);
/// With frames waiting for the decoder, look this often whether they can be acknowledged.
const ACK_POLL: Duration = Duration::from_millis(2);
/// Socket buffers for UDP: a large frame arrives as a burst of hundreds of packets.
pub const UDP_BUFFER: usize = 8 << 20;
/// How often the link is reported to the server (version 4), pinged and shown.
const LINK_PERIOD: Duration = Duration::from_secs(1);

/// What the media task counts for the link report.
#[derive(Default)]
struct LinkCount {
    packets: link::LossCounter,
    frames_lost: u32,
}

pub struct NativeOptions {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub width: u16,
    pub height: u16,
    /// Codecs the platform decodes, preferred first.
    pub codecs: Vec<Codec>,
    /// Refresh rate of the client's display.
    pub refresh: u32,
    /// Which server certificate to accept.
    pub trust: crate::trust::Trust,
    /// Open this app of the server instead of the desktop (protocol version 3); the session
    /// ends when the app does.
    pub app: Option<String>,
    /// Show the desktop on these monitors (servers from version 6): monitor 0, with the size
    /// above, and the others ([`crate::gfx::VideoSink::reset_monitor`] and the rest). Empty
    /// or one: a single-monitor session.
    pub monitors: Vec<glidedesk_proto::Monitor>,
}

/// Accepts the server's self-signed certificate if [`Trust`](crate::trust::Trust) does, and
/// remembers what it saw.
#[derive(Debug)]
struct Pin {
    trust: crate::trust::Trust,
    seen: std::sync::Mutex<Option<[u8; 32]>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fp = crate::trust::fingerprint(end_entity.as_ref());
        *self.seen.lock().unwrap() = Some(fp);
        self.trust
            .check(&fp)
            .map(|()| rustls::client::danger::ServerCertVerified::assertion())
            .map_err(rustls::Error::General)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn input_event(i: Input) -> Option<ClientMsg> {
    Some(match i {
        Input::MouseMove { x, y } => ClientMsg::Input(InputEvent::MouseMove { x, y }),
        Input::MouseButton { button, pressed } => ClientMsg::Input(InputEvent::MouseButton {
            // The wire numbers buttons from 1: left, right, middle, back, forward.
            button: match button {
                0 => 1,
                1 => 3,
                2 => 2,
                b => b + 1,
            },
            down: pressed,
        }),
        Input::Wheel { vertical, units } => ClientMsg::Input(InputEvent::Wheel { vertical, units }),
        Input::Key {
            scancode,
            extended,
            pressed,
        } => ClientMsg::Input(InputEvent::Key {
            scancode: scancode as u8,
            extended,
            down: pressed,
        }),
        Input::Unicode { ch, pressed } => {
            ClientMsg::Input(InputEvent::Unicode { ch, down: pressed })
        }
        Input::Resize { width, height } => ClientMsg::Resize {
            width: width.min(u32::from(u16::MAX)) as u16,
            height: height.min(u32::from(u16::MAX)) as u16,
        },
        Input::Clipboard(text) => ClientMsg::Clipboard(text),
        // Files are handled by the session loop (they need an offer kept for reads).
        Input::Disconnect | Input::Files(_) => return None,
    })
}

/// What the video waits for before frames can be decoded again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Nothing,
    /// An IDR (the first one, or the server could not do better).
    Keyframe,
    /// A frame with the recovery flag, after an [`ClientMsg::InvalidateFrames`].
    Recovery,
}

/// The video of one monitor, as the media task follows it.
struct Stream {
    monitor: u8,
    frames: fec::Reassembler,
    /// Until the first IDR, and after a lost frame until the server's recovery frame (or an
    /// IDR), P-frames cannot be decoded. Either can be lost too: a keyframe is asked for
    /// while waiting too long.
    wait: Wait,
    asked: Instant,
    /// Received frames not acknowledged yet, oldest first.
    held: std::collections::VecDeque<u32>,
}

impl Stream {
    fn new(monitor: u8) -> Self {
        Self {
            monitor,
            frames: fec::Reassembler::default(),
            wait: Wait::Keyframe,
            asked: Instant::now(),
            held: std::collections::VecDeque::new(),
        }
    }

    fn send(&self, ctl: &mpsc::UnboundedSender<ClientMsg>, msg: VideoMsg) {
        let _ = ctl.send(msg.for_monitor(self.monitor));
    }

    /// Acts on what the reassembler found; returns the frames delivered and lost.
    fn handle(
        &mut self,
        events: Vec<fec::Event>,
        invalidation: bool,
        sink: &SharedSink,
        ctl: &mpsc::UnboundedSender<ClientMsg>,
        spread_total: &mut Duration,
    ) -> (u32, u32) {
        let (mut got, mut lost) = (0, 0);
        for ev in events {
            match ev {
                fec::Event::Frame {
                    frame_id,
                    keyframe,
                    recovery,
                    data,
                    spread,
                } => {
                    got += 1;
                    *spread_total += spread;
                    if keyframe || (recovery && self.wait == Wait::Recovery) {
                        if self.wait != Wait::Nothing {
                            debug!(monitor = self.monitor, frame_id, keyframe, "Recovered");
                        }
                        self.wait = Wait::Nothing;
                    }
                    if self.wait == Wait::Nothing {
                        let mut sink = sink.lock().unwrap();
                        if self.monitor == 0 {
                            sink.frame(&data);
                        } else {
                            sink.frame_monitor(self.monitor, &data);
                        }
                    } else if self.asked.elapsed() > KEYFRAME_RETRY {
                        self.asked = Instant::now();
                        self.wait = Wait::Keyframe;
                        self.send(ctl, VideoMsg::RequestKeyframe);
                    }
                    self.held.push_back(frame_id);
                }
                fec::Event::Lost { frame_id } => {
                    lost += 1;
                    debug!(monitor = self.monitor, frame_id, "Frame lost");
                    match self.wait {
                        // Version 2: the server stops referencing it. (The wait for recovery
                        // keeps its start: a keyframe is still asked for if recovery takes
                        // too long.)
                        Wait::Nothing | Wait::Recovery if invalidation => {
                            if self.wait == Wait::Nothing {
                                self.asked = Instant::now();
                            }
                            self.wait = Wait::Recovery;
                            self.send(ctl, VideoMsg::InvalidateFrames { first: frame_id });
                        }
                        Wait::Nothing => {
                            self.wait = Wait::Keyframe;
                            self.asked = Instant::now();
                            self.send(ctl, VideoMsg::RequestKeyframe);
                        }
                        _ if self.asked.elapsed() > KEYFRAME_RETRY => {
                            self.wait = Wait::Keyframe;
                            self.asked = Instant::now();
                            self.send(ctl, VideoMsg::RequestKeyframe);
                        }
                        _ => {}
                    }
                }
            }
        }
        (got, lost)
    }

    /// Acknowledges the received frames the decoder has caught up with. One may still be
    /// decoding, so the server can already send the next one.
    fn release_acks(&mut self, sink: &SharedSink, ctl: &mpsc::UnboundedSender<ClientMsg>) {
        if self.held.is_empty() {
            return;
        }
        let backlog = {
            let sink = sink.lock().unwrap();
            if self.monitor == 0 {
                sink.backlog()
            } else {
                sink.backlog_monitor(self.monitor)
            }
        };
        let waiting = backlog.saturating_sub(1);
        while self.held.len() > waiting {
            let Some(frame_id) = self.held.pop_front() else {
                break;
            };
            self.send(ctl, VideoMsg::FrameAck { frame_id });
        }
    }
}

/// A server that speaks an older protocol refused ours: the fingerprint of its certificate
/// (already accepted for this connection) and the newest version it named.
#[derive(Debug)]
struct OldServer([u8; 32], Option<u32>);

impl std::fmt::Display for OldServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server speaks an older protocol version")
    }
}

impl std::error::Error for OldServer {}

/// Runs a session until the server closes it or `input` sends [`Input::Disconnect`].
/// Returns the SHA-256 fingerprint of the server certificate (to pin next time).
pub async fn run(
    mut opts: NativeOptions,
    sink: SharedSink,
    mut input: mpsc::UnboundedReceiver<Input>,
) -> Result<[u8; 32]> {
    let mut version = glidedesk_proto::VERSION;
    loop {
        let r = session(&opts, &sink, &mut input, version).await;
        let old = match &r {
            Err(e) => e.downcast_ref::<OldServer>().map(|o| (o.0, o.1)),
            Ok(_) => None,
        };
        let Some((fingerprint, newest)) = old else {
            return r;
        };
        // glidedesk 0.3.0 says "server speaks 1 to 1", 0.3.1 "1 to 2"; anything else, the
        // oldest. The certificate was just accepted, so it is not asked about again.
        version = newest
            .filter(|&v| (glidedesk_proto::MIN_VERSION..version).contains(&v))
            .unwrap_or(glidedesk_proto::MIN_VERSION);
        info!(version, "Older server: speaking its protocol version");
        opts.trust.pinned = Some(fingerprint);
    }
}

/// The newest version in a refusal like "protocol version 3, server speaks 1 to 2".
fn newest_version(reason: &str) -> Option<u32> {
    reason.rsplit(' ').next()?.parse().ok()
}

/// One connection, speaking protocol `version`.
async fn session(
    opts: &NativeOptions,
    sink: &SharedSink,
    input: &mut mpsc::UnboundedReceiver<Input>,
    version: u32,
) -> Result<[u8; 32]> {
    let invalidation = version >= 2;
    let tcp = TcpStream::connect((opts.host.as_str(), opts.port))
        .await
        .with_context(|| format!("connecting to {}:{}", opts.host, opts.port))?;
    tcp.set_nodelay(true)?;
    let server: SocketAddr = tcp.peer_addr()?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let pin = Arc::new(Pin {
        trust: opts.trust.clone(),
        seen: std::sync::Mutex::new(None),
        provider: Arc::clone(&provider),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(pin.clone())
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("glidedesk")?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .context("TLS handshake")?;
    let fingerprint = pin.seen.lock().unwrap().context("no server certificate")?;
    let (mut rd, mut wr) = tokio::io::split(tls);

    glidedesk_proto::write_msg(
        &mut wr,
        &ClientMsg::Hello {
            version,
            user: opts.username.clone(),
            password: opts.password.clone(),
            width: opts.width,
            height: opts.height,
            codecs: opts.codecs.clone(),
            refresh: opts.refresh,
        },
    )
    .await?;
    let (key, token, codec, width, height) = match glidedesk_proto::read_msg(&mut rd).await? {
        ServerMsg::Welcome {
            key,
            token,
            codec,
            width,
            height,
            ..
        } => (key, token, codec, width, height),
        ServerMsg::Denied(reason)
            if version > glidedesk_proto::MIN_VERSION && reason.starts_with("protocol version") =>
        {
            return Err(OldServer(fingerprint, newest_version(&reason)).into());
        }
        ServerMsg::Denied(reason) => bail!("the server refused the login: {reason}"),
        other => bail!("unexpected first message {other:?}"),
    };
    info!(?codec, width, height, version, "Logged in");
    sink.lock()
        .unwrap()
        .reset(u32::from(width), u32::from(height), codec);
    // Version 5: HEVC 4:4:4 where this client shows it. Sent before the UDP hello, so the
    // server usually knows before its first picture (if not, it starts over with a
    // keyframe).
    if version >= 5 && codec == Codec::Hevc && sink.lock().unwrap().yuv444() {
        info!("Asking for HEVC 4:4:4");
        glidedesk_proto::write_msg(&mut wr, &ClientMsg::Formats { yuv444: true }).await?;
    }
    if let Some(app) = &opts.app {
        if version < 3 {
            bail!("this server is too old to open apps (it needs glidedesk 0.4 or newer)");
        }
        glidedesk_proto::write_msg(&mut wr, &ClientMsg::OpenApp(app.clone())).await?;
    }
    if opts.monitors.len() > 1 {
        if version >= 6 {
            info!(monitors = opts.monitors.len(), "Asking for several monitors");
            glidedesk_proto::write_msg(&mut wr, &ClientMsg::Monitors(opts.monitors.clone()))
                .await?;
        } else {
            warn!(version, "This server shows one monitor (several need protocol version 6)");
            sink.lock().unwrap().layout(&opts.monitors[..1]);
        }
    }

    let key = Arc::new(PacketKey::new(&key));
    let udp = {
        let s = socket2::Socket::new(
            socket2::Domain::for_address(server),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        let _ = s.set_recv_buffer_size(UDP_BUFFER);
        let any: SocketAddr = if server.is_ipv4() {
            "0.0.0.0:0".parse()?
        } else {
            "[::]:0".parse()?
        };
        s.bind(&any.into())?;
        s.set_nonblocking(true)?;
        UdpSocket::from_std(s.into())?
    };
    udp.connect(server).await?;
    let udp = Arc::new(udp);

    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let media_flowing = Arc::new(AtomicBool::new(false));
    let counts = Arc::new(std::sync::Mutex::new(LinkCount::default()));

    // UDP hello until media flows, then a keepalive every second (NAT mappings).
    let hello = {
        let (udp, key, flowing) = (
            Arc::clone(&udp),
            Arc::clone(&key),
            Arc::clone(&media_flowing),
        );
        tokio::spawn(async move {
            let mut seq = 0u64;
            loop {
                seq += 1;
                let p = key.seal(Direction::ToServer, seq, &fec::hello_packet(&token));
                let _ = udp.send(&p).await;
                let wait = if flowing.load(Ordering::Relaxed) {
                    1000
                } else {
                    100
                };
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
        })
    };

    // Media.
    let media = {
        let (udp, key, sink, ctl) = (
            Arc::clone(&udp),
            Arc::clone(&key),
            sink.clone(),
            ctl_tx.clone(),
        );
        let flowing = Arc::clone(&media_flowing);
        let counts = Arc::clone(&counts);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let mut replay = ReplayWindow::default();
            // Monitor 0, and the others as their first packets come (version 6).
            let mut streams = vec![Stream::new(0)];
            // GLIDEDESK_DROP=PERCENT throws away that share of the packets, to test recovery.
            let drop: u64 = std::env::var("GLIDEDESK_DROP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut rng = 0x9e37_79b9_7f4a_7c15u64;
            let (mut got, mut lost, mut since) = (0u32, 0u32, Instant::now());
            // Time the frames took to arrive (first packet to last), for the statistics.
            let mut spread_total = Duration::ZERO;
            loop {
                let received = if streams.iter().all(|s| s.held.is_empty()) {
                    udp.recv(&mut buf).await
                } else {
                    match tokio::time::timeout(ACK_POLL, udp.recv(&mut buf)).await {
                        Ok(r) => r,
                        Err(_) => {
                            for s in &mut streams {
                                s.release_acks(&sink, &ctl);
                            }
                            continue;
                        }
                    }
                };
                let Ok(n) = received else {
                    continue;
                };
                let Some((seq, plain)) = key.open(Direction::ToClient, &buf[..n]) else {
                    continue;
                };
                if !replay.accept(seq) {
                    continue;
                }
                if drop > 0 {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    if rng % 100 < drop {
                        continue;
                    }
                }
                // Counted after the simulated losses, so GLIDEDESK_DROP shows in the reports.
                counts.lock().unwrap().packets.packet(seq);
                flowing.store(true, Ordering::Relaxed);
                let video = match fec::video_monitor(&plain) {
                    Some(m) => Some((m, None)),
                    // The server waits for acknowledgements: frames it sent that have not
                    // come (and are not still coming) are lost.
                    None => fec::parse_sync_monitor(&plain).map(|(m, last)| (m, Some(last))),
                };
                if let Some((monitor, sync)) = video {
                    let i = match streams.iter().position(|s| s.monitor == monitor) {
                        Some(i) => i,
                        None => {
                            streams.push(Stream::new(monitor));
                            streams.len() - 1
                        }
                    };
                    let stream = &mut streams[i];
                    let events = match sync {
                        None => stream.frames.push(&plain),
                        Some(last) => stream.frames.sync(last, SYNC_QUIET),
                    };
                    let (g, l) =
                        stream.handle(events, invalidation, &sink, &ctl, &mut spread_total);
                    got += g;
                    lost += l;
                    counts.lock().unwrap().frames_lost += l;
                } else if plain.first() == Some(&fec::AUDIO)
                    && let Some((_, pcm)) = fec::parse_audio(&plain)
                {
                    sink.lock().unwrap().audio(pcm);
                }
                for s in &mut streams {
                    s.release_acks(&sink, &ctl);
                }
                if since.elapsed() >= Duration::from_secs(5) {
                    info!(
                        frames = got,
                        lost,
                        arrival_ms = format!(
                            "{:.2}",
                            spread_total.as_secs_f64() * 1000.0 / f64::from(got.max(1))
                        ),
                        "Media"
                    );
                    (got, lost, since) = (0, 0, Instant::now());
                    spread_total = Duration::ZERO;
                }
            }
        })
    };

    // Server messages are read by a task of their own: `read_msg` is not cancel safe, and a
    // message cut by another branch of the select below would lose the stream's framing.
    let (srv_tx, mut srv_rx) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        loop {
            let m = glidedesk_proto::read_msg::<_, ServerMsg>(&mut rd).await;
            let end = m.is_err();
            if srv_tx.send(m).is_err() || end {
                break;
            }
        }
    });

    // Control: server messages in, client messages out, and once a second the link.
    let mut tick = tokio::time::interval(LINK_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut rtt: Option<Duration> = None;
    let mut filter = link::QualityFilter::default();
    let mut shown: Option<String> = None;
    // Files: ours as offered to the server, and the server's being fetched.
    let mut offered: Option<(u32, Arc<files::Offer>)> = None;
    let mut next_offer = 0u32;
    let mut download: Option<files::Download> = None;
    let result = loop {
        tokio::select! {
            _ = tick.tick() => {
                let (sample, frames_lost) = {
                    let mut c = counts.lock().unwrap();
                    (c.packets.take(), std::mem::take(&mut c.frames_lost))
                };
                if version >= 4
                    && let Some(s) = sample
                    && let Err(e) = glidedesk_proto::write_msg(
                        &mut wr,
                        &ClientMsg::LinkReport { received: s.received, lost: s.lost },
                    )
                    .await
                {
                    break Err(e.into());
                }
                if let Err(e) =
                    glidedesk_proto::write_msg(&mut wr, &ClientMsg::Ping { t_us: now_us() }).await
                {
                    break Err(e.into());
                }
                let stats = link::LinkStats {
                    loss: sample.map_or(0.0, |s| s.fraction()),
                    frames_lost,
                    rtt,
                };
                let quality = filter.update(stats.quality()).unwrap_or(filter.shown());
                let warning = if quality == link::Quality::Good {
                    None
                } else {
                    // The numbers of this second, under the level the filter keeps.
                    stats.warning().or_else(|| shown.clone())
                };
                if warning != shown {
                    if let Some(w) = &warning {
                        info!(warning = w.as_str(), "Link");
                    }
                    sink.lock().unwrap().link_warning(warning.as_deref());
                    shown = warning;
                }
            }
            msg = srv_rx.recv() => match msg.unwrap_or_else(|| {
                Err(glidedesk_proto::Error::Io(std::io::ErrorKind::UnexpectedEof.into()))
            }) {
                Ok(ServerMsg::Size { width, height }) => {
                    info!(width, height, "Desktop size");
                    sink.lock().unwrap().reset(u32::from(width), u32::from(height), codec);
                }
                Ok(ServerMsg::Layout(monitors)) => {
                    info!(?monitors, "Monitors");
                    sink.lock().unwrap().layout(&monitors);
                }
                Ok(ServerMsg::MonitorSize { monitor, width, height }) => {
                    info!(monitor, width, height, "Monitor size");
                    sink.lock()
                        .unwrap()
                        .reset_monitor(monitor, u32::from(width), u32::from(height), codec);
                }
                Ok(ServerMsg::Cursor(c)) => sink.lock().unwrap().cursor(&c),
                Ok(ServerMsg::Pong { t_us }) => {
                    let us = now_us().saturating_sub(t_us);
                    debug!(rtt_us = us, "Pong");
                    rtt = Some(Duration::from_micros(us));
                }
                Ok(ServerMsg::Clipboard(text)) => sink.lock().unwrap().clipboard(&text),
                Ok(ServerMsg::Denied(reason)) => break Err(anyhow::anyhow!(reason)),
                Ok(ServerMsg::Apps(names)) => {
                    info!(?names, "Apps on the server");
                    sink.lock().unwrap().apps(&names);
                }
                Ok(ServerMsg::AppStarted(name)) => info!(name, "App open"),
                Ok(ServerMsg::Files { id, entries }) => {
                    download = None;
                    let bytes: u64 = entries.iter().map(|e| e.size).sum();
                    if bytes > FILES_LIMIT {
                        warn!(bytes, "The server's copied files are too large to fetch");
                        continue;
                    }
                    match files::new_folder(&files_cache()).and_then(|dir| files::Download::new(&dir, id, entries)) {
                        Ok(d) => {
                            info!(bytes, "Fetching the server's copied files");
                            download = Some(d);
                            step_download(&mut download, &ctl_tx, sink);
                        }
                        Err(e) => warn!(%e, "Cannot take the server's files"),
                    }
                }
                Ok(ServerMsg::FileData(data)) => {
                    let Some(d) = download.as_mut().filter(|d| d.id == data.id) else { continue };
                    let r = match &data.error {
                        Some(e) => Err(std::io::Error::other(e.clone())),
                        None => d.on_data(data.index, data.offset, &data.data),
                    };
                    match r {
                        Ok(()) => step_download(&mut download, &ctl_tx, sink),
                        Err(e) => {
                            warn!(%e, "Fetching the server's files failed");
                            download = None;
                        }
                    }
                }
                Ok(ServerMsg::FileRead(r)) => {
                    let offer = offered.as_ref().filter(|(id, _)| *id == r.id).map(|(_, o)| Arc::clone(o));
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
                        let _ = ctl.send(ClientMsg::FileData(glidedesk_proto::FileData {
                            id: r.id, index: r.index, offset: r.offset, data, error,
                        }));
                    });
                }
                Ok(ServerMsg::AppExited { name, reason }) if opts.app.as_deref() == Some(name.as_str()) => {
                    break match reason {
                        None => {
                            info!(name, "The app ended");
                            Ok(())
                        }
                        Some(r) => Err(anyhow::anyhow!("{name}: {r}")),
                    };
                }
                Ok(other) => warn!(?other, "Unexpected message"),
                Err(e) => break Err(anyhow::Error::from(e).context("control channel")),
            },
            out = ctl_rx.recv() => {
                if let Some(m) = out
                    && let Err(e) = glidedesk_proto::write_msg(&mut wr, &m).await
                {
                    break Err(e.into());
                }
            }
            i = input.recv() => match i {
                Some(Input::Files(paths)) => {
                    if version < 4 {
                        debug!("This server takes no files on the clipboard");
                        continue;
                    }
                    let offer = match tokio::task::spawn_blocking(move || files::Offer::new(&paths)).await {
                        Ok(Ok(o)) => o,
                        Ok(Err(e)) => { warn!(%e, "Cannot offer the copied files"); continue }
                        Err(_) => continue,
                    };
                    next_offer = next_offer.wrapping_add(1);
                    let msg = ClientMsg::Files { id: next_offer, entries: offer.entries.clone() };
                    debug!(files = offer.entries.len(), bytes = offer.bytes(), "Offering copied files");
                    offered = Some((next_offer, Arc::new(offer)));
                    if let Err(e) = glidedesk_proto::write_msg(&mut wr, &msg).await {
                        break Err(e.into());
                    }
                }
                other => match other.and_then(input_event) {
                    Some(m) => {
                        if let Err(e) = glidedesk_proto::write_msg(&mut wr, &m).await {
                            break Err(e.into());
                        }
                    }
                    None => break Ok(()),
                },
            },
        }
    };
    hello.abort();
    media.abort();
    reader.abort();
    if result.is_ok() {
        // Leaving on purpose: close TLS properly so the server sees a clean end.
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut wr).await;
    }
    result.map(|()| fingerprint)
}

/// Copies larger than this are not fetched.
const FILES_LIMIT: u64 = 4 << 30;

/// Where the server's copied files are fetched to (one fresh folder per copy).
fn files_cache() -> std::path::PathBuf {
    std::env::temp_dir().join("glidedesk-clipboard")
}

/// Asks the server for the next range of its files; once they have all arrived, hands them
/// to the platform's clipboard.
fn step_download(
    download: &mut Option<files::Download>,
    ctl: &mpsc::UnboundedSender<ClientMsg>,
    sink: &SharedSink,
) {
    let Some(d) = download.as_mut() else { return };
    match d.next_request() {
        Ok(Some((index, offset, len))) => {
            let _ = ctl.send(ClientMsg::FileRead(glidedesk_proto::FileRead {
                id: d.id,
                index,
                offset,
                len,
            }));
        }
        Ok(None) if d.done() => {
            info!(bytes = d.bytes, "The server's files are here");
            sink.lock().unwrap().files(d.top_paths());
            *download = None;
        }
        Ok(None) => {}
        Err(e) => {
            warn!(%e, "Fetching the server's files failed");
            *download = None;
        }
    }
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn reads_the_newest_version_from_a_refusal() {
        assert_eq!(
            super::newest_version("protocol version 3, server speaks 1 to 2"),
            Some(2)
        );
        assert_eq!(super::newest_version("wrong user name or password"), None);
    }
}

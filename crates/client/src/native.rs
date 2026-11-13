//! Client of the native low-latency protocol (`glidedesk-proto`, ADR-0004).
//!
//! The control channel is TLS over TCP; video and audio arrive over UDP, sealed with the
//! session key the server sends at login. Video frames are rebuilt from their Reed-Solomon
//! shards and go to the same [`VideoSink`] as RDP's; a frame that cannot be rebuilt makes
//! the client ask for an IDR and skip frames until it arrives (decoding P-frames without
//! their reference would only show garbage).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use glidedesk_proto::crypto::{Direction, PacketKey, ReplayWindow};
use glidedesk_proto::{ClientMsg, Codec, InputEvent, ServerMsg, fec};
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
        Input::Disconnect => return None,
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

/// A server that only speaks protocol version 1 refused version 2 (with the fingerprint of
/// its certificate, already accepted for this connection).
#[derive(Debug)]
struct OldServer([u8; 32]);

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
    let r = session(&opts, &sink, &mut input, glidedesk_proto::VERSION).await;
    let old = match &r {
        Err(e) => e.downcast_ref::<OldServer>().map(|o| o.0),
        Ok(_) => None,
    };
    let Some(fingerprint) = old else {
        return r;
    };
    // glidedesk 0.3.0 and older: version 1, no frame invalidation. The certificate was
    // just accepted, so it is not asked about again.
    info!("Older server: protocol version 1, lost frames recovered by keyframes");
    opts.trust.pinned = Some(fingerprint);
    session(&opts, &sink, &mut input, glidedesk_proto::MIN_VERSION).await
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
            return Err(OldServer(fingerprint).into());
        }
        ServerMsg::Denied(reason) => bail!("the server refused the login: {reason}"),
        other => bail!("unexpected first message {other:?}"),
    };
    info!(?codec, width, height, "Logged in");
    sink.lock()
        .unwrap()
        .reset(u32::from(width), u32::from(height), codec);

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
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let mut replay = ReplayWindow::default();
            let mut frames = fec::Reassembler::default();
            // Until the first IDR, and after a lost frame until the server's recovery frame
            // (or an IDR), P-frames cannot be decoded. Either can be lost too: a keyframe is
            // asked for while waiting too long.
            let mut wait = Wait::Keyframe;
            let mut asked = Instant::now();
            // GLIDEDESK_DROP=PERCENT throws away that share of the packets, to test recovery.
            let drop: u64 = std::env::var("GLIDEDESK_DROP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut rng = 0x9e37_79b9_7f4a_7c15u64;
            let (mut got, mut lost, mut since) = (0u32, 0u32, Instant::now());
            // Received frames not acknowledged yet, oldest first.
            let mut held = std::collections::VecDeque::new();
            loop {
                let received = if held.is_empty() {
                    udp.recv(&mut buf).await
                } else {
                    match tokio::time::timeout(ACK_POLL, udp.recv(&mut buf)).await {
                        Ok(r) => r,
                        Err(_) => {
                            release_acks(&mut held, &sink, &ctl);
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
                flowing.store(true, Ordering::Relaxed);
                let events = match plain.first() {
                    Some(&fec::VIDEO) => frames.push(&plain),
                    // The server waits for acknowledgements: frames it sent that have not
                    // come (and are not still coming) are lost.
                    Some(&fec::SYNC) => match fec::parse_sync(&plain) {
                        Some(last) => frames.sync(last, SYNC_QUIET),
                        None => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                match plain.first() {
                    Some(&fec::VIDEO | &fec::SYNC) => {
                        for ev in events {
                            match ev {
                                fec::Event::Frame {
                                    frame_id,
                                    keyframe,
                                    recovery,
                                    data,
                                } => {
                                    got += 1;
                                    if keyframe || (recovery && wait == Wait::Recovery) {
                                        if wait != Wait::Nothing {
                                            debug!(frame_id, keyframe, "Recovered");
                                        }
                                        wait = Wait::Nothing;
                                    }
                                    if wait == Wait::Nothing {
                                        sink.lock().unwrap().frame(&data);
                                    } else if asked.elapsed() > KEYFRAME_RETRY {
                                        asked = Instant::now();
                                        wait = Wait::Keyframe;
                                        let _ = ctl.send(ClientMsg::RequestKeyframe);
                                    }
                                    held.push_back(frame_id);
                                }
                                fec::Event::Lost { frame_id } => {
                                    lost += 1;
                                    debug!(frame_id, "Frame lost");
                                    match wait {
                                        // Version 2: the server stops referencing it. (The
                                        // wait for recovery keeps its start: a keyframe is
                                        // still asked for if recovery takes too long.)
                                        Wait::Nothing | Wait::Recovery if invalidation => {
                                            if wait == Wait::Nothing {
                                                asked = Instant::now();
                                            }
                                            wait = Wait::Recovery;
                                            let _ = ctl.send(ClientMsg::InvalidateFrames {
                                                first: frame_id,
                                            });
                                        }
                                        Wait::Nothing => {
                                            wait = Wait::Keyframe;
                                            asked = Instant::now();
                                            let _ = ctl.send(ClientMsg::RequestKeyframe);
                                        }
                                        _ if asked.elapsed() > KEYFRAME_RETRY => {
                                            wait = Wait::Keyframe;
                                            asked = Instant::now();
                                            let _ = ctl.send(ClientMsg::RequestKeyframe);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    Some(&fec::AUDIO) => {
                        if let Some((_, pcm)) = fec::parse_audio(&plain) {
                            sink.lock().unwrap().audio(pcm);
                        }
                    }
                    _ => {}
                }
                release_acks(&mut held, &sink, &ctl);
                if since.elapsed() >= Duration::from_secs(5) {
                    info!(frames = got, lost, "Media");
                    (got, lost, since) = (0, 0, Instant::now());
                }
            }
        })
    };

    // Control: server messages in, client messages out.
    let result = loop {
        tokio::select! {
            msg = glidedesk_proto::read_msg::<_, ServerMsg>(&mut rd) => match msg {
                Ok(ServerMsg::Size { width, height }) => {
                    info!(width, height, "Desktop size");
                    sink.lock().unwrap().reset(u32::from(width), u32::from(height), codec);
                }
                Ok(ServerMsg::Cursor(c)) => sink.lock().unwrap().cursor(&c),
                Ok(ServerMsg::Pong { t_us }) => {
                    debug!(rtt_us = now_us().saturating_sub(t_us), "Pong");
                }
                Ok(ServerMsg::Clipboard(text)) => sink.lock().unwrap().clipboard(&text),
                Ok(ServerMsg::Denied(reason)) => break Err(anyhow::anyhow!(reason)),
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
            i = input.recv() => match i.and_then(input_event) {
                Some(m) => {
                    if let Err(e) = glidedesk_proto::write_msg(&mut wr, &m).await {
                        break Err(e.into());
                    }
                }
                None => break Ok(()),
            },
        }
    };
    hello.abort();
    media.abort();
    if result.is_ok() {
        // Leaving on purpose: close TLS properly so the server sees a clean end.
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut wr).await;
    }
    result.map(|()| fingerprint)
}

/// Acknowledges the received frames the decoder has caught up with. One may still be
/// decoding, so the server can already send the next one.
fn release_acks(
    held: &mut std::collections::VecDeque<u32>,
    sink: &SharedSink,
    ctl: &mpsc::UnboundedSender<ClientMsg>,
) {
    if held.is_empty() {
        return;
    }
    let waiting = sink.lock().unwrap().backlog().saturating_sub(1);
    while held.len() > waiting {
        let Some(frame_id) = held.pop_front() else {
            break;
        };
        let _ = ctl.send(ClientMsg::FrameAck { frame_id });
    }
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or_default()
}

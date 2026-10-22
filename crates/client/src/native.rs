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
    /// SHA-256 of the server certificate seen before; `None` accepts any (and logs it).
    pub pinned: Option<[u8; 32]>,
}

/// Accepts the server's self-signed certificate if it matches the pinned fingerprint (or
/// any, the first time), and remembers what it saw.
#[derive(Debug)]
struct Pin {
    pinned: Option<[u8; 32]>,
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
        use sha2::Digest;
        let fp: [u8; 32] = sha2::Sha256::digest(end_entity.as_ref()).into();
        *self.seen.lock().unwrap() = Some(fp);
        match self.pinned {
            Some(p) if p != fp => Err(rustls::Error::General(
                "the server certificate changed since the last connection".into(),
            )),
            _ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
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

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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
        Input::Disconnect => return None,
    })
}

/// Runs a session until the server closes it or `input` sends [`Input::Disconnect`].
/// Returns the SHA-256 fingerprint of the server certificate (to pin next time).
pub async fn run(
    opts: NativeOptions,
    sink: SharedSink,
    mut input: mpsc::UnboundedReceiver<Input>,
) -> Result<[u8; 32]> {
    let tcp = TcpStream::connect((opts.host.as_str(), opts.port))
        .await
        .with_context(|| format!("connecting to {}:{}", opts.host, opts.port))?;
    tcp.set_nodelay(true)?;
    let server: SocketAddr = tcp.peer_addr()?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let pin = Arc::new(Pin {
        pinned: opts.pinned,
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
    if opts.pinned.is_none() {
        info!(
            fingerprint = hex(&fingerprint),
            "Server certificate (first connection)"
        );
    }
    let (mut rd, mut wr) = tokio::io::split(tls);

    glidedesk_proto::write_msg(
        &mut wr,
        &ClientMsg::Hello {
            version: glidedesk_proto::VERSION,
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
            // Until the first IDR (and after a lost frame) P-frames cannot be decoded. The IDR
            // can be lost too: the request is repeated while waiting.
            let mut need_keyframe = true;
            let mut asked = Instant::now();
            // GLIDEDESK_DROP=PERCENT throws away that share of the packets, to test recovery.
            let drop: u64 = std::env::var("GLIDEDESK_DROP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut rng = 0x9e37_79b9_7f4a_7c15u64;
            let (mut got, mut lost, mut since) = (0u32, 0u32, Instant::now());
            loop {
                let Ok(n) = udp.recv(&mut buf).await else {
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
                match plain.first() {
                    Some(&fec::VIDEO) => {
                        for ev in frames.push(&plain) {
                            match ev {
                                fec::Event::Frame {
                                    frame_id,
                                    keyframe,
                                    data,
                                } => {
                                    got += 1;
                                    if keyframe {
                                        need_keyframe = false;
                                    }
                                    if !need_keyframe {
                                        sink.lock().unwrap().frame(&data);
                                    } else if asked.elapsed() > KEYFRAME_RETRY {
                                        asked = Instant::now();
                                        let _ = ctl.send(ClientMsg::RequestKeyframe);
                                    }
                                    let _ = ctl.send(ClientMsg::FrameAck { frame_id });
                                }
                                fec::Event::Lost { frame_id } => {
                                    lost += 1;
                                    debug!(frame_id, "Frame lost");
                                    if !need_keyframe || asked.elapsed() > KEYFRAME_RETRY {
                                        need_keyframe = true;
                                        asked = Instant::now();
                                        let _ = ctl.send(ClientMsg::RequestKeyframe);
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
                Ok(ServerMsg::Clipboard(_)) => {}
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
    result.map(|()| fingerprint)
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or_default()
}

//! Server of the phone's own screen over the native protocol (`glidedesk-proto`, ADR-0004):
//! the part of ADR-0006's Android server that runs in the app.
//!
//! Kotlin captures the screen (a display of the shell user through Shizuku or ADB, or
//! MediaProjection) and encodes it with MediaCodec; each encoded buffer comes here
//! ([`Server::frame`]) and leaves as Reed-Solomon protected, sealed UDP packets, as from the
//! desktop host, whose control channel this follows (`crates/host/src/native.rs`). What the
//! client asks for goes back to Kotlin as [`Event`]s: (re)start the encoder at a size, a
//! keyframe, a bitrate, and the input to inject, already as Android key codes and positions
//! in video pixels.
//!
//! One client at a time; a new login takes over. IPv4 only. No sound, clipboard or apps
//! yet: the client is told there is no audio, and clipboard and file messages are ignored.

pub mod auth;
pub mod keys;
pub mod video;

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use glidedesk_proto::crypto::{Direction, PacketKey};
use glidedesk_proto::{ClientMsg, Codec, InputEvent, ServerMsg, fec, link};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use keys::KeyState;
use video::{ParameterSets, RateControl};

pub struct Config {
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Where the TLS identity is kept (the app's private files).
    pub state_dir: PathBuf,
    /// The phone can encode HEVC.
    pub hevc: bool,
    /// The client gets a display of its own size instead of a copy of the phone's screen.
    pub own_display: bool,
    /// Longest side of the video.
    pub max_size: u32,
    pub max_bitrate: u32,
    /// The phone's screen as shown (width and height swap with the rotation).
    pub display: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerAction {
    /// A finger goes down (the left button).
    Down = 0,
    Move = 1,
    Up = 2,
    /// The mouse moves with no button down.
    Hover = 3,
}

/// What Kotlin has to do. Positions are in video pixels (the size of the last `Start`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// (Re)start the capture and the encoder at this size.
    Start {
        codec: Codec,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
    },
    /// No client any more: stop the capture and the encoder.
    Stop,
    Keyframe,
    Bitrate(u32),
    Pointer {
        action: PointerAction,
        x: u32,
        y: u32,
    },
    /// 120 units per notch, positive up and right.
    Scroll {
        x: u32,
        y: u32,
        horizontal: i32,
        vertical: i32,
    },
    /// An Android key code, with the meta state for its `KeyEvent`.
    Key { keycode: i32, down: bool, meta: i32 },
    Text(char),
}

impl Event {
    /// The event as Kotlin reads it (`NativeServer.kt`): a kind, then its fields.
    pub fn to_ints(&self) -> [i32; 7] {
        let n = |v: u32| v.min(i32::MAX as u32) as i32;
        match *self {
            Event::Start {
                codec,
                width,
                height,
                bitrate,
                fps,
            } => [
                1,
                i32::from(codec == Codec::Hevc),
                n(width),
                n(height),
                n(bitrate),
                n(fps),
                0,
            ],
            Event::Stop => [2, 0, 0, 0, 0, 0, 0],
            Event::Keyframe => [3, 0, 0, 0, 0, 0, 0],
            Event::Bitrate(bps) => [4, n(bps), 0, 0, 0, 0, 0],
            Event::Pointer { action, x, y } => [5, action as i32, n(x), n(y), 0, 0, 0],
            Event::Scroll {
                x,
                y,
                horizontal,
                vertical,
            } => [6, n(x), n(y), horizontal, vertical, 0, 0],
            Event::Key {
                keycode,
                down,
                meta,
            } => [7, keycode, i32::from(down), meta, 0, 0, 0],
            Event::Text(ch) => [8, ch as i32, 0, 0, 0, 0, 0],
        }
    }
}

/// Mouse buttons other than the left one, as keys (scrcpy's default: right is Back, middle
/// is Home, the side buttons the app switcher and the notifications).
fn button_key(button: u8) -> Option<i32> {
    match button {
        2 => Some(keys::code::BACK),
        3 => Some(keys::code::HOME),
        4 => Some(keys::code::APP_SWITCH),
        5 => Some(keys::code::NOTIFICATION),
        _ => None,
    }
}

/// Events waiting for Kotlin at most; past this the oldest go (Kotlin is not reading).
const EVENTS_KEPT: usize = 4096;
/// A frame not acknowledged after this long may be lost: the client is told which frame was
/// the last (so it can ask for a keyframe instead of waiting for packets that will not come).
const SYNC_AFTER: Duration = Duration::from_millis(20);
/// SYNC packets stop this long after the last frame (a still screen sends no frames).
const SYNC_FOR: Duration = Duration::from_millis(500);
/// Bitrate changes at most this often.
const RATE_PERIOD: Duration = Duration::from_secs(1);

/// The current client, as the encoder thread and the UDP thread see it.
struct Link {
    key: PacketKey,
    token: [u8; 16],
    /// Where its UDP hello came from; no video before it.
    peer: Option<SocketAddr>,
    control: mpsc::UnboundedSender<ServerMsg>,
    seq: u64,
    frame_id: u32,
    /// The size the client was last told about.
    told: (u32, u32),
    /// The size the client asked for.
    client: (u32, u32),
    codec: Codec,
    fps: u32,
    last_frame: Option<Instant>,
    last_sync: Option<Instant>,
}

impl Link {
    fn seal_and_send(&mut self, udp: &UdpSocket, peer: SocketAddr, plain: &[u8]) {
        self.seq += 1;
        let packet = self.key.seal(Direction::ToClient, self.seq, plain);
        if let Err(e) = udp.send_to(&packet, peer) {
            debug!(%e, "UDP send failed");
        }
    }
}

struct Shared {
    user: String,
    password: String,
    hevc: bool,
    own_display: bool,
    max_bitrate: u32,
    udp: UdpSocket,
    link: Mutex<Option<Link>>,
    /// Newest frame the client acknowledged.
    acked: AtomicU32,
    fec: Mutex<link::FecControl>,
    rate: Mutex<RateControl>,
    params: Mutex<ParameterSets>,
    /// The size of the last `Start`.
    size: Mutex<Option<(u32, u32)>>,
    display: Mutex<(u32, u32)>,
    max_size: AtomicU32,
    /// The user's limit, which each new client starts from again.
    max_size_set: u32,
    events: Mutex<VecDeque<Event>>,
    ready: Condvar,
    status: Mutex<String>,
    stop: AtomicBool,
    limiter: auth::Limiter,
}

const WAITING: &str = "Waiting for a client";

impl Shared {
    fn push(&self, ev: Event) {
        let mut q = self.events.lock().unwrap();
        if q.len() >= EVENTS_KEPT {
            q.pop_front();
        }
        q.push_back(ev);
        self.ready.notify_one();
    }

    fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
    }

    fn video_size(&self, client: (u32, u32)) -> (u32, u32) {
        video::video_size(
            *self.display.lock().unwrap(),
            client,
            self.max_size.load(Ordering::Relaxed),
            self.own_display,
        )
    }

    fn start_encoder(&self, size: (u32, u32), codec: Codec, fps: u32) {
        *self.size.lock().unwrap() = Some(size);
        // The new encoder sends its own parameter sets first.
        self.params.lock().unwrap().clear();
        let bitrate = self.rate.lock().unwrap().bps();
        info!(?codec, w = size.0, h = size.1, bitrate, fps, "Starting the encoder");
        self.push(Event::Start {
            codec,
            width: size.0,
            height: size.1,
            bitrate,
            fps,
        });
    }

    /// The size follows the client's window, the phone's rotation and the encoder's limits:
    /// a different one restarts the encoder.
    fn update_size(&self) {
        let (client, codec, fps) = {
            let guard = self.link.lock().unwrap();
            let Some(l) = guard.as_ref() else { return };
            (l.client, l.codec, l.fps)
        };
        let size = self.video_size(client);
        if *self.size.lock().unwrap() != Some(size) {
            self.start_encoder(size, codec, fps);
        }
    }

    fn is_current(&self, token: [u8; 16]) -> bool {
        self.link
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|l| l.token == token)
    }

    /// The video size the client knows, for clamping its positions.
    fn told(&self) -> (u32, u32) {
        self.link
            .lock()
            .unwrap()
            .as_ref()
            .map_or((1, 1), |l| l.told)
    }

    fn input(&self, st: &mut InputState, ev: InputEvent) {
        match ev {
            InputEvent::MouseMove { x, y } => {
                let (w, h) = self.told();
                st.pos = (u32::from(x).min(w - 1), u32::from(y).min(h - 1));
                let action = if st.touching {
                    PointerAction::Move
                } else {
                    PointerAction::Hover
                };
                self.push(Event::Pointer {
                    action,
                    x: st.pos.0,
                    y: st.pos.1,
                });
            }
            InputEvent::MouseButton { button: 1, down } => {
                if down != st.touching {
                    st.touching = down;
                    let action = if down {
                        PointerAction::Down
                    } else {
                        PointerAction::Up
                    };
                    self.push(Event::Pointer {
                        action,
                        x: st.pos.0,
                        y: st.pos.1,
                    });
                }
            }
            InputEvent::MouseButton { button, down } => {
                if let Some(keycode) = button_key(button) {
                    let meta = st.keys.meta();
                    self.push(Event::Key {
                        keycode,
                        down,
                        meta,
                    });
                }
            }
            InputEvent::Wheel { vertical, units } => {
                let units = i32::from(units);
                self.push(Event::Scroll {
                    x: st.pos.0,
                    y: st.pos.1,
                    horizontal: if vertical { 0 } else { units },
                    vertical: if vertical { units } else { 0 },
                });
            }
            InputEvent::Key {
                scancode,
                extended,
                down,
            } => {
                if let Some(keycode) = keys::keycode(scancode, extended) {
                    let meta = st.keys.key(keycode, down);
                    self.push(Event::Key {
                        keycode,
                        down,
                        meta,
                    });
                }
            }
            InputEvent::Unicode { ch, down } => {
                if down {
                    self.push(Event::Text(ch));
                }
            }
            InputEvent::ReleaseAll => self.release_input(st),
        }
    }

    /// Lifts the finger and the keys the client left down.
    fn release_input(&self, st: &mut InputState) {
        if st.touching {
            st.touching = false;
            self.push(Event::Pointer {
                action: PointerAction::Up,
                x: st.pos.0,
                y: st.pos.1,
            });
        }
        for keycode in st.keys.release_all() {
            self.push(Event::Key {
                keycode,
                down: false,
                meta: st.keys.meta(),
            });
        }
    }
}

/// One client's input as it arrives.
#[derive(Default)]
struct InputState {
    pos: (u32, u32),
    touching: bool,
    keys: KeyState,
}

pub struct Server {
    shared: Arc<Shared>,
    fingerprint: [u8; 32],
    runtime: Option<tokio::runtime::Runtime>,
    udp_thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Binds TCP and UDP `config.port` and starts listening.
    pub fn start(config: Config) -> Result<Self> {
        let (tls, fingerprint) = auth::identity(&config.state_dir)?;
        let bind = SocketAddr::from(([0, 0, 0, 0], config.port));
        let tcp = std::net::TcpListener::bind(bind).with_context(|| format!("TCP port {bind}"))?;
        tcp.set_nonblocking(true)?;
        let udp = {
            let s = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            // A keyframe leaves as a burst of hundreds of packets.
            let _ = s.set_send_buffer_size(4 << 20);
            s.bind(&bind.into())
                .with_context(|| format!("UDP port {bind}"))?;
            UdpSocket::from(s)
        };
        // The UDP thread looks at the stop flag between reads.
        udp.set_read_timeout(Some(Duration::from_millis(250)))?;
        let now = Instant::now();
        let shared = Arc::new(Shared {
            user: config.user,
            password: config.password,
            hevc: config.hevc,
            own_display: config.own_display,
            max_bitrate: config.max_bitrate,
            udp,
            link: Mutex::new(None),
            acked: AtomicU32::new(0),
            fec: Mutex::default(),
            rate: Mutex::new(RateControl::new(config.max_bitrate, now)),
            params: Mutex::default(),
            size: Mutex::new(None),
            display: Mutex::new(config.display),
            max_size: AtomicU32::new(config.max_size.max(video::ALIGN)),
            max_size_set: config.max_size.max(video::ALIGN),
            events: Mutex::default(),
            ready: Condvar::new(),
            status: Mutex::new(WAITING.to_owned()),
            stop: AtomicBool::new(false),
            limiter: auth::Limiter::default(),
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("glidedesk-server")
            .enable_all()
            .build()?;
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        runtime.spawn(accept_loop(Arc::clone(&shared), tcp, acceptor));
        runtime.spawn(housekeeping(Arc::clone(&shared)));
        let udp_thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("glidedesk-udp".into())
                .spawn(move || udp_loop(&shared))?
        };
        info!(%bind, "Listening for native clients");
        Ok(Self {
            shared,
            fingerprint,
            runtime: Some(runtime),
            udp_thread: Some(udp_thread),
        })
    }

    /// SHA-256 of the server certificate, as clients show it.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub fn status(&self) -> String {
        self.shared.status.lock().unwrap().clone()
    }

    /// The next thing for Kotlin to do, waiting up to `timeout` for one.
    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        let q = self.shared.events.lock().unwrap();
        let (mut q, _) = self
            .shared
            .ready
            .wait_timeout_while(q, timeout, |q| {
                q.is_empty() && !self.shared.stop.load(Ordering::Relaxed)
            })
            .unwrap();
        q.pop_front()
    }

    /// The phone's screen changed size (it rotated).
    pub fn display(&self, width: u32, height: u32) {
        *self.shared.display.lock().unwrap() = (width.max(1), height.max(1));
        self.shared.update_size();
    }

    /// The encoder refused the size of the last `Start`: try the next smaller one.
    pub fn encoder_failed(&self) {
        let Some(size) = *self.shared.size.lock().unwrap() else {
            // The client left meanwhile: the next one starts afresh.
            return;
        };
        match video::smaller(size) {
            Some(long) => {
                warn!(?size, long, "The encoder refused the size; trying a smaller one");
                self.shared.max_size.store(long, Ordering::Relaxed);
                self.shared.update_size();
            }
            None => {
                warn!(?size, "The encoder refused every size");
                self.shared
                    .set_status("The video encoder failed (see the log)");
                self.shared.push(Event::Stop);
            }
        }
    }

    /// One encoder output buffer (`flags`: MediaCodec's `BUFFER_FLAG_*`) of a `width` ×
    /// `height` video. Sent to the client right away, paced to its bitrate.
    pub fn frame(&self, data: &[u8], flags: i32, width: u32, height: u32) {
        let au = self.shared.params.lock().unwrap().frame(data, flags);
        let Some(au) = au else { return };
        let keyframe = flags & video::FLAG_KEY_FRAME != 0;
        let start = Instant::now();
        // Sealed under the lock, sent after it: the pacing below sleeps, and the input,
        // the UDP hello and the main thread (rotation) take the same lock.
        let (peer, sealed, bps) = {
            let mut guard = self.shared.link.lock().unwrap();
            let Some(link) = guard.as_mut() else { return };
            let Some(peer) = link.peer else { return };
            if link.told != (width, height) {
                link.told = (width, height);
                let _ = link.control.send(ServerMsg::Size {
                    width: width.min(u32::from(u16::MAX)) as u16,
                    height: height.min(u32::from(u16::MAX)) as u16,
                });
            }
            link.frame_id = link.frame_id.wrapping_add(1);
            let bps = {
                let mut rate = self.shared.rate.lock().unwrap();
                rate.sent(link.frame_id, start);
                rate.bps()
            };
            let parity = self.shared.fec.lock().unwrap().percent();
            let packets = fec::packetize_frame(link.frame_id, keyframe, false, &au, parity);
            let sealed: Vec<Vec<u8>> = packets
                .iter()
                .map(|p| {
                    link.seq += 1;
                    link.key.seal(Direction::ToClient, link.seq, p)
                })
                .collect();
            link.last_frame = Some(start);
            (peer, sealed, bps)
        };
        // Pacing as the host's: past a first burst, a few times the bitrate, so a keyframe
        // does not overflow the queue of a Wi-Fi access point.
        let (pace_bps, burst) = link::pacing(bps, fec::SHARD);
        for (i, packet) in sealed.iter().enumerate() {
            if let Err(e) = self.shared.udp.send_to(packet, peer) {
                debug!(%e, "UDP send failed");
            }
            if i >= burst && i % 8 == 0 {
                let due =
                    Duration::from_secs_f64(((i - burst) * fec::SHARD * 8) as f64 / pace_bps);
                if let Some(wait) = due.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.ready.notify_all();
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(1));
        }
        if let Some(t) = self.udp_thread.take() {
            let _ = t.join();
        }
    }
}

/// UDP from clients: the hello that tells the server where to send.
fn udp_loop(shared: &Shared) {
    let mut buf = vec![0u8; 2048];
    while !shared.stop.load(Ordering::Relaxed) {
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
            // Frames before this went nowhere: the client needs a keyframe to start.
            if first {
                shared.push(Event::Keyframe);
            }
        }
    }
}

/// Every few milliseconds: SYNC packets while the last frames are unacknowledged, and once a
/// second the bitrate.
async fn housekeeping(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_rate = Instant::now();
    loop {
        tick.tick().await;
        let now = Instant::now();
        let connected = {
            let mut guard = shared.link.lock().unwrap();
            if let Some(link) = guard.as_mut()
                && let (Some(peer), Some(last)) = (link.peer, link.last_frame)
            {
                let since = now.saturating_duration_since(last);
                if shared.acked.load(Ordering::SeqCst) != link.frame_id
                    && (SYNC_AFTER..SYNC_FOR).contains(&since)
                    && link.last_sync.is_none_or(|t| now.saturating_duration_since(t) >= SYNC_AFTER)
                {
                    link.last_sync = Some(now);
                    let last_id = link.frame_id;
                    link.seal_and_send(&shared.udp, peer, &fec::sync_packet(last_id));
                }
            }
            guard.is_some()
        };
        if now.saturating_duration_since(last_rate) >= RATE_PERIOD {
            last_rate = now;
            if connected && let Some(bps) = shared.rate.lock().unwrap().tick(now) {
                debug!(bps, "Bitrate");
                shared.push(Event::Bitrate(bps));
            }
        }
    }
}

async fn accept_loop(
    shared: Arc<Shared>,
    tcp: std::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
) {
    let listener = match tokio::net::TcpListener::from_std(tcp) {
        Ok(l) => l,
        Err(e) => {
            shared.set_status(format!("Cannot listen: {e}"));
            return;
        }
    };
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                warn!(%e, "Accepting a connection failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let (acceptor, shared) = (acceptor.clone(), Arc::clone(&shared));
        tokio::spawn(async move {
            info!(%peer, "Native client connecting");
            let r = async {
                let tls = acceptor.accept(tcp).await.context("TLS handshake")?;
                connection(tls, peer, &shared).await
            }
            .await;
            match r {
                Ok(()) => info!(%peer, "Native client disconnected"),
                Err(e) => {
                    let e = format!("{e:#}");
                    info!(%peer, error = e, "Native client disconnected");
                    // Shown in the app unless another client is on.
                    if shared.link.lock().unwrap().is_none() {
                        shared.set_status(format!("{peer}: {e}"));
                    }
                }
            }
        });
    }
}

async fn connection<S>(stream: S, peer: SocketAddr, shared: &Arc<Shared>) -> Result<()>
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
    let now = Instant::now();
    let deny = if let Some(wait) = shared.limiter.check(ip, now) {
        Some(format!(
            "too many failed attempts; try again in {:.0}s",
            wait.as_secs_f64().max(1.0)
        ))
    } else if !(glidedesk_proto::MIN_VERSION..=glidedesk_proto::VERSION).contains(&version) {
        Some(format!(
            "protocol version {version}, server speaks {} to {}",
            glidedesk_proto::MIN_VERSION,
            glidedesk_proto::VERSION
        ))
    } else if !auth::credentials_match(&shared.user, &shared.password, &user, &password) {
        shared.limiter.failure(ip, now);
        warn!(%peer, "Authentication failed");
        Some("wrong user name or password".to_owned())
    } else {
        shared.limiter.success(ip);
        codecs
            .iter()
            .all(|c| *c == Codec::Hevc && !shared.hevc)
            .then(|| "the phone cannot encode any codec the client decodes".to_owned())
    };
    if let Some(reason) = deny {
        glidedesk_proto::write_msg(&mut wr, &ServerMsg::Denied(reason.clone())).await?;
        bail!("denied: {reason}");
    }
    let codec = codecs
        .iter()
        .copied()
        .find(|c| *c == Codec::H264 || shared.hevc)
        .context("no codec in common")?;
    let client = (u32::from(width), u32::from(height));
    // A smaller size forced by an encoder failure lasts for one client.
    shared
        .max_size
        .store(shared.max_size_set, Ordering::Relaxed);
    let size = shared.video_size(client);
    let fps = refresh.clamp(30, 120);
    info!(%peer, user, w = size.0, h = size.1, refresh, ?codec, version, "Native client logged in");

    let key: [u8; 32] = glidedesk_proto::random();
    let token: [u8; 16] = glidedesk_proto::random();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
    // This client replaces any previous one, which is told why.
    let previous = shared.link.lock().unwrap().replace(Link {
        key: PacketKey::new(&key),
        token,
        peer: None,
        control: ctl_tx.clone(),
        seq: 0,
        frame_id: 0,
        told: size,
        client,
        codec,
        fps,
        last_frame: None,
        last_sync: None,
    });
    if let Some(old) = previous {
        let _ = old.control.send(ServerMsg::Denied(format!(
            "another client ({peer}) took over the session"
        )));
    }
    shared.acked.store(0, Ordering::SeqCst);
    *shared.fec.lock().unwrap() = link::FecControl::default();
    *shared.rate.lock().unwrap() = RateControl::new(shared.max_bitrate, now);
    glidedesk_proto::write_msg(
        &mut wr,
        &ServerMsg::Welcome {
            key,
            token,
            codec,
            width: size.0 as u16,
            height: size.1 as u16,
            audio: false,
        },
    )
    .await?;
    // Always a fresh encoder: its first frame is the keyframe this client starts from.
    shared.start_encoder(size, codec, fps);
    shared.set_status(format!("{peer} connected ({codec:?}, {}×{})", size.0, size.1));

    let writer = tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            if glidedesk_proto::write_msg(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    let mut input = InputState::default();
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
        if !shared.is_current(token) {
            break Ok(());
        }
        match msg {
            ClientMsg::Input(ev) => shared.input(&mut input, ev),
            ClientMsg::Resize { width, height } => {
                if let Some(l) = shared.link.lock().unwrap().as_mut() {
                    l.client = (u32::from(width), u32::from(height));
                }
                shared.update_size();
            }
            ClientMsg::FrameAck { frame_id } => {
                shared.acked.store(frame_id, Ordering::SeqCst);
                shared.rate.lock().unwrap().acked(frame_id, Instant::now());
            }
            // MediaCodec cannot drop references: both get a keyframe.
            ClientMsg::RequestKeyframe | ClientMsg::InvalidateFrames { .. } => {
                shared.push(Event::Keyframe);
            }
            ClientMsg::Ping { t_us } => {
                let _ = ctl_tx.send(ServerMsg::Pong { t_us });
            }
            ClientMsg::LinkReport { received, lost } => {
                let sample = link::LossSample { received, lost };
                if let Some(percent) = shared.fec.lock().unwrap().report(sample) {
                    debug!(percent, loss = sample.fraction(), "Video parity");
                }
                shared.rate.lock().unwrap().loss(sample.fraction());
            }
            ClientMsg::OpenApp(name) => {
                let _ = ctl_tx.send(ServerMsg::AppExited {
                    name,
                    reason: Some("a phone server shares its screen, not apps".to_owned()),
                });
            }
            // Nothing is ever offered, so a read can only fail.
            ClientMsg::FileRead(r) => {
                let _ = ctl_tx.send(ServerMsg::FileData(glidedesk_proto::FileData {
                    id: r.id,
                    index: r.index,
                    offset: r.offset,
                    data: Vec::new(),
                    error: Some("no files are copied".to_owned()),
                }));
            }
            ClientMsg::Clipboard(_) | ClientMsg::Files { .. } | ClientMsg::FileData(_) => {}
            ClientMsg::Hello { .. } => warn!("Second Hello ignored"),
            // One screen: a client that asks for several monitors (version 6) gets it alone.
            ClientMsg::Monitors(_) => {
                let told = shared.link.lock().unwrap().as_ref().map(|l| l.told);
                if let Some((w, h)) = told {
                    let _ = ctl_tx.send(ServerMsg::Layout(vec![glidedesk_proto::Monitor {
                        id: 0,
                        x: 0,
                        y: 0,
                        width: w.min(u32::from(u16::MAX)) as u16,
                        height: h.min(u32::from(u16::MAX)) as u16,
                        refresh: 60,
                        primary: true,
                    }]));
                }
            }
            // MediaCodec here encodes 4:2:0 only; no monitor but the first.
            ClientMsg::Formats { .. } | ClientMsg::MonitorVideo { .. } => {}
        }
    };
    shared.release_input(&mut input);
    let ended = {
        let mut link = shared.link.lock().unwrap();
        let ours = link.as_ref().is_some_and(|l| l.token == token);
        if ours {
            *link = None;
        }
        ours
    };
    if ended {
        *shared.size.lock().unwrap() = None;
        shared.push(Event::Stop);
        shared.set_status(WAITING);
    }
    writer.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_as_ints() {
        let start = Event::Start {
            codec: Codec::Hevc,
            width: 1080,
            height: 2400,
            bitrate: 8_000_000,
            fps: 60,
        };
        assert_eq!(start.to_ints(), [1, 1, 1080, 2400, 8_000_000, 60, 0]);
        let pointer = Event::Pointer {
            action: PointerAction::Hover,
            x: 5,
            y: 6,
        };
        assert_eq!(pointer.to_ints(), [5, 3, 5, 6, 0, 0, 0]);
        let key = Event::Key {
            keycode: 29,
            down: true,
            meta: 1,
        };
        assert_eq!(key.to_ints(), [7, 29, 1, 1, 0, 0, 0]);
        assert_eq!(Event::Text('é').to_ints()[1], 0xe9);
        assert_eq!(Event::Bitrate(u32::MAX).to_ints()[1], i32::MAX);
    }

    #[test]
    fn buttons() {
        assert_eq!(button_key(2), Some(keys::code::BACK));
        assert_eq!(button_key(3), Some(keys::code::HOME));
        assert_eq!(button_key(9), None);
    }
}

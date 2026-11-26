//! A Moonlight stream: what `/launch` set up, the settings the RTSP handshake agrees on, and
//! the video itself (RTP over UDP, as Sunshine sends it).
//!
//! Video packets (Sunshine `src/stream.cpp` `videoBroadcastThread`, moonlight-common-c
//! `Video.h` and `RtpVideoQueue.c` at the commits in THIRD_PARTY.md): each frame gets an
//! 8-byte frame header and is cut into packets of the size the client asked for. Every packet
//! carries an RTP header (12 bytes, with the extension bit), 4 reserved bytes and the
//! GameStream video header (16 bytes), then its part of the frame; the last one is padded with
//! zeros. The packets of a frame form up to 4 FEC blocks of at most 255 packets, each followed
//! by Reed-Solomon parity packets ([`super::fec`], 20% by default as in Sunshine) from which
//! the client rebuilds lost ones.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::encoder::{Codec, EncodedFrame};
use crate::pipeline::{self, Mode, Output, Prepared};

/// The ports of a GameStream host, from its HTTP port (Sunshine `net::map_port`): RTSP 48010,
/// video 47998, control 47999 and audio 48000 for the usual 47989.
#[derive(Clone, Copy, Debug)]
pub struct Ports {
    pub rtsp: u16,
    pub video: u16,
    pub control: u16,
    pub audio: u16,
}

impl Ports {
    pub fn from_http(http: u16) -> Self {
        Self {
            rtsp: http + 21,
            video: http + 9,
            control: http + 10,
            audio: http + 11,
        }
    }
}

/// What `/launch` (or `/resume`) set up for one client.
#[derive(Clone, Debug)]
pub struct Launch {
    /// AES-128 key for the control stream and audio (`rikey`).
    pub key: [u8; 16],
    /// `rikeyid`: the start of the audio IVs.
    pub key_id: u32,
    /// The app the client opened (its `/applist` id and name).
    pub app_id: u32,
    pub app_name: String,
    /// Sent back in the RTSP `SETUP` answers; the client puts it in its UDP pings so the
    /// host knows where to send video and audio (16 hex digits).
    pub ping_payload: [u8; 16],
    /// Sent back too; the client puts it in its ENet connect.
    pub connect_data: u32,
    /// Only this address may set the stream up.
    pub client: IpAddr,
}

impl Launch {
    /// The screen mode the client asks for here comes again in `ANNOUNCE`, which is what
    /// counts ([`Settings`]).
    pub fn new(key: [u8; 16], key_id: u32, client: IpAddr, app_id: u32, app_name: &str) -> Self {
        let raw: [u8; 8] = glidedesk_proto::random();
        let mut ping_payload = [0u8; 16];
        for (i, b) in raw.iter().enumerate() {
            ping_payload[i * 2..i * 2 + 2].copy_from_slice(format!("{b:02X}").as_bytes());
        }
        Self {
            key,
            key_id,
            app_id,
            app_name: app_name.to_owned(),
            ping_payload,
            connect_data: u32::from_le_bytes(glidedesk_proto::random()),
            client,
        }
    }
}

/// What the client asked for in its RTSP `ANNOUNCE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Bytes of video payload per packet, after the RTP header (`x-nv-video[0].packetSize`).
    pub packet_size: usize,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    /// The client decrypts audio (Moonlight always asks).
    pub audio_encrypted: bool,
    /// Milliseconds of sound per audio packet.
    pub audio_ms: u32,
    pub audio_channels: u32,
    /// Fewest parity packets per FEC block (`x-nv-vqos[0].fec.minRequiredFecPackets`).
    pub min_parity: usize,
}

impl Settings {
    /// From the `a=name:value` lines of the SDP.
    pub fn from_sdp(sdp: &str) -> Result<Self> {
        let get = |name: &str| -> Option<i64> {
            sdp.lines().find_map(|l| {
                let (k, v) = l.trim().strip_prefix("a=")?.split_once(':')?;
                (k == name).then(|| v.trim().parse().ok()).flatten()
            })
        };
        let need =
            |name: &str| get(name).with_context(|| format!("the client did not send {name}"));
        let packet_size = need("x-nv-video[0].packetSize")?;
        if !(256..=8192).contains(&packet_size) {
            bail!("packet size {packet_size} is out of range");
        }
        // Moonlight sends what the user picked as configuredBitrateKbps; maximumBitrateKbps is
        // that less 20% for FEC.
        let bitrate = get("x-ml-video.configuredBitrateKbps")
            .filter(|&b| b > 0)
            .or_else(|| get("x-nv-vqos[0].bw.maximumBitrateKbps"))
            .unwrap_or(20_000);
        let codec = match get("x-nv-vqos[0].bitStreamFormat").unwrap_or(0) {
            0 => Codec::H264,
            1 => Codec::Hevc,
            other => bail!("the client asked for a codec this host does not send ({other})"),
        };
        // Audio encryption: the legacy feature flag (0x20) or Sunshine's SS_ENC_AUDIO (0x04).
        let audio_encrypted = get("x-nv-general.featureFlags").unwrap_or(0) & 0x20 != 0
            || get("x-ss-general.encryptionEnabled").unwrap_or(0) & 0x04 != 0;
        let audio_ms = match get("x-nv-aqos.packetDuration") {
            Some(10) => 10,
            Some(20) => 20,
            _ => 5,
        };
        let positive =
            |name: &str| -> Result<u32> { Ok(u32::try_from(need(name)?).unwrap_or(0).max(1)) };
        Ok(Self {
            width: positive("x-nv-video[0].clientViewportWd")?,
            height: positive("x-nv-video[0].clientViewportHt")?,
            fps: positive("x-nv-video[0].maxFPS")?,
            packet_size: packet_size as usize,
            bitrate_kbps: u32::try_from(bitrate).unwrap_or(20_000).max(500),
            codec,
            audio_encrypted,
            audio_ms,
            audio_channels: u32::try_from(get("x-nv-audio.surround.numChannels").unwrap_or(2))
                .unwrap_or(2),
            min_parity: usize::try_from(get("x-nv-vqos[0].fec.minRequiredFecPackets").unwrap_or(0))
                .unwrap_or(0)
                .min(8),
        })
    }
}

/// The video connection to one client, from its `ANNOUNCE` until it goes away.
struct Video {
    settings: Settings,
    ping_payload: [u8; 16],
    client: IpAddr,
    /// Where the client's pings come from (learned from the first one).
    peer: Option<SocketAddr>,
    /// GameStream frame number of the last frame sent (the first is 1).
    frame_index: u32,
    /// RTP sequence number of the next packet.
    seq: u16,
    epoch: Instant,
    /// Size of the last frame sent; a change needs a keyframe.
    size: Option<(u32, u32)>,
    /// Reused packet buffer.
    buf: Vec<u8>,
    /// Frame numbers sent lately and the encoder's picture index of each, for invalidation
    /// requests.
    sent: std::collections::VecDeque<(u32, u64)>,
}

/// Frames remembered for invalidation (a loss further back needs a keyframe).
const SENT_HISTORY: usize = 128;

/// Streaming state shared by `/launch`, RTSP, the control stream and the pipeline.
pub struct Stream {
    session: Arc<pipeline::Shared>,
    index: usize,
    pub ports: Ports,
    pub hevc: bool,
    launch: Mutex<Option<Launch>>,
    video: Mutex<Option<Video>>,
    socket: UdpSocket,
    pub(super) audio: Mutex<Option<super::audio::Target>>,
    pub(super) audio_socket: UdpSocket,
    keyframe: AtomicBool,
    /// Encoder picture index of the oldest frame the client lost and asked to invalidate.
    lost: Mutex<Option<u64>>,
    bitrate: AtomicU32,
    sent: AtomicU64,
}

impl Stream {
    pub fn launch(&self) -> Option<Launch> {
        self.launch.lock().unwrap().clone()
    }

    /// A new `/launch` or `/resume`: any stream of an earlier one ends.
    pub fn set_launch(&self, launch: Launch) {
        *self.video.lock().unwrap() = None;
        *self.audio.lock().unwrap() = None;
        *self.launch.lock().unwrap() = Some(launch);
    }

    /// `/cancel`: the client quit the app.
    pub fn clear_launch(&self) {
        *self.video.lock().unwrap() = None;
        *self.audio.lock().unwrap() = None;
        *self.launch.lock().unwrap() = None;
    }

    /// The RTSP `ANNOUNCE` came: video starts once the client's first ping shows where to send
    /// it.
    pub fn start_video(&self, settings: Settings) -> Result<()> {
        let launch = self.launch().context("no app was launched")?;
        if settings.codec == Codec::Hevc && !self.hevc {
            bail!("the client asked for HEVC, which this host's encoder does not produce");
        }
        let (w, h) = self.session.request_size(settings.width, settings.height);
        *self.session.wanted_refresh.lock().unwrap() = Some(settings.fps);
        // Leave room for the parity packets within what the user picked (as Sunshine does).
        let video_kbps = settings.bitrate_kbps / 100 * (100 - FEC_PERCENT as u32);
        self.bitrate
            .store(video_kbps.saturating_mul(1000), Ordering::Relaxed);
        info!(
            w,
            h,
            fps = settings.fps,
            kbps = settings.bitrate_kbps,
            codec = ?settings.codec,
            packet = settings.packet_size,
            audio_ms = settings.audio_ms,
            audio_encrypted = settings.audio_encrypted,
            "Moonlight stream set up"
        );
        *self.audio.lock().unwrap() = Some(super::audio::Target::new(&launch, &settings));
        *self.video.lock().unwrap() = Some(Video {
            settings,
            ping_payload: launch.ping_payload,
            client: launch.client,
            peer: None,
            frame_index: 0,
            seq: 0,
            epoch: Instant::now(),
            size: None,
            buf: Vec::new(),
            sent: std::collections::VecDeque::new(),
        });
        *self.lost.lock().unwrap() = None;
        Ok(())
    }

    /// The control stream ended: no more video until the client sets up a stream again.
    pub fn end_video(&self) {
        *self.audio.lock().unwrap() = None;
        if self.video.lock().unwrap().take().is_some() {
            info!("Moonlight stream ended");
        }
    }

    /// Size of the desktop being captured, for pointer positions.
    pub fn desktop_size(&self) -> (u32, u32) {
        *self.session.current_size.lock().unwrap()
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::SeqCst);
    }

    /// The client lost frames from `first` on (reference frame invalidation): the encoder stops
    /// referencing them, or sends a keyframe when it cannot or they are too far back.
    pub fn invalidate_from(&self, first: u32) {
        let index = self.video.lock().unwrap().as_ref().and_then(|v| {
            v.sent
                .iter()
                .find(|&&(f, _)| f == first)
                .map(|&(_, index)| index)
        });
        debug!(first, ?index, "Moonlight lost frames");
        match index {
            Some(index) => {
                let mut lost = self.lost.lock().unwrap();
                *lost = Some(lost.map_or(index, |l| l.min(index)));
            }
            None => self.request_keyframe(),
        }
    }

    /// A UDP datagram on the audio port: a ping tells where the client is.
    fn on_audio_ping(&self, data: &[u8], from: SocketAddr) {
        if let Some(t) = self.audio.lock().unwrap().as_mut()
            && t.on_ping(data, from)
        {
            info!(%from, "Moonlight audio goes to the client");
        }
    }

    /// A UDP datagram on the video port: a ping tells where the client is.
    fn on_ping(&self, data: &[u8], from: SocketAddr) {
        let mut guard = self.video.lock().unwrap();
        let Some(v) = guard.as_mut() else { return };
        // Sunshine's pings carry the payload from SETUP and a sequence number; old clients
        // send "PING" and are told apart by their address.
        let ours = if data.len() >= 20 {
            data[..16] == v.ping_payload
        } else {
            data == b"PING" && from.ip() == v.client
        };
        if !ours || v.peer == Some(from) {
            return;
        }
        info!(%from, "Moonlight video goes to the client");
        v.peer = Some(from);
        drop(guard);
        self.keyframe.store(true, Ordering::SeqCst);
        self.session.take_over(self.index);
    }
}

pub struct GameStreamOutput {
    stream: Arc<Stream>,
}

impl Output for GameStreamOutput {
    fn prepare(&mut self, w: u32, h: u32, _max_in_flight: u32) -> Result<Prepared> {
        let mut guard = self.stream.video.lock().unwrap();
        let Some(v) = guard.as_mut() else {
            return Ok(Prepared::Absent);
        };
        if v.peer.is_none() {
            return Ok(Prepared::Absent);
        }
        let mut new_surface = self.stream.keyframe.swap(false, Ordering::SeqCst);
        if v.size != Some((w, h)) {
            v.size = Some((w, h));
            new_surface = true;
        }
        // No acknowledgements: the client picked the bitrate and the ceiling stays there.
        Ok(Prepared::Ready {
            new_surface,
            avc444: false,
            codec: v.settings.codec,
        })
    }

    fn send(&mut self, _mode: Mode, main: &EncodedFrame, _aux: &EncodedFrame) -> Result<bool> {
        let mut guard = self.stream.video.lock().unwrap();
        let Some(v) = guard.as_mut() else {
            return Ok(false);
        };
        let Some(peer) = v.peer else {
            return Ok(false);
        };
        v.frame_index = v.frame_index.wrapping_add(1);
        let timestamp = (v.epoch.elapsed().as_micros() * 9 / 100) as u32;
        let frame_type = if main.keyframe {
            2
        } else if main.recovery {
            5
        } else {
            1
        };
        let mut buf = std::mem::take(&mut v.buf);
        let packets = packetize(
            &main.data,
            &FrameInfo {
                index: v.frame_index,
                frame_type,
                timestamp,
                packet_size: v.settings.packet_size,
                fec_percent: FEC_PERCENT,
                min_parity: v.settings.min_parity,
            },
            &mut v.seq,
            &mut buf,
        )?;
        let block = v.settings.packet_size + MAX_RTP_HEADER;
        // GLIDEDESK_GS_TEST_DROP=N leaves out one packet in N, to see the client rebuild them
        // from parity.
        static DROP: std::sync::LazyLock<Option<u64>> = std::sync::LazyLock::new(|| {
            std::env::var("GLIDEDESK_GS_TEST_DROP")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n > 1)
        });
        static SENT: AtomicU64 = AtomicU64::new(0);
        // GLIDEDESK_GS_TEST_DROP_FRAME=N leaves out every Nth P-frame whole, to see the client
        // ask for an invalidation.
        static DROP_FRAME: std::sync::LazyLock<Option<u32>> = std::sync::LazyLock::new(|| {
            std::env::var("GLIDEDESK_GS_TEST_DROP_FRAME")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n > 1)
        });
        let skip_frame =
            !main.keyframe && DROP_FRAME.is_some_and(|n| v.frame_index.is_multiple_of(n));
        for p in 0..packets {
            if skip_frame {
                break;
            }
            if let Some(n) = *DROP
                && SENT.fetch_add(1, Ordering::Relaxed).is_multiple_of(n)
            {
                continue;
            }
            if let Err(e) = self
                .stream
                .socket
                .send_to(&buf[p * block..(p + 1) * block], peer)
            {
                debug!(%e, "Sending video failed");
            }
        }
        v.buf = buf;
        if main.index != 0 {
            if v.sent.len() == SENT_HISTORY {
                v.sent.pop_front();
            }
            v.sent.push_back((v.frame_index, main.index));
        }
        self.stream.sent.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    fn take_acked(&mut self) -> u64 {
        self.stream.sent.swap(0, Ordering::Relaxed)
    }

    fn take_delays(&mut self) -> Vec<(Duration, usize)> {
        Vec::new()
    }

    fn reset(&mut self) {
        if let Some(v) = self.stream.video.lock().unwrap().as_mut() {
            v.size = None;
        }
    }

    fn wants_refresh(&self) -> bool {
        self.stream.keyframe.load(Ordering::SeqCst) || self.stream.lost.lock().unwrap().is_some()
    }

    fn take_lost(&mut self) -> Option<u64> {
        self.stream.lost.lock().unwrap().take()
    }

    fn link_estimate(&mut self) -> Option<u32> {
        let bps = self.stream.bitrate.load(Ordering::Relaxed);
        (bps > 0).then_some(bps)
    }
}

/// The GameStream front end's side of the pipeline (output `index`), with its video socket
/// bound on the video port.
pub fn new(
    session: Arc<pipeline::Shared>,
    index: usize,
    ip: IpAddr,
    ports: Ports,
    hevc: bool,
    audio: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
) -> Result<(GameStreamOutput, Arc<Stream>)> {
    let udp = |port: u16| -> Result<UdpSocket> {
        let bind = SocketAddr::new(ip, port);
        let s = socket2::Socket::new(
            socket2::Domain::for_address(bind),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        let _ = s.set_send_buffer_size(8 << 20);
        s.bind(&bind.into())
            .with_context(|| format!("binding UDP {bind}"))?;
        Ok(UdpSocket::from(s))
    };
    let stream = Arc::new(Stream {
        session,
        index,
        ports,
        hevc,
        launch: Mutex::new(None),
        video: Mutex::new(None),
        socket: udp(ports.video)?,
        audio: Mutex::new(None),
        audio_socket: udp(ports.audio)?,
        keyframe: AtomicBool::new(false),
        lost: Mutex::new(None),
        bitrate: AtomicU32::new(0),
        sent: AtomicU64::new(0),
    });
    {
        let stream = Arc::clone(&stream);
        std::thread::Builder::new()
            .name("gs-video".into())
            .spawn(move || ping_loop(&stream.socket, "video", |d, f| stream.on_ping(d, f)))?;
    }
    {
        let stream = Arc::clone(&stream);
        std::thread::Builder::new()
            .name("gs-audio-ping".into())
            .spawn(move || {
                ping_loop(&stream.audio_socket, "audio", |d, f| {
                    stream.on_audio_ping(d, f)
                })
            })?;
    }
    if let Some(audio) = audio {
        super::audio::spawn(Arc::clone(&stream), audio)?;
    }
    Ok((
        GameStreamOutput {
            stream: Arc::clone(&stream),
        },
        stream,
    ))
}

/// Reads the client's pings on a media socket.
fn ping_loop(socket: &UdpSocket, what: &str, on_ping: impl Fn(&[u8], SocketAddr)) {
    let mut buf = [0u8; 64];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => on_ping(&buf[..n], from),
            // Windows reports an ICMP "port unreachable" for an earlier send as an error on
            // the next receive; it says nothing about this socket.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(e) => {
                warn!(%e, what, "Moonlight media socket failed");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Packets

/// Room the client leaves for the RTP header and its extension (moonlight-common-c
/// `MAX_RTP_HEADER_SIZE`): each packet is `packet_size` + this.
const MAX_RTP_HEADER: usize = 16;
/// RTP header (12) + reserved (4) + `NV_VIDEO_PACKET` (16).
const HEADER: usize = 32;
const FRAME_HEADER: usize = 8;
/// Shards (data and parity) in an FEC block: 8-bit Reed-Solomon.
const MAX_SHARDS: usize = 255;
/// Parity sent with the video, in percent of the data packets (Sunshine's default).
pub const FEC_PERCENT: usize = 20;
/// The FEC block number has 2 bits.
const MAX_BLOCKS: usize = 4;
/// The packet index within a block has 10 bits.
const MAX_PACKETS_PER_BLOCK_FIELD: usize = 1023;

const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
const FLAG_EOF: u8 = 0x2;
const FLAG_SOF: u8 = 0x4;
const RTP_FLAG_EXTENSION: u8 = 0x10;

struct FrameInfo {
    index: u32,
    /// 1 P-frame, 2 IDR, 5 P-frame after reference invalidation.
    frame_type: u8,
    /// 90 kHz.
    timestamp: u32,
    packet_size: usize,
    /// Parity packets per block, in percent of its data packets (0: none).
    fec_percent: usize,
    /// The client's minimum of parity packets per block.
    min_parity: usize,
}

/// Most data packets in an FEC block at `pct` percent parity: data and parity together stay
/// within the 255 shards of 8-bit Reed-Solomon (Sunshine `fec::max_data_shards`).
fn max_data_shards(pct: usize) -> usize {
    MAX_SHARDS * 100 / (100 + pct)
}

/// Parity packets for a block of `ds` data packets, and the percentage to announce so that the
/// client works out the same count (`(ds * pct + 99) / 100`). The client's minimum raises the
/// count for small blocks, as Sunshine does.
fn parity_for(ds: usize, pct: usize, min: usize) -> (usize, usize) {
    if pct == 0 {
        return (0, 0);
    }
    let mut parity = (ds * pct).div_ceil(100);
    let mut pct = pct;
    if parity < min {
        parity = min;
        pct = 100 * parity / ds;
    }
    (parity.min(MAX_SHARDS - ds), pct.min(255))
}

/// Lays the packets of one frame out in `buf`, each `packet_size + 16` bytes: per FEC block,
/// its data packets and then its parity packets. Returns how many. `seq` is the RTP sequence
/// number of the first and comes back past the last.
fn packetize(data: &[u8], f: &FrameInfo, seq: &mut u16, buf: &mut Vec<u8>) -> Result<usize> {
    let block = f.packet_size + MAX_RTP_HEADER;
    let per_packet = block - HEADER;
    let total = FRAME_HEADER + data.len();
    let packets = total.div_ceil(per_packet);
    let mut pct = f.fec_percent;
    let mut blocks = packets.div_ceil(max_data_shards(pct)).max(1);
    if blocks > MAX_BLOCKS {
        // An enormous frame: no parity for it, as Sunshine does.
        pct = 0;
        blocks = packets.div_ceil(MAX_SHARDS).clamp(1, MAX_BLOCKS);
    }
    let per_block = packets.div_ceil(blocks);
    if per_block > MAX_PACKETS_PER_BLOCK_FIELD {
        bail!("frame of {} bytes is too large to send", data.len());
    }
    // Sunshine's frame header: type 0x01 (short), processing latency (unknown: 0), frame type,
    // and the length of the last packet's payload, for codecs that cannot take the padding.
    let mut last = (total % per_packet) as u16;
    if last == 0 {
        last = per_packet as u16;
    }
    let mut header = [0u8; FRAME_HEADER];
    header[0] = 0x01;
    header[3] = f.frame_type;
    header[4..6].copy_from_slice(&last.to_le_bytes());

    buf.clear();
    let mut src = header.iter().chain(data.iter()).copied();
    let mut count = 0;
    let multi_fec = |b: usize| ((b << 4) | ((blocks - 1) << 6)) as u8;
    for b in 0..blocks {
        let ds = per_block.min(packets - b * per_block);
        let (parity, block_pct) = parity_for(ds, pct, f.min_parity);
        let fec_info =
            |x: usize| ((x as u32) << 12) | ((ds as u32) << 22) | ((block_pct as u32) << 4);
        let first = *seq;
        buf.resize((count + ds + parity) * block, 0);
        for x in 0..ds {
            let out = &mut buf[(count + x) * block..(count + x + 1) * block];
            // RTP: version 2 with the extension bit, sequence number and timestamp (BE).
            out[0] = 0x80 | RTP_FLAG_EXTENSION;
            let s = first.wrapping_add(x as u16);
            out[2..4].copy_from_slice(&s.to_be_bytes());
            out[4..8].copy_from_slice(&f.timestamp.to_be_bytes());
            // NV_VIDEO_PACKET (LE) after 4 reserved bytes.
            let nv = &mut out[16..32];
            nv[0..4].copy_from_slice(&(u32::from(s) << 8).to_le_bytes());
            nv[4..8].copy_from_slice(&f.index.to_le_bytes());
            let mut flags = FLAG_CONTAINS_PIC_DATA;
            if x == 0 {
                flags |= FLAG_SOF;
            }
            if x == ds - 1 {
                flags |= FLAG_EOF;
            }
            nv[8] = flags;
            nv[10] = 0x10;
            nv[11] = multi_fec(b);
            nv[12..16].copy_from_slice(&fec_info(x).to_le_bytes());
            for (dst, byte) in out[HEADER..].iter_mut().zip(&mut src) {
                *dst = byte;
            }
        }
        if parity > 0 {
            let (data_part, parity_part) =
                buf[count * block..(count + ds + parity) * block].split_at_mut(ds * block);
            let shards: Vec<&[u8]> = data_part.chunks(block).collect();
            let mut outs: Vec<&mut [u8]> = parity_part.chunks_mut(block).collect();
            super::fec::encode(&shards, &mut outs);
            // Parity covers whole packets. Its own header gets only what the client reads
            // from it; the client rebuilds those fields of a recovered packet itself.
            for (k, out) in outs.iter_mut().enumerate() {
                let x = ds + k;
                out[0] = 0x80 | RTP_FLAG_EXTENSION;
                out[2..4].copy_from_slice(&first.wrapping_add(x as u16).to_be_bytes());
                out[4..8].copy_from_slice(&f.timestamp.to_be_bytes());
                out[20..24].copy_from_slice(&f.index.to_le_bytes());
                out[27] = multi_fec(b);
                out[28..32].copy_from_slice(&fec_info(x).to_le_bytes());
            }
        }
        count += ds + parity;
        *seq = first.wrapping_add((ds + parity) as u16);
    }
    Ok(count)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_moonlight_sdp() {
        let sdp = "v=0\r\na=x-nv-video[0].clientViewportWd:1920 \r\na=x-nv-video[0].clientViewportHt:1080 \r\n\
                   a=x-nv-video[0].maxFPS:60 \r\na=x-nv-video[0].packetSize:1392 \r\n\
                   a=x-nv-vqos[0].bw.maximumBitrateKbps:16000 \r\na=x-ml-video.configuredBitrateKbps:20000 \r\n\
                   a=x-nv-vqos[0].bitStreamFormat:1 \r\n";
        let s = Settings::from_sdp(sdp).unwrap();
        assert_eq!(
            s,
            Settings {
                width: 1920,
                height: 1080,
                fps: 60,
                packet_size: 1392,
                bitrate_kbps: 20_000,
                codec: Codec::Hevc,
                audio_encrypted: false,
                audio_ms: 5,
                audio_channels: 2,
                min_parity: 0,
            }
        );
        assert!(Settings::from_sdp("a=x-nv-video[0].maxFPS:60").is_err());
    }

    /// One packet as the client reads it.
    struct Seen {
        seq: u16,
        index_in_block: usize,
        data_shards: usize,
        pct: usize,
        block: usize,
        last_block: usize,
        flags: u8,
        frame: u32,
    }

    fn seen(pkt: &[u8]) -> Seen {
        let nv = &pkt[16..32];
        let fec = u32::from_le_bytes(nv[12..16].try_into().unwrap());
        Seen {
            seq: u16::from_be_bytes([pkt[2], pkt[3]]),
            index_in_block: ((fec >> 12) & 0x3ff) as usize,
            data_shards: (fec >> 22) as usize,
            pct: ((fec >> 4) & 0xff) as usize,
            block: usize::from((nv[11] >> 4) & 3),
            last_block: usize::from((nv[11] >> 6) & 3),
            flags: nv[8],
            frame: u32::from_le_bytes(nv[4..8].try_into().unwrap()),
        }
    }

    fn frame(len: usize, packet_size: usize, fec_percent: usize) -> (Vec<u8>, Vec<u8>, usize) {
        let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3 + i / 251) as u8).collect();
        let mut seq = 65530u16;
        let mut buf = Vec::new();
        let f = FrameInfo {
            index: 9,
            frame_type: 2,
            timestamp: 1234,
            packet_size,
            fec_percent,
            min_parity: 2,
        };
        let n = packetize(&data, &f, &mut seq, &mut buf).unwrap();
        assert_eq!(seq, 65530u16.wrapping_add(n as u16));
        (data, buf, n)
    }

    #[test]
    fn frames_are_cut_like_sunshine() {
        let packet_size = 1024;
        let block = packet_size + MAX_RTP_HEADER;
        let per_packet = block - HEADER;
        for fec_percent in [0, FEC_PERCENT] {
            for len in [
                1usize,
                per_packet - FRAME_HEADER,
                per_packet,
                300 * per_packet + 7,
            ] {
                let (data, buf, n) = frame(len, packet_size, fec_percent);
                // Walk the packets as the client does: each block's data, then its parity.
                let mut payload = Vec::new();
                let mut p = 0;
                let mut blocks = 0;
                while p < n {
                    let first = seen(&buf[p * block..(p + 1) * block]);
                    assert_eq!(first.index_in_block, 0);
                    assert_eq!(first.block, blocks);
                    let ds = first.data_shards;
                    // The client's own count: (ds * pct + 99) / 100.
                    let parity = (ds * first.pct).div_ceil(100);
                    if fec_percent > 0 {
                        assert!(parity >= 2, "the client's minimum");
                    }
                    assert!(ds + parity <= 255);
                    for x in 0..ds + parity {
                        let pkt = &buf[(p + x) * block..(p + x + 1) * block];
                        let s = seen(pkt);
                        assert_eq!(pkt[0], 0x90);
                        assert_eq!(s.seq, first.seq.wrapping_add(x as u16));
                        assert_eq!((s.index_in_block, s.data_shards, s.pct), (x, ds, first.pct));
                        assert_eq!((s.block, s.frame), (blocks, 9));
                        if x < ds {
                            assert_eq!(s.flags & FLAG_SOF != 0, x == 0);
                            assert_eq!(s.flags & FLAG_EOF != 0, x == ds - 1);
                            payload.extend_from_slice(&pkt[HEADER..]);
                        }
                    }
                    p += ds + parity;
                    blocks += 1;
                }
                assert_eq!(p, n);
                assert_eq!(seen(&buf[..block]).last_block, blocks - 1);
                assert_eq!(buf[HEADER], 1);
                assert_eq!(buf[HEADER + 3], 2);
                payload.drain(..FRAME_HEADER);
                assert_eq!(&payload[..len], &data[..]);
            }
        }
    }

    #[test]
    fn lost_packets_are_recovered_from_parity() {
        let packet_size = 1024;
        let block = packet_size + MAX_RTP_HEADER;
        let (_, buf, n) = frame(40 * (block - HEADER), packet_size, FEC_PERCENT);
        let ds = seen(&buf[..block]).data_shards;
        assert_eq!(n, ds + (ds * FEC_PERCENT).div_ceil(100));
        let shards: Vec<Vec<u8>> = buf.chunks(block).map(<[u8]>::to_vec).collect();
        let (data, parity) = shards.split_at(ds);
        let mut damaged = data.to_vec();
        let lost = [0, 7, ds - 1];
        for &i in &lost {
            damaged[i] = vec![0; block];
        }
        super::super::fec::tests::recover(&mut damaged, &lost, parity);
        // What the client keeps from a recovered packet (it rebuilds the RTP header, frame
        // number and block numbers itself): packet index, flags and the payload.
        for &i in &lost {
            assert_eq!(damaged[i][16..20], data[i][16..20]);
            assert_eq!(damaged[i][24..27], data[i][24..27]);
            assert_eq!(damaged[i][HEADER..], data[i][HEADER..]);
        }
    }
    #[test]
    fn ping_payload_is_hex() {
        let l = Launch::new([0; 16], 7, [10, 0, 0, 2].into(), 1, "Desktop");
        assert!(l.ping_payload.iter().all(u8::is_ascii_hexdigit));
    }
}

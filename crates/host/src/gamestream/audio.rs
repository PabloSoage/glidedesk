//! Moonlight audio: the session's sound as Opus at a constant bitrate, one RTP packet per
//! 5 ms (or what the client asks for) on UDP 48000 (Sunshine `src/stream.cpp`
//! `audioBroadcastThread` and `src/audio.cpp`, moonlight-common-c `AudioStream.c` and
//! `RtpAudioQueue.c`, commits in THIRD_PARTY.md).
//!
//! RTP header: version 2, payload type 97, sequence number and timestamp (in milliseconds),
//! big-endian. Clients that ask for it (Moonlight always does) get the Opus data encrypted with
//! AES-128-CBC and PKCS#7 padding under the `/launch` key; the IV is `rikeyid` plus the
//! sequence number (BE u32) followed by zeros. The client collects packets in blocks of four;
//! in-order packets play right away, so no parity packets are sent yet, but every packet of a
//! block must have the same size, which is why the bitrate is constant.
//!
//! Stereo, 5.1 and 7.1 go in the Opus layouts `DESCRIBE` lists ([`Layout`]): multistream Opus
//! for surround, in Moonlight's channel order (FL FR FC LFE BL BR SL SR). The sound comes from
//! the session in 7.1 where it can ([`crate::audio`]) and is fitted to what the client asked
//! for ([`remix`]); from a stereo session, surround clients get it in the front channels.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use anyhow::Result;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::stream::{Launch, Settings, Stream};

const RATE: u32 = 48_000;
const PAYLOAD_TYPE: u8 = 97;

/// An Opus layout, as Sunshine lists them (`src/audio.cpp` `stream_configs`): channels,
/// streams, coupled (two-channel) streams and bitrate. The channel mapping is the identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub channels: usize,
    pub streams: u8,
    pub coupled: u8,
    pub bitrate: i32,
}

impl Layout {
    /// What the client asked for: channels and normal or high quality. Normal-quality stereo
    /// is 128 kbit/s here (Sunshine: 96), transparent for desktop sound.
    pub fn for_client(channels: u32, high_quality: bool) -> Self {
        let l = |channels, streams, coupled, bitrate| Layout {
            channels,
            streams,
            coupled,
            bitrate,
        };
        match (channels, high_quality) {
            (6, false) => l(6, 4, 2, 256_000),
            (6, true) => l(6, 6, 0, 1_536_000),
            (8, false) => l(8, 5, 3, 450_000),
            (8, true) => l(8, 8, 0, 2_048_000),
            (_, false) => l(2, 1, 1, 128_000),
            (_, true) => l(2, 1, 1, 512_000),
        }
    }
}

/// Where the audio of the current stream goes.
pub struct Target {
    key: [u8; 16],
    key_id: u32,
    encrypted: bool,
    /// Milliseconds of sound per packet.
    ms: u32,
    layout: Layout,
    ping_payload: [u8; 16],
    client: IpAddr,
    peer: Option<SocketAddr>,
    seq: u16,
    timestamp: u32,
}

impl Target {
    pub fn new(launch: &Launch, settings: &Settings) -> Self {
        let layout = Layout::for_client(settings.audio_channels, settings.audio_high_quality);
        if layout.channels != settings.audio_channels as usize {
            warn!(
                channels = settings.audio_channels,
                "Moonlight asked for a sound layout this host does not send; sending stereo"
            );
        }
        Self {
            key: launch.key,
            key_id: launch.key_id,
            encrypted: settings.audio_encrypted,
            ms: settings.audio_ms,
            layout,
            ping_payload: launch.ping_payload,
            client: launch.client,
            peer: None,
            seq: 0,
            timestamp: 0,
        }
    }

    /// A ping on the audio port: true if it is this client's and new.
    pub fn on_ping(&mut self, data: &[u8], from: SocketAddr) -> bool {
        let ours = if data.len() >= 20 {
            data[..16] == self.ping_payload
        } else {
            data == b"PING" && from.ip() == self.client
        };
        if !ours || self.peer == Some(from) {
            return false;
        }
        self.peer = Some(from);
        true
    }

    /// One RTP packet for an Opus frame; the sequence number and timestamp move on.
    fn packet(&mut self, opus: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(12 + opus.len() + 16);
        p.extend_from_slice(&[0x80, PAYLOAD_TYPE]);
        p.extend_from_slice(&self.seq.to_be_bytes());
        p.extend_from_slice(&self.timestamp.to_be_bytes());
        p.extend_from_slice(&[0; 4]);
        if self.encrypted {
            let mut iv = [0u8; 16];
            iv[..4].copy_from_slice(&self.key_id.wrapping_add(u32::from(self.seq)).to_be_bytes());
            p.extend_from_slice(&cbc_encrypt(&self.key, &iv, opus));
        } else {
            p.extend_from_slice(opus);
        }
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.ms);
        p
    }
}

/// AES-128-CBC with PKCS#7 padding.
fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let pad = 16 - data.len() % 16;
    let mut out = data.to_vec();
    out.resize(data.len() + pad, pad as u8);
    let mut prev = *iv;
    for block in out.as_chunks_mut::<16>().0 {
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
        prev = *block;
    }
    out
}

/// Fits interleaved samples with `from` channels (2, or 7.1) to `to` channels (2, 6 or 8):
/// 7.1 to 5.1 folds the side channels into the back ones, to stereo mixes down; stereo to
/// surround fills the front channels and leaves the rest silent.
pub fn remix(src: &[i16], from: usize, to: usize, out: &mut Vec<i16>) {
    match (from, to) {
        (a, b) if a == b => out.extend_from_slice(src),
        (8, 2) => crate::sound::downmix_71(src, out),
        (8, 6) => {
            for f in src.as_chunks::<8>().0 {
                out.extend_from_slice(&f[..4]);
                out.push(f[4].saturating_add(f[6]));
                out.push(f[5].saturating_add(f[7]));
            }
        }
        (2, n) => {
            for f in src.as_chunks::<2>().0 {
                out.extend_from_slice(f);
                out.extend(std::iter::repeat_n(0, n - 2));
            }
        }
        _ => {}
    }
}

/// Sends the session's sound (`channels` per sample: 2, or 7.1) to whichever Moonlight client
/// is streaming.
pub fn spawn(
    stream: Arc<Stream>,
    audio: broadcast::Sender<Vec<u8>>,
    channels: usize,
) -> Result<()> {
    std::thread::Builder::new()
        .name("gs-audio".into())
        .spawn(move || run(&stream, &audio, channels))?;
    Ok(())
}

enum Encoder {
    Stereo(opus::Encoder),
    Surround(opus::MSEncoder),
}

impl Encoder {
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> opus::Result<usize> {
        match self {
            Self::Stereo(e) => e.encode(pcm, out),
            Self::Surround(e) => e.encode(pcm, out),
        }
    }
}

fn encoder(layout: Layout, ms: u32) -> Option<Encoder> {
    let app = opus::Application::LowDelay;
    let made = if layout.channels == 2 {
        opus::Encoder::new(RATE, opus::Channels::Stereo, app).and_then(|mut e| {
            e.set_vbr(false)?;
            e.set_bitrate(opus::Bitrate::Bits(layout.bitrate))?;
            Ok(Encoder::Stereo(e))
        })
    } else {
        let mapping: Vec<u8> = (0..layout.channels as u8).collect();
        opus::MSEncoder::new(RATE, layout.streams, layout.coupled, &mapping, app).and_then(
            |mut e| {
                e.set_vbr(false)?;
                e.set_bitrate(opus::Bitrate::Bits(layout.bitrate))?;
                Ok(Encoder::Surround(e))
            },
        )
    };
    match made {
        Ok(e) => {
            debug!(?layout, ms, "Opus encoder for Moonlight");
            Some(e)
        }
        Err(e) => {
            warn!(%e, ?layout, "No Opus encoder: Moonlight gets no sound");
            None
        }
    }
}

fn run(stream: &Stream, audio: &broadcast::Sender<Vec<u8>>, channels: usize) {
    let mut rx = audio.subscribe();
    let mut enc: Option<(Encoder, Layout, u32)> = None;
    let mut samples: Vec<i16> = Vec::new();
    let mut pending: Vec<i16> = Vec::new();
    let mut out = vec![0u8; 8000];
    let mut started = false;
    loop {
        let block = match rx.blocking_recv() {
            Ok(b) => b,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let mut guard = stream.audio.lock().unwrap();
        let Some((t, peer)) = guard.as_mut().and_then(|t| t.peer.map(|p| (t, p))) else {
            pending.clear();
            started = false;
            continue;
        };
        if !started {
            info!(channels = t.layout.channels, "Moonlight audio starts");
            started = true;
        }
        samples.clear();
        samples.extend(
            block
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b)),
        );
        if enc
            .as_ref()
            .is_none_or(|(_, layout, ms)| (*layout, *ms) != (t.layout, t.ms))
        {
            pending.clear();
            enc = encoder(t.layout, t.ms).map(|e| (e, t.layout, t.ms));
        }
        let Some((e, layout, _)) = enc.as_mut() else {
            continue;
        };
        remix(&samples, channels, layout.channels, &mut pending);
        let frame = (RATE / 1000 * t.ms) as usize * layout.channels;

        let mut used = 0;
        while pending.len() - used >= frame {
            let Ok(n) = e.encode(&pending[used..used + frame], &mut out) else {
                used += frame;
                continue;
            };
            used += frame;
            let packet = t.packet(&out[..n]);
            if let Err(e) = stream.audio_socket.send_to(&packet, peer) {
                debug!(%e, "Sending audio failed");
            }
        }
        pending.drain(..used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockDecrypt;

    fn cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let cipher = Aes128::new(GenericArray::from_slice(key));
        let mut prev = *iv;
        let mut out = Vec::new();
        for block in data.as_chunks::<16>().0 {
            let mut b = GenericArray::clone_from_slice(block);
            cipher.decrypt_block(&mut b);
            out.extend(b.iter().zip(prev).map(|(x, p)| x ^ p));
            prev = *block;
        }
        let pad = *out.last().unwrap() as usize;
        out.truncate(out.len() - pad);
        out
    }

    #[test]
    fn packets_are_encrypted_like_moonlight_expects() {
        let launch = Launch::new([3; 16], 0xfffffff0, [10, 0, 0, 2].into(), 1, "Desktop");
        let settings = Settings::from_sdp(
            "a=x-nv-video[0].clientViewportWd:1280\na=x-nv-video[0].clientViewportHt:720\n\
             a=x-nv-video[0].maxFPS:60\na=x-nv-video[0].packetSize:1024\n\
             a=x-nv-general.featureFlags:167\na=x-nv-aqos.packetDuration:5\n",
        )
        .unwrap();
        assert!(settings.audio_encrypted);
        let mut t = Target::new(&launch, &settings);
        for seq in 0..20u16 {
            let opus: Vec<u8> = (0..80).map(|i| (i as u8).wrapping_mul(seq as u8)).collect();
            let p = t.packet(&opus);
            assert_eq!(&p[..2], &[0x80, 97]);
            assert_eq!(u16::from_be_bytes([p[2], p[3]]), seq);
            assert_eq!(
                u32::from_be_bytes(p[4..8].try_into().unwrap()),
                u32::from(seq) * 5
            );
            // The IV wraps like the client's (avRiKeyId + sequence number, as u32).
            let mut iv = [0u8; 16];
            iv[..4].copy_from_slice(&0xfffffff0u32.wrapping_add(u32::from(seq)).to_be_bytes());
            assert_eq!(cbc_decrypt(&[3; 16], &iv, &p[12..]), opus);
            // 80 bytes pad to 96: a constant size for constant-size Opus frames.
            assert_eq!(p.len(), 12 + 96);
        }
    }

    #[test]
    fn constant_bitrate_gives_constant_sizes() {
        let mut out = vec![0u8; 8000];
        for (channels, hq) in [(2, false), (6, false), (6, true), (8, false), (8, true)] {
            let layout = Layout::for_client(channels, hq);
            let mut e = encoder(layout, 5).unwrap();
            let mut sizes = std::collections::BTreeSet::new();
            for k in 0..100 {
                // A tone that changes, then silence: VBR would vary the size.
                let frame: Vec<i16> = (0..240 * layout.channels)
                    .map(|i| {
                        if k < 50 {
                            ((((i / layout.channels) + k * 240) as f64 * 0.05).sin() * 8000.0)
                                as i16
                        } else {
                            0
                        }
                    })
                    .collect();
                sizes.insert(e.encode(&frame, &mut out).unwrap());
            }
            assert_eq!(sizes.len(), 1, "{layout:?}: {sizes:?}");
        }
    }

    #[test]
    fn surround_decodes_with_moonlights_layout() {
        // Moonlight's decoder is built from the DESCRIBE parameters: for normal 5.1, 6
        // channels, 4 streams, 2 coupled, mapping 0..6 (once it undoes GFE's rotation).
        let layout = Layout::for_client(6, false);
        let mut e = encoder(layout, 10).unwrap();
        let mut d = opus::MSDecoder::new(RATE, 4, 2, &[0, 1, 2, 3, 4, 5]).unwrap();
        // A tone on the back left channel only.
        let mut pcm = vec![0i16; 480 * 6];
        for (i, f) in pcm.chunks_mut(6).enumerate() {
            f[4] = ((i as f64 * 0.2).sin() * 10000.0) as i16;
        }
        let mut out = vec![0u8; 4000];
        let mut back = vec![0i16; 480 * 6];
        let mut energy = [0f64; 6];
        for _ in 0..10 {
            let n = e.encode(&pcm, &mut out).unwrap();
            d.decode(&out[..n], &mut back, false).unwrap();
        }
        for f in back.chunks(6) {
            for (c, s) in f.iter().enumerate() {
                energy[c] += f64::from(*s).powi(2);
            }
        }
        let loudest = (0..6)
            .max_by(|&a, &b| energy[a].total_cmp(&energy[b]))
            .unwrap();
        assert_eq!(loudest, 4, "{energy:?}");
    }

    #[test]
    fn layouts_are_fitted() {
        // 7.1 frame: FL FR FC LFE RL RR SL SR.
        let f71 = [1000i16, 2000, 0, 500, 300, 400, 30, 40];
        let mut out = Vec::new();
        remix(&f71, 8, 6, &mut out);
        assert_eq!(out, [1000, 2000, 0, 500, 330, 440]);
        out.clear();
        remix(&[100, -100], 2, 8, &mut out);
        assert_eq!(out, [100, -100, 0, 0, 0, 0, 0, 0]);
        out.clear();
        // Stereo played into the 7.1 output comes back as it was.
        remix(&[1234, -4321, 0, 0, 0, 0, 0, 0], 8, 2, &mut out);
        assert_eq!(out, [1234, -4321]);
    }
}

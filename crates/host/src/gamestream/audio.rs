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

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use anyhow::Result;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::stream::{Launch, Settings, Stream};

/// Stereo Opus bitrate: what Sunshine uses for high-quality stereo is 512 kbit/s, normal
/// 96 kbit/s; this sits in between and stays transparent.
const BITRATE: i32 = 128_000;
const RATE: u32 = 48_000;
const PAYLOAD_TYPE: u8 = 97;

/// Where the audio of the current stream goes.
pub struct Target {
    key: [u8; 16],
    key_id: u32,
    encrypted: bool,
    /// Milliseconds of sound per packet.
    ms: u32,
    ping_payload: [u8; 16],
    client: IpAddr,
    peer: Option<SocketAddr>,
    seq: u16,
    timestamp: u32,
}

impl Target {
    pub fn new(launch: &Launch, settings: &Settings) -> Self {
        if settings.audio_channels != 2 {
            warn!(
                channels = settings.audio_channels,
                "Moonlight asked for surround sound; this host sends stereo only"
            );
        }
        Self {
            key: launch.key,
            key_id: launch.key_id,
            encrypted: settings.audio_encrypted,
            ms: settings.audio_ms,
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

/// Sends the session's sound to whichever Moonlight client is streaming.
pub fn spawn(stream: Arc<Stream>, audio: broadcast::Sender<Vec<u8>>) -> Result<()> {
    std::thread::Builder::new()
        .name("gs-audio".into())
        .spawn(move || run(&stream, &audio))?;
    Ok(())
}

fn encoder(ms: u32) -> Option<opus::Encoder> {
    let mut enc =
        match opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay) {
            Ok(e) => e,
            Err(e) => {
                warn!(%e, "No Opus encoder: Moonlight gets no sound");
                return None;
            }
        };
    let _ = enc.set_vbr(false);
    let _ = enc.set_bitrate(opus::Bitrate::Bits(BITRATE));
    debug!(ms, "Opus encoder for Moonlight");
    Some(enc)
}

fn run(stream: &Stream, audio: &broadcast::Sender<Vec<u8>>) {
    let mut rx = audio.subscribe();
    let mut enc: Option<(opus::Encoder, u32)> = None;
    let mut pending: Vec<i16> = Vec::new();
    let mut out = vec![0u8; 4000];
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
            info!("Moonlight audio starts");
            started = true;
        }
        pending.extend(
            block
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b)),
        );
        if enc.as_ref().is_none_or(|(_, ms)| *ms != t.ms) {
            enc = encoder(t.ms).map(|e| (e, t.ms));
        }
        let Some((e, _)) = enc.as_mut() else { continue };
        let frame = (RATE / 1000 * t.ms * 2) as usize;

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
        let mut e = encoder(5).unwrap();
        let mut out = vec![0u8; 4000];
        let mut sizes = std::collections::BTreeSet::new();
        for k in 0..100 {
            // A tone that changes, then silence: VBR would vary the size.
            let frame: Vec<i16> = (0..480)
                .map(|i| {
                    if k < 50 {
                        ((f64::from(i + k * 480) * 0.05).sin() * 8000.0) as i16
                    } else {
                        0
                    }
                })
                .collect();
            sizes.insert(e.encode(&frame, &mut out).unwrap());
        }
        assert_eq!(sizes.len(), 1, "{sizes:?}");
    }
}

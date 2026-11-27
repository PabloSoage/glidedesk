//! Media packets: video frames in Reed-Solomon protected shards, audio blocks, the client's
//! UDP hello.
//!
//! A video frame is cut into blocks of at most [`MAX_DATA_SHARDS`] shards of up to
//! [`SHARD`] bytes; each block gets `parity` extra shards (GF(2^8) Reed-Solomon), so any
//! `data_shards` of its packets rebuild it. Packet plaintext (before [`crate::crypto`]):
//!
//! ```text
//! VIDEO u8 | frame_id u32 | flags u8 | block u8 | blocks u8 | shard u8 | data_shards u8
//!          | parity_shards u8 | frame_len u32 | block_len u32 | shard bytes
//! AUDIO u8 | seq u32 | PCM S16LE 48 kHz stereo
//! HELLO u8 | token [16]
//! SYNC  u8 | last_frame_id u32   (server: frames are held back waiting for acknowledgements)
//! VIDEO2 u8 | monitor u8 | the rest of a VIDEO packet         (version 6)
//! SYNC2  u8 | monitor u8 | last_frame_id u32                   (version 6)
//! ```
//!
//! VIDEO2 and SYNC2 carry the video of monitors other than monitor 0 (see
//! [`crate::Monitor`]); each monitor numbers its frames on its own and needs a
//! [`Reassembler`] of its own. Monitor 0 keeps VIDEO and SYNC, so a session of one monitor
//! looks the same to every version.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const VIDEO: u8 = 1;
pub const AUDIO: u8 = 2;
pub const HELLO: u8 = 3;
pub const SYNC: u8 = 4;
/// (Version 6.) Video of a monitor other than 0.
pub const VIDEO2: u8 = 5;
/// (Version 6.) [`SYNC`] for a monitor other than 0.
pub const SYNC2: u8 = 6;

/// Shard payload: with the headers and encryption a packet stays under 1200 bytes, below
/// the MTU of any usual path (VPNs included).
pub const SHARD: usize = 1100;
/// Data shards per block (with the parity, a block stays within 255 shards: the shard
/// numbers are bytes).
pub const MAX_DATA_SHARDS: usize = 200;
const VIDEO_HEADER: usize = 1 + 4 + 1 + 1 + 1 + 1 + 1 + 1 + 4 + 4;
const FLAG_KEYFRAME: u8 = 1;
/// (Version 2.) The frame references only frames from before an invalidation.
const FLAG_RECOVERY: u8 = 2;

/// Splits one encoded frame into packets, with `fec_percent` parity per block (at least one
/// parity shard when it is not zero).
pub fn packetize(frame_id: u32, keyframe: bool, data: &[u8], fec_percent: u32) -> Vec<Vec<u8>> {
    packetize_frame(frame_id, keyframe, false, data, fec_percent)
}

/// [`packetize`], marking a recovery frame (see [`crate::ClientMsg::InvalidateFrames`]).
pub fn packetize_frame(
    frame_id: u32,
    keyframe: bool,
    recovery: bool,
    data: &[u8],
    fec_percent: u32,
) -> Vec<Vec<u8>> {
    packetize_with(&[VIDEO], frame_id, keyframe, recovery, data, fec_percent)
}

/// [`packetize_frame`] for the video of `monitor` (version 6): [`VIDEO2`] packets, or plain
/// [`VIDEO`] ones for monitor 0.
pub fn packetize_monitor(
    monitor: u8,
    frame_id: u32,
    keyframe: bool,
    recovery: bool,
    data: &[u8],
    fec_percent: u32,
) -> Vec<Vec<u8>> {
    if monitor == 0 {
        return packetize_frame(frame_id, keyframe, recovery, data, fec_percent);
    }
    packetize_with(&[VIDEO2, monitor], frame_id, keyframe, recovery, data, fec_percent)
}

/// The monitor a video packet belongs to ([`VIDEO`] is monitor 0); `None` for other packets.
pub fn video_monitor(p: &[u8]) -> Option<u8> {
    match p {
        [VIDEO, ..] => Some(0),
        [VIDEO2, monitor, ..] => Some(*monitor),
        _ => None,
    }
}

/// A video packet from its frame id on (the kind, and the monitor of [`VIDEO2`], left out).
fn video_body(p: &[u8]) -> Option<&[u8]> {
    match p {
        [VIDEO, body @ ..] => Some(body),
        [VIDEO2, _, body @ ..] => Some(body),
        _ => None,
    }
}

fn packetize_with(
    prefix: &[u8],
    frame_id: u32,
    keyframe: bool,
    recovery: bool,
    data: &[u8],
    fec_percent: u32,
) -> Vec<Vec<u8>> {
    let flags = if keyframe { FLAG_KEYFRAME } else { 0 } | if recovery { FLAG_RECOVERY } else { 0 };
    let block_max = SHARD * MAX_DATA_SHARDS;
    let blocks: Vec<&[u8]> = if data.is_empty() {
        vec![&[][..]]
    } else {
        data.chunks(block_max).collect()
    };
    let mut out = Vec::new();
    for (bi, block) in blocks.iter().enumerate() {
        let data_shards = block.len().div_ceil(SHARD).max(1);
        // The coder works on shards of an even size.
        let shard_size = block.len().div_ceil(data_shards).max(1).next_multiple_of(2);
        let parity = if fec_percent == 0 {
            0
        } else {
            (data_shards * fec_percent as usize)
                .div_ceil(100)
                .clamp(1, 255 - data_shards)
        };
        let mut shards: Vec<Vec<u8>> = (0..data_shards)
            .map(|i| {
                let mut s = vec![0u8; shard_size];
                let start = (i * shard_size).min(block.len());
                let end = ((i + 1) * shard_size).min(block.len());
                s[..end - start].copy_from_slice(&block[start..end]);
                s
            })
            .collect();
        if parity > 0 {
            shards.extend(
                reed_solomon_simd::encode(data_shards, parity, &shards)
                    .expect("supported shard counts and sizes"),
            );
        }
        for (si, shard) in shards.iter().enumerate() {
            let mut p = Vec::with_capacity(prefix.len() + VIDEO_HEADER + shard.len());
            p.extend_from_slice(prefix);
            p.extend_from_slice(&frame_id.to_le_bytes());
            p.push(flags);
            p.push(bi as u8);
            p.push(blocks.len() as u8);
            p.push(si as u8);
            p.push(data_shards as u8);
            p.push(parity as u8);
            p.extend_from_slice(&(data.len() as u32).to_le_bytes());
            p.extend_from_slice(&(block.len() as u32).to_le_bytes());
            p.extend_from_slice(shard);
            out.push(p);
        }
    }
    out
}

/// What the reassembler hands back.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Frame {
        frame_id: u32,
        keyframe: bool,
        /// References only frames from before an invalidation (version 2).
        recovery: bool,
        data: Vec<u8>,
        /// From its first packet to the one that completed it (how long the frame took to
        /// cross the link, for the latency statistics).
        spread: Duration,
    },
    /// The frame will not be completed (a later one was): its successors may not decode
    /// until the next keyframe.
    Lost { frame_id: u32 },
}

struct Block {
    data_shards: usize,
    parity: usize,
    len: usize,
    shards: Vec<Option<Vec<u8>>>,
    have: usize,
    done: Option<Vec<u8>>,
}

struct Partial {
    keyframe: bool,
    recovery: bool,
    len: usize,
    blocks: Vec<Option<Block>>,
    /// When its first and its last packet arrived.
    first: Instant,
    seen: Instant,
}

/// Rebuilds frames from video packets in any order, with up to `parity` losses per block.
#[derive(Default)]
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    /// Newest frame delivered or declared lost.
    last: Option<u32>,
}

/// `a` comes after `b`, allowing the frame counter to wrap.
fn after(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

impl Reassembler {
    /// Feeds one video packet (plaintext, starting with [`VIDEO`] or [`VIDEO2`]; one
    /// reassembler takes the packets of one monitor).
    pub fn push(&mut self, p: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        // The header without its first byte, the kind.
        let Some(p) = video_body(p).filter(|b| b.len() >= VIDEO_HEADER - 1) else {
            return events;
        };
        let frame_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
        if self.last.is_some_and(|l| !after(frame_id, l)) {
            return events; // already delivered or given up
        }
        let (flags, bi, blocks, si, data_shards, parity) = (
            p[4],
            p[5] as usize,
            p[6] as usize,
            p[7] as usize,
            p[8] as usize,
            p[9] as usize,
        );
        let frame_len = u32::from_le_bytes(p[10..14].try_into().unwrap()) as usize;
        let block_len = u32::from_le_bytes(p[14..18].try_into().unwrap()) as usize;
        let shard = &p[VIDEO_HEADER - 1..];
        if bi >= blocks || si >= data_shards + parity || data_shards == 0 {
            return events;
        }
        let part = self.partial.entry(frame_id).or_insert_with(|| Partial {
            keyframe: flags & FLAG_KEYFRAME != 0,
            recovery: flags & FLAG_RECOVERY != 0,
            len: frame_len,
            blocks: (0..blocks).map(|_| None).collect(),
            first: Instant::now(),
            seen: Instant::now(),
        });
        part.seen = Instant::now();
        if part.blocks.len() != blocks {
            return events;
        }
        let block = part.blocks[bi].get_or_insert_with(|| Block {
            data_shards,
            parity,
            len: block_len,
            shards: vec![None; data_shards + parity],
            have: 0,
            done: None,
        });
        if block.done.is_some() || block.shards.len() != data_shards + parity {
            return events;
        }
        if block.shards[si].is_none() {
            block.shards[si] = Some(shard.to_vec());
            block.have += 1;
        }
        if block.have >= block.data_shards {
            let mut shards = std::mem::take(&mut block.shards);
            let ok = rebuild(block.data_shards, block.parity, &mut shards);
            if ok {
                let mut data: Vec<u8> = shards[..block.data_shards]
                    .iter()
                    .flat_map(|s| s.as_deref().unwrap_or(&[]).iter().copied())
                    .collect();
                data.truncate(block.len);
                block.done = Some(data);
            } else {
                block.shards = shards;
            }
        }
        if part
            .blocks
            .iter()
            .all(|b| b.as_ref().is_some_and(|b| b.done.is_some()))
        {
            let part = self.partial.remove(&frame_id).expect("present");
            let mut data: Vec<u8> = part
                .blocks
                .into_iter()
                .flat_map(|b| b.and_then(|b| b.done).unwrap_or_default())
                .collect();
            data.truncate(part.len);
            // Anything older still incomplete will not be usable any more.
            let older: Vec<u32> = self
                .partial
                .keys()
                .copied()
                .filter(|&id| after(frame_id, id))
                .collect();
            for id in older {
                self.partial.remove(&id);
                events.push(Event::Lost { frame_id: id });
            }
            if let Some(l) = self.last {
                // Frames never seen at all between the last one and this one.
                let mut id = l.wrapping_add(1);
                while id != frame_id {
                    if !events.contains(&Event::Lost { frame_id: id }) {
                        events.push(Event::Lost { frame_id: id });
                    }
                    id = id.wrapping_add(1);
                    if events.len() > 64 {
                        break;
                    }
                }
            }
            self.last = Some(frame_id);
            events.push(Event::Frame {
                frame_id,
                keyframe: part.keyframe,
                recovery: part.recovery,
                data,
                spread: part.first.elapsed(),
            });
        }
        // Bound memory if a frame never completes.
        while self.partial.len() > 32 {
            let oldest = *self.partial.keys().next().expect("non-empty");
            self.partial.remove(&oldest);
        }
        events
    }

    /// The server says it sent up to `last_sent` and waits for acknowledgements. Frames up to
    /// there that are missing, and have had no packet for `quiet` (so are not still on
    /// their way), are lost: without this, losing the last frames before the server stops
    /// would leave both sides waiting.
    pub fn sync(&mut self, last_sent: u32, quiet: Duration) -> Vec<Event> {
        if self.last.is_some_and(|l| !after(last_sent, l)) {
            return Vec::new(); // everything up to there arrived
        }
        let pending: Vec<u32> = self
            .partial
            .keys()
            .copied()
            .filter(|&id| !after(id, last_sent))
            .collect();
        if pending
            .iter()
            .any(|id| self.partial[id].seen.elapsed() < quiet)
        {
            return Vec::new();
        }
        for id in &pending {
            self.partial.remove(id);
        }
        let mut events = Vec::new();
        let mut id = self.last.map_or(last_sent, |l| l.wrapping_add(1));
        loop {
            events.push(Event::Lost { frame_id: id });
            if id == last_sent || events.len() > 64 {
                break;
            }
            id = id.wrapping_add(1);
        }
        self.last = Some(last_sent);
        events
    }
}

pub fn sync_packet(last_frame_id: u32) -> Vec<u8> {
    let mut p = vec![SYNC];
    p.extend_from_slice(&last_frame_id.to_le_bytes());
    p
}

pub fn parse_sync(p: &[u8]) -> Option<u32> {
    if p.len() != 5 || p[0] != SYNC {
        return None;
    }
    Some(u32::from_le_bytes(p[1..5].try_into().ok()?))
}

/// [`sync_packet`] for the video of `monitor` (version 6): [`SYNC2`], or [`SYNC`] for
/// monitor 0.
pub fn sync_packet_monitor(monitor: u8, last_frame_id: u32) -> Vec<u8> {
    if monitor == 0 {
        return sync_packet(last_frame_id);
    }
    let mut p = vec![SYNC2, monitor];
    p.extend_from_slice(&last_frame_id.to_le_bytes());
    p
}

/// The monitor and last frame of a [`SYNC`] (monitor 0) or [`SYNC2`] packet.
pub fn parse_sync_monitor(p: &[u8]) -> Option<(u8, u32)> {
    match p {
        [SYNC, ..] => parse_sync(p).map(|last| (0, last)),
        [SYNC2, monitor, last @ ..] => Some((*monitor, u32::from_le_bytes(last.try_into().ok()?))),
        _ => None,
    }
}

/// Fills in the missing data shards of a block from the ones received (at least
/// `data` of them); false if that is not possible.
fn rebuild(data: usize, parity: usize, shards: &mut [Option<Vec<u8>>]) -> bool {
    if shards[..data].iter().all(Option::is_some) {
        return true;
    }
    if parity == 0 {
        return false;
    }
    let original = shards[..data]
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|s| (i, s)));
    let recovery = shards[data..]
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|s| (i, s)));
    match reed_solomon_simd::decode(data, parity, original, recovery) {
        Ok(restored) => {
            for (i, s) in restored {
                shards[i] = Some(s);
            }
            true
        }
        Err(_) => false,
    }
}

/// Audio packet: sequence number and PCM.
pub fn audio_packet(seq: u32, pcm: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(5 + pcm.len());
    p.push(AUDIO);
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(pcm);
    p
}

pub fn parse_audio(p: &[u8]) -> Option<(u32, &[u8])> {
    if p.len() < 5 || p[0] != AUDIO {
        return None;
    }
    Some((u32::from_le_bytes(p[1..5].try_into().ok()?), &p[5..]))
}

pub fn hello_packet(token: &[u8; 16]) -> Vec<u8> {
    let mut p = vec![HELLO];
    p.extend_from_slice(token);
    p
}

pub fn parse_hello(p: &[u8]) -> Option<[u8; 16]> {
    if p.len() != 17 || p[0] != HELLO {
        return None;
    }
    p[1..].try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize, seed: u32) -> Vec<u8> {
        (0..len).map(|i| (i as u32 * 31 + seed) as u8).collect()
    }

    fn frames_of(events: Vec<Event>) -> Vec<(u32, Vec<u8>)> {
        events
            .into_iter()
            .filter_map(|e| match e {
                Event::Frame { frame_id, data, .. } => Some((frame_id, data)),
                Event::Lost { .. } => None,
            })
            .collect()
    }

    #[test]
    fn whole_frames_in_any_order() {
        for len in [0, 1, 1100, 1101, 50_000, 500_000] {
            let data = frame(len, 3);
            let mut packets = packetize(5, true, &data, 20);
            packets.reverse();
            let mut r = Reassembler::default();
            let got: Vec<_> = packets.iter().flat_map(|p| frames_of(r.push(p))).collect();
            assert_eq!(got, vec![(5, data)], "len {len}");
        }
    }

    #[test]
    fn recovers_lost_shards() {
        let data = frame(100_000, 9);
        let packets = packetize(1, false, &data, 20);
        // 91 data + 19 parity shards: drop 15 of them, data shards included.
        let mut r = Reassembler::default();
        let got: Vec<_> = packets
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 7 != 0)
            .flat_map(|(_, p)| frames_of(r.push(p)))
            .collect();
        assert_eq!(got, vec![(1, data)]);
    }

    #[test]
    fn too_many_losses_reports_the_frame_lost() {
        let mut r = Reassembler::default();
        let a = packetize(1, true, &frame(20_000, 1), 10);
        let b = packetize(2, false, &frame(20_000, 2), 10);
        let c = packetize(3, false, &frame(20_000, 3), 10);
        for p in &a {
            r.push(p);
        }
        for p in b.iter().skip(5) {
            r.push(p);
        }
        let ev: Vec<Event> = c.iter().flat_map(|p| r.push(p)).collect();
        assert!(ev.contains(&Event::Lost { frame_id: 2 }));
        assert!(matches!(ev.last(), Some(Event::Frame { frame_id: 3, .. })));
        // Late packets of the lost frame are ignored.
        assert!(b.iter().flat_map(|p| r.push(p)).next().is_none());
    }

    #[test]
    fn no_parity() {
        let data = frame(3000, 4);
        let mut r = Reassembler::default();
        let got: Vec<_> = packetize(9, false, &data, 0)
            .iter()
            .flat_map(|p| frames_of(r.push(p)))
            .collect();
        assert_eq!(got, vec![(9, data)]);
    }

    #[test]
    fn recovery_flag_reaches_the_client() {
        let mut r = Reassembler::default();
        let events: Vec<_> = packetize_frame(4, false, true, &frame(5000, 7), 20)
            .iter()
            .flat_map(|p| r.push(p))
            .collect();
        assert!(matches!(
            events.as_slice(),
            [Event::Frame {
                frame_id: 4,
                keyframe: false,
                recovery: true,
                ..
            }]
        ));
    }

    #[test]
    fn sync_declares_the_last_frames_lost() {
        let mut r = Reassembler::default();
        for p in packetize(1, true, &[1; 3000], 20) {
            r.push(&p);
        }
        // Frame 2 never arrives; frame 3 only in part.
        let three = packetize(3, false, &[3; 5000], 0);
        r.push(&three[0]);
        // Still arriving: nothing yet.
        assert!(r.sync(3, Duration::from_secs(5)).is_empty());
        let ev = r.sync(3, Duration::ZERO);
        assert_eq!(
            ev,
            vec![Event::Lost { frame_id: 2 }, Event::Lost { frame_id: 3 }]
        );
        // Nothing missing any more.
        assert!(r.sync(3, Duration::ZERO).is_empty());
        assert_eq!(parse_sync(&sync_packet(7)), Some(7));
    }

    #[test]
    fn monitors_have_their_own_packets() {
        let data = frame(30_000, 5);
        let second = packetize_monitor(2, 1, true, false, &data, 20);
        assert!(second.iter().all(|p| p[0] == VIDEO2 && video_monitor(p) == Some(2)));
        // Monitor 0 looks like a single-monitor session.
        let first = packetize_monitor(0, 1, true, false, &data, 20);
        assert_eq!(first, packetize(1, true, &data, 20));
        assert!(first.iter().all(|p| video_monitor(p) == Some(0)));
        let mut r = Reassembler::default();
        let got: Vec<_> = second.iter().flat_map(|p| frames_of(r.push(p))).collect();
        assert_eq!(got, vec![(1, data)]);
        assert_eq!(video_monitor(&audio_packet(1, &[0; 4])), None);
        assert_eq!(parse_sync_monitor(&sync_packet_monitor(3, 9)), Some((3, 9)));
        assert_eq!(parse_sync_monitor(&sync_packet_monitor(0, 9)), Some((0, 9)));
        assert_eq!(sync_packet_monitor(0, 9), sync_packet(9));
        assert_eq!(parse_sync_monitor(&[SYNC2, 1, 0]), None);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release -p glidedesk-proto fec_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn fec_speed() {
        for len in [30_000usize, 150_000, 600_000] {
            let data: Vec<u8> = (0..len).map(|i| (i * 13) as u8).collect();
            let t = std::time::Instant::now();
            let mut packets = Vec::new();
            for _ in 0..20 {
                packets = packetize(1, false, &data, 20);
            }
            let enc = t.elapsed().as_secs_f64() * 1000.0 / 20.0;
            let t = std::time::Instant::now();
            for i in 0..20u32 {
                let mut r = Reassembler::default();
                // Lose every 8th packet so the decoder has to rebuild.
                for (k, p) in packets.iter().enumerate() {
                    if k % 8 != (i % 8) as usize {
                        r.push(p);
                    }
                }
            }
            let dec = t.elapsed().as_secs_f64() * 1000.0 / 20.0;
            println!(
                "{len} B: packetize {enc:.3} ms, rebuild {dec:.3} ms, {} packets",
                packets.len()
            );
        }
    }
}

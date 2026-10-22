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
//! ```

use std::collections::BTreeMap;

pub const VIDEO: u8 = 1;
pub const AUDIO: u8 = 2;
pub const HELLO: u8 = 3;

/// Shard payload: with the headers and encryption a packet stays under 1200 bytes, below
/// the MTU of any usual path (VPNs included).
pub const SHARD: usize = 1100;
/// Data shards per block (with the parity, a block stays within 255 shards: the shard
/// numbers are bytes).
pub const MAX_DATA_SHARDS: usize = 200;
const VIDEO_HEADER: usize = 1 + 4 + 1 + 1 + 1 + 1 + 1 + 1 + 4 + 4;
const FLAG_KEYFRAME: u8 = 1;

/// Splits one encoded frame into packets, with `fec_percent` parity per block (at least one
/// parity shard when it is not zero).
pub fn packetize(frame_id: u32, keyframe: bool, data: &[u8], fec_percent: u32) -> Vec<Vec<u8>> {
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
            let mut p = Vec::with_capacity(VIDEO_HEADER + shard.len());
            p.push(VIDEO);
            p.extend_from_slice(&frame_id.to_le_bytes());
            p.push(if keyframe { FLAG_KEYFRAME } else { 0 });
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
        data: Vec<u8>,
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
    len: usize,
    blocks: Vec<Option<Block>>,
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
    /// Feeds one video packet (plaintext, starting with [`VIDEO`]).
    pub fn push(&mut self, p: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        if p.len() < VIDEO_HEADER || p[0] != VIDEO {
            return events;
        }
        let frame_id = u32::from_le_bytes(p[1..5].try_into().unwrap());
        if self.last.is_some_and(|l| !after(frame_id, l)) {
            return events; // already delivered or given up
        }
        let (flags, bi, blocks, si, data_shards, parity) = (
            p[5],
            p[6] as usize,
            p[7] as usize,
            p[8] as usize,
            p[9] as usize,
            p[10] as usize,
        );
        let frame_len = u32::from_le_bytes(p[11..15].try_into().unwrap()) as usize;
        let block_len = u32::from_le_bytes(p[15..19].try_into().unwrap()) as usize;
        let shard = &p[VIDEO_HEADER..];
        if bi >= blocks || si >= data_shards + parity || data_shards == 0 {
            return events;
        }
        let part = self.partial.entry(frame_id).or_insert_with(|| Partial {
            keyframe: flags & FLAG_KEYFRAME != 0,
            len: frame_len,
            blocks: (0..blocks).map(|_| None).collect(),
        });
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
                data,
            });
        }
        // Bound memory if a frame never completes.
        while self.partial.len() > 32 {
            let oldest = *self.partial.keys().next().expect("non-empty");
            self.partial.remove(&oldest);
        }
        events
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

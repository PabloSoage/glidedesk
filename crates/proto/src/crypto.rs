//! UDP packet protection: ChaCha20-Poly1305 with the session key from the TLS control
//! channel.
//!
//! A datagram is `seq (u64 LE) || ciphertext || tag`. The top bit of `seq` is the direction,
//! so the two sides never use the same nonce with the shared key; the nonce is `seq` padded
//! to 96 bits. The receiver drops packets it has already seen ([`ReplayWindow`]).

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

const DIR_BIT: u64 = 1 << 63;
/// Bytes a sealed packet adds to its plaintext.
pub const OVERHEAD: usize = 8 + 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ToClient,
    ToServer,
}

impl Direction {
    fn bit(self) -> u64 {
        match self {
            Direction::ToClient => 0,
            Direction::ToServer => DIR_BIT,
        }
    }
}

pub struct PacketKey {
    cipher: ChaCha20Poly1305,
}

fn nonce(seq: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&seq.to_le_bytes());
    n.into()
}

impl PacketKey {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    /// Seals packet number `seq` (below 2^63) going in `dir`.
    pub fn seal(&self, dir: Direction, seq: u64, plaintext: &[u8]) -> Vec<u8> {
        let s = (seq & !DIR_BIT) | dir.bit();
        let ct = self
            .cipher
            .encrypt(&nonce(s), plaintext)
            .expect("ChaCha20-Poly1305 encryption does not fail");
        let mut out = Vec::with_capacity(8 + ct.len());
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&ct);
        out
    }

    /// Opens a packet that should be going in `dir`: its number and plaintext, or `None` if
    /// it was forged, damaged or going the other way.
    pub fn open(&self, dir: Direction, packet: &[u8]) -> Option<(u64, Vec<u8>)> {
        let (head, ct) = packet.split_at_checked(8)?;
        let s = u64::from_le_bytes(head.try_into().ok()?);
        if s & DIR_BIT != dir.bit() {
            return None;
        }
        let pt = self.cipher.decrypt(&nonce(s), ct).ok()?;
        Some((s & !DIR_BIT, pt))
    }
}

/// Accepts each packet number once, within the last 128.
#[derive(Default)]
pub struct ReplayWindow {
    highest: Option<u64>,
    seen: u128,
}

impl ReplayWindow {
    pub fn accept(&mut self, seq: u64) -> bool {
        let Some(hi) = self.highest else {
            self.highest = Some(seq);
            self.seen = 1;
            return true;
        };
        if seq > hi {
            let shift = seq - hi;
            self.seen = if shift >= 128 { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.highest = Some(seq);
            return true;
        }
        let back = hi - seq;
        if back >= 128 || self.seen & (1 << back) != 0 {
            return false;
        }
        self.seen |= 1 << back;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_reject() {
        let k = PacketKey::new(&[7; 32]);
        let p = k.seal(Direction::ToClient, 42, b"hello");
        assert_eq!(p.len(), 5 + OVERHEAD);
        assert_eq!(
            k.open(Direction::ToClient, &p),
            Some((42, b"hello".to_vec()))
        );
        // Wrong direction, tampering, wrong key.
        assert_eq!(k.open(Direction::ToServer, &p), None);
        let mut bad = p.clone();
        bad[10] ^= 1;
        assert_eq!(k.open(Direction::ToClient, &bad), None);
        assert_eq!(PacketKey::new(&[8; 32]).open(Direction::ToClient, &p), None);
    }

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(10));
        assert!(!w.accept(10));
        assert!(w.accept(12));
        assert!(w.accept(11));
        assert!(!w.accept(11));
        assert!(w.accept(500));
        assert!(!w.accept(12)); // too old
    }
}

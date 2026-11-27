//! UDP packet protection: ChaCha20-Poly1305 with the session key from the TLS control
//! channel.
//!
//! A datagram is `seq (u64 LE) || ciphertext || tag`. The top bit of `seq` is the direction,
//! so the two sides never use the same nonce with the shared key; the nonce is `seq` padded
//! to 96 bits. The receiver drops packets it has already seen ([`ReplayWindow`]).
//!
//! Two implementations of the same AEAD, interchangeable on the wire: RustCrypto's
//! `chacha20poly1305` (the default), or `ring`'s with the `ring` feature (assembly on x86-64
//! and ARMv8, several times faster where it matters, phones and the Termux server). The
//! `crypto_speed` test measures whichever is built.

#[cfg(not(feature = "ring"))]
mod imp {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    pub const NAME: &str = "RustCrypto chacha20poly1305";

    pub struct Cipher(ChaCha20Poly1305);

    impl Cipher {
        pub fn new(key: &[u8; 32]) -> Self {
            Self(ChaCha20Poly1305::new(Key::from_slice(key)))
        }

        /// Ciphertext and tag.
        pub fn seal(&self, nonce: [u8; 12], plaintext: &[u8]) -> Vec<u8> {
            self.0
                .encrypt(&Nonce::from(nonce), plaintext)
                .expect("ChaCha20-Poly1305 encryption does not fail")
        }

        pub fn open(&self, nonce: [u8; 12], sealed: &[u8]) -> Option<Vec<u8>> {
            self.0.decrypt(&Nonce::from(nonce), sealed).ok()
        }
    }
}

#[cfg(feature = "ring")]
mod imp {
    use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};

    pub const NAME: &str = "ring CHACHA20_POLY1305";

    pub struct Cipher(LessSafeKey);

    impl Cipher {
        pub fn new(key: &[u8; 32]) -> Self {
            Self(LessSafeKey::new(
                UnboundKey::new(&CHACHA20_POLY1305, key).expect("a 32-byte key"),
            ))
        }

        /// Ciphertext and tag.
        pub fn seal(&self, nonce: [u8; 12], plaintext: &[u8]) -> Vec<u8> {
            let mut buf = Vec::with_capacity(plaintext.len() + 16);
            buf.extend_from_slice(plaintext);
            self.0
                .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
                .expect("ChaCha20-Poly1305 encryption does not fail");
            buf
        }

        pub fn open(&self, nonce: [u8; 12], sealed: &[u8]) -> Option<Vec<u8>> {
            let mut buf = sealed.to_vec();
            let len = self
                .0
                .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
                .ok()?
                .len();
            buf.truncate(len);
            Some(buf)
        }
    }
}

/// Which implementation is built.
pub const IMPLEMENTATION: &str = imp::NAME;

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
    cipher: imp::Cipher,
}

fn nonce(seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&seq.to_le_bytes());
    n
}

impl PacketKey {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: imp::Cipher::new(key),
        }
    }

    /// Seals packet number `seq` (below 2^63) going in `dir`.
    pub fn seal(&self, dir: Direction, seq: u64, plaintext: &[u8]) -> Vec<u8> {
        let s = (seq & !DIR_BIT) | dir.bit();
        let ct = self.cipher.seal(nonce(s), plaintext);
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
        let pt = self.cipher.open(nonce(s), ct)?;
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

    /// ring's packets are RustCrypto's: peers built either way talk to each other.
    #[cfg(feature = "ring")]
    #[test]
    fn ring_matches_rustcrypto() {
        use chacha20poly1305::aead::{Aead, KeyInit};
        let key = [3u8; 32];
        let rc = chacha20poly1305::ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&key));
        let k = PacketKey::new(&key);
        let p = k.seal(Direction::ToServer, 9, b"same bytes");
        let s = u64::from_le_bytes(p[..8].try_into().unwrap());
        let n = chacha20poly1305::Nonce::from(nonce(s));
        assert_eq!(rc.decrypt(&n, &p[8..]).unwrap(), b"same bytes");
        assert_eq!(rc.encrypt(&n, &b"same bytes"[..]).unwrap(), p[8..].to_vec());
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

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release -p glidedesk-proto crypto_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn crypto_speed() {
        let k = PacketKey::new(&[42; 32]);
        let packet_size = 1200usize;
        let count = 100_000usize;
        let payload = vec![0x5au8; packet_size];
        let total_bytes = (packet_size * count) as f64;

        // Seal benchmark
        let t = std::time::Instant::now();
        let mut sealed = Vec::with_capacity(count);
        for seq in 0..count as u64 {
            sealed.push(k.seal(Direction::ToClient, seq, &payload));
        }
        let seal_secs = t.elapsed().as_secs_f64();
        let seal_gbps = (total_bytes / seal_secs) / 1_000_000_000.0;
        let seal_us = (seal_secs * 1_000_000.0) / count as f64;

        // Open benchmark
        let t = std::time::Instant::now();
        let mut opened_count = 0usize;
        for p in &sealed {
            if k.open(Direction::ToClient, p).is_some() {
                opened_count += 1;
            }
        }
        let open_secs = t.elapsed().as_secs_f64();
        let open_gbps = (total_bytes / open_secs) / 1_000_000_000.0;
        let open_us = (open_secs * 1_000_000.0) / count as f64;

        assert_eq!(opened_count, count);
        println!(
            "{IMPLEMENTATION} (1200 B packets, {count} iters):\n  Seal: {seal_gbps:.2} GB/s ({seal_us:.2} \u{03bc}s/pkt)\n  Open: {open_gbps:.2} GB/s ({open_us:.2} \u{03bc}s/pkt)"
        );
    }
}

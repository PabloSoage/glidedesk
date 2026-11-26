//! The Moonlight control stream: ENet over UDP (port 47999), every message encrypted with
//! AES-128-GCM under the key from `/launch` (Sunshine `src/stream.cpp`
//! `controlBroadcastThread`, moonlight-common-c `ControlStream.c`, commits in THIRD_PARTY.md).
//!
//! Encrypted message: type 0x0001 (LE u16), length (LE u16: what follows), sequence number
//! (LE u32), GCM tag (16 bytes), then the ciphertext of type (LE u16), payload length (LE u16)
//! and payload. The 12-byte IV is the sequence number (LE) followed by zeros and two bytes
//! naming the direction: "CC" from the client, "HC" from the host.
//!
//! What the host acts on: keyframe requests, reference frame invalidations (the encoder stops
//! referencing the lost frames, or sends a keyframe if it cannot), keyboard, mouse and gamepads
//! ([`super::input`]) and the end of the connection. What the host sends: gamepad rumble.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use rusty_enet::{Event, Host, HostSettings, Packet, PeerID};
use tracing::{debug, info, warn};

use super::input::MoonlightInput;
use super::stream::Stream;
use crate::input::InputForwarder;

/// ENet channels Moonlight opens (moonlight-common-c `CTRL_CHANNEL_COUNT`).
const CHANNELS: usize = 0x30;
const TAG: usize = 16;

/// Control message types (moonlight-common-c `packetTypesGen7Enc`).
const ENCRYPTED: u16 = 0x0001;
const INVALIDATE_REFS: u16 = 0x0301;
const REQUEST_IDR: u16 = 0x0302;
const PERIODIC_PING: u16 = 0x0200;
const LOSS_STATS: u16 = 0x0201;
const INPUT: u16 = 0x0206;
const START_A: u16 = 0x0305;
const START_B: u16 = 0x0307;
const RUMBLE: u16 = 0x010b;

/// Runs the control stream on its own thread (ENet polls a non-blocking socket).
pub fn spawn(stream: Arc<Stream>, bind: SocketAddr, input: Option<InputForwarder>) -> Result<()> {
    let socket = UdpSocket::bind(bind).with_context(|| format!("binding UDP {bind}"))?;
    let host = Host::new(
        socket,
        HostSettings {
            peer_limit: 4,
            channel_limit: CHANNELS,
            ..HostSettings::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("ENet host: {e:?}"))?;
    std::thread::Builder::new()
        .name("gs-control".into())
        .spawn(move || {
            // Rumble from the session's gamepads, to send from this thread.
            let (tx, rx) = std::sync::mpsc::channel();
            let rumble: super::gamepad::Rumble = Arc::new(move |n, low, high| {
                let _ = tx.send((n, low, high));
            });
            let mut input = input.map(|i| MoonlightInput::new(i, rumble));
            run(host, &stream, &mut input, &rx)
        })?;
    Ok(())
}

/// The connected client and the sequence number of the host's next message.
#[derive(Default)]
struct Client {
    peer: Option<PeerID>,
    seq: u32,
}

fn run(
    mut host: Host<UdpSocket>,
    stream: &Stream,
    input: &mut Option<MoonlightInput>,
    rumble: &std::sync::mpsc::Receiver<(u16, u16, u16)>,
) {
    let mut client = Client::default();
    loop {
        while let Ok((n, low, high)) = rumble.try_recv() {
            // Sunshine's control_rumble_t: a constant, controller, low and high motor (LE).
            let mut payload = 0x00c0_ffee_u32.to_le_bytes().to_vec();
            for v in [n, low, high] {
                payload.extend_from_slice(&v.to_le_bytes());
            }
            send(&mut host, stream, &mut client, RUMBLE, &payload);
        }
        match host.service() {
            Ok(Some(event)) => on_event(event, stream, input, &mut client),
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            // Windows reports ICMP errors for earlier sends on the next receive.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(e) => {
                warn!(%e, "Moonlight control socket failed");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Sends a message to the connected client, encrypted.
fn send(
    host: &mut Host<UdpSocket>,
    stream: &Stream,
    client: &mut Client,
    kind: u16,
    payload: &[u8],
) {
    let (Some(peer), Some(launch)) = (client.peer, stream.launch()) else {
        return;
    };
    let data = seal(&launch.key, client.seq, *b"HC", kind, payload);
    client.seq = client.seq.wrapping_add(1);
    if let Some(p) = host.get_peer_mut(peer)
        && let Err(e) = p.send(0, &Packet::reliable(data.as_slice()))
    {
        debug!(?e, "Sending a control message failed");
    }
}

fn on_event(
    event: Event<'_, UdpSocket>,
    stream: &Stream,
    input: &mut Option<MoonlightInput>,
    client: &mut Client,
) {
    match event {
        Event::Connect { peer, data } => {
            let expected = stream.launch().map(|l| l.connect_data);
            if expected != Some(data) {
                warn!(addr = ?peer.address(), "Control connection for no launched app");
                peer.disconnect_now(0);
                return;
            }
            // As moonlight-common-c sets it for itself: 10 s without an answer ends it.
            peer.set_timeout(2, 10_000, 10_000);
            *client = Client {
                peer: Some(peer.id()),
                seq: 0,
            };
            info!(addr = ?peer.address(), "Moonlight control stream connected");
        }
        Event::Disconnect { peer, .. } => {
            info!(addr = ?peer.address(), "Moonlight control stream ended");
            if client.peer == Some(peer.id()) {
                client.peer = None;
            }
            stream.end_video();
            if let Some(i) = input {
                i.release_all();
            }
        }
        Event::Receive { packet, .. } => {
            let Some(launch) = stream.launch() else {
                return;
            };
            match open(&launch.key, packet.data()) {
                Some((kind, payload)) => on_message(kind, &payload, stream, input),
                None => debug!(len = packet.data().len(), "Dropped a control message"),
            }
        }
    }
}

fn on_message(kind: u16, payload: &[u8], stream: &Stream, input: &mut Option<MoonlightInput>) {
    match kind {
        REQUEST_IDR => {
            debug!("Moonlight asked for a keyframe");
            stream.request_keyframe();
        }
        // SS_RFI_REQUEST: first lost frame (LE u32), reserved, last lost frame.
        INVALIDATE_REFS => match payload.get(..4) {
            Some(b) => stream.invalidate_from(u32::from_le_bytes(b.try_into().unwrap())),
            None => stream.request_keyframe(),
        },
        INPUT => {
            if let Some(i) = input {
                i.handle(payload, stream.desktop_size());
            }
        }
        PERIODIC_PING | LOSS_STATS | START_A | START_B => {}
        other => debug!(
            kind = format!("{other:#06x}"),
            len = payload.len(),
            "Control message"
        ),
    }
}

fn nonce(seq: u32, direction: [u8; 2]) -> Nonce {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10..].copy_from_slice(&direction);
    Nonce::assume_unique_for_key(iv)
}

/// Decrypts a message from the client: its type and payload.
pub fn open(key: &[u8; 16], data: &[u8]) -> Option<(u16, Vec<u8>)> {
    if data.len() < 8 + TAG + 4 || u16::from_le_bytes([data[0], data[1]]) != ENCRYPTED {
        return None;
    }
    let length = usize::from(u16::from_le_bytes([data[2], data[3]]));
    if 4 + length != data.len() {
        return None;
    }
    let seq = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, key).ok()?);
    // ring wants the tag after the ciphertext.
    let mut sealed = data[8 + TAG..].to_vec();
    sealed.extend_from_slice(&data[8..8 + TAG]);
    let plain = key
        .open_in_place(nonce(seq, *b"CC"), Aad::empty(), &mut sealed)
        .ok()?;
    if plain.len() < 4 {
        return None;
    }
    let kind = u16::from_le_bytes([plain[0], plain[1]]);
    let len = usize::from(u16::from_le_bytes([plain[2], plain[3]]));
    let payload = plain.get(4..4 + len)?.to_vec();
    Some((kind, payload))
}

/// Encrypts a message the way [`open`] reads it, from either side.
fn seal(key: &[u8; 16], seq: u32, direction: [u8; 2], kind: u16, payload: &[u8]) -> Vec<u8> {
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, key).unwrap());
    let mut plain = kind.to_le_bytes().to_vec();
    plain.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    plain.extend_from_slice(payload);
    let tag = key
        .seal_in_place_separate_tag(nonce(seq, direction), Aad::empty(), &mut plain)
        .unwrap();
    let mut out = ENCRYPTED.to_le_bytes().to_vec();
    out.extend_from_slice(&((4 + TAG + plain.len()) as u16).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(tag.as_ref());
    out.extend_from_slice(&plain);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_decrypt() {
        let key = [7u8; 16];
        let msg = seal(&key, 42, *b"CC", REQUEST_IDR, &[0, 0]);
        assert_eq!(open(&key, &msg), Some((REQUEST_IDR, vec![0, 0])));
        // The host's direction, another key or a flipped bit: refused.
        assert_eq!(
            open(&key, &seal(&key, 42, *b"HC", REQUEST_IDR, &[0, 0])),
            None
        );
        assert_eq!(open(&[8u8; 16], &msg), None);
        let mut bad = msg.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(open(&key, &bad), None);
        assert_eq!(open(&key, &msg[..msg.len() - 1]), None);
    }
}

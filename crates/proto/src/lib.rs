//! glidedesk native protocol (see the docs repo, ADR-0004).
//!
//! - Control over TCP + TLS: [`ClientMsg`] and [`ServerMsg`], length-prefixed `postcard`
//!   ([`write_msg`], [`read_msg`]).
//! - Media over UDP on the same port: every datagram is sealed with ChaCha20-Poly1305
//!   ([`crypto`]); video frames are split into shards with Reed-Solomon parity ([`fec`]).

pub mod crypto;
pub mod fec;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped on any incompatible change.
pub const VERSION: u32 = 1;
/// Default TCP and UDP port.
pub const DEFAULT_PORT: u16 = 3391;
/// Largest control message accepted (a cursor image or clipboard text fits easily).
pub const MAX_MSG: usize = 8 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute position in desktop pixels.
    MouseMove { x: u16, y: u16 },
    /// 1 left, 2 right, 3 middle, 4 back, 5 forward.
    MouseButton { button: u8, down: bool },
    /// 120 units per notch; positive is up / right.
    Wheel { vertical: bool, units: i16 },
    /// PC/AT set-1 scan code, `extended` for the E0 prefix.
    Key {
        scancode: u8,
        extended: bool,
        down: bool,
    },
    /// Text that has no key (IME, soft keyboards).
    Unicode { ch: char, down: bool },
    /// The client window lost the focus: release everything held.
    ReleaseAll,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        version: u32,
        user: String,
        password: String,
        width: u16,
        height: u16,
        /// Codecs the client decodes, preferred first.
        codecs: Vec<Codec>,
        /// Refresh rate of the client's display.
        refresh: u32,
    },
    Input(InputEvent),
    Resize {
        width: u16,
        height: u16,
    },
    Clipboard(String),
    /// A frame was decoded (for pacing).
    FrameAck {
        frame_id: u32,
    },
    /// A frame could not be recovered: send an IDR.
    RequestKeyframe,
    Ping {
        t_us: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cursor {
    Hidden,
    /// RGBA, rows top-down, straight alpha.
    Shape {
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        rgba: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        /// Key for the UDP packets of this session (both directions).
        key: [u8; 32],
        /// Sent back by the client in its UDP hello, so the server learns its address.
        token: [u8; 16],
        codec: Codec,
        width: u16,
        height: u16,
        /// PCM 48 kHz stereo S16 in [`fec::AUDIO`] packets.
        audio: bool,
    },
    Denied(String),
    /// The desktop size changed; frames that follow have it.
    Size {
        width: u16,
        height: u16,
    },
    Cursor(Cursor),
    Clipboard(String),
    Pong {
        t_us: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding: {0}")]
    Encoding(#[from] postcard::Error),
    #[error("message of {0} bytes is too large")]
    TooLarge(usize),
}

/// Writes one length-prefixed message.
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> Result<(), Error> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_MSG {
        return Err(Error::TooLarge(body.len()));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one length-prefixed message.
pub async fn read_msg<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> Result<T, Error> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MSG {
        return Err(Error::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

/// Fresh random bytes (session keys, tokens).
pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random source failed");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_round_trip() {
        let msgs = vec![
            ClientMsg::Hello {
                version: VERSION,
                user: "u".into(),
                password: "p".into(),
                width: 1920,
                height: 1080,
                codecs: vec![Codec::Hevc, Codec::H264],
                refresh: 144,
            },
            ClientMsg::Input(InputEvent::Key {
                scancode: 0x1e,
                extended: false,
                down: true,
            }),
            ClientMsg::FrameAck { frame_id: 7 },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_msg(&mut buf, m).await.unwrap();
        }
        let mut r = buf.as_slice();
        for m in &msgs {
            let got: ClientMsg = read_msg(&mut r).await.unwrap();
            assert_eq!(&got, m);
        }
    }
}

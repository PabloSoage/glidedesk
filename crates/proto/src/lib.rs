//! glidedesk native protocol (see the docs repo, ADR-0004).
//!
//! - Control over TCP + TLS: [`ClientMsg`] and [`ServerMsg`], length-prefixed `postcard`
//!   ([`write_msg`], [`read_msg`]).
//! - Media over UDP on the same port: every datagram is sealed with ChaCha20-Poly1305
//!   ([`crypto`]); video frames are split into shards with Reed-Solomon parity ([`fec`]).

pub mod crypto;
pub mod fec;
pub mod files;
pub mod link;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped on any change; servers also accept clients from [`MIN_VERSION`] on.
///
/// - 2: [`ClientMsg::InvalidateFrames`] and the recovery flag on video packets.
/// - 3: apps: [`ServerMsg::Apps`], [`ClientMsg::OpenApp`], [`ServerMsg::AppExited`].
/// - 4: [`ClientMsg::LinkReport`]: the server adapts parity, pacing and bitrate to losses;
///   files on the clipboard ([`files`]: `Files`, `FileRead`, `FileData` both ways).
/// - 5: [`ClientMsg::Formats`] (HEVC 4:4:4), sent by the client right after
///   [`ServerMsg::Welcome`].
/// - 6: several monitors: [`ClientMsg::Monitors`] asks for a layout, [`ServerMsg::Layout`]
///   says what the server set up, [`ServerMsg::MonitorSize`]; the video of the monitors
///   after the first comes in [`fec::VIDEO2`] packets and is acknowledged with
///   [`ClientMsg::MonitorVideo`].
///
/// A client learns the server's version from the login itself: a server refuses versions it
/// does not know, and the client then retries with the newest one the refusal names (so the
/// version of an accepted `Hello` is one both speak).
pub const VERSION: u32 = 6;
/// Oldest client version a server accepts (it then uses only what that version knows).
pub const MIN_VERSION: u32 = 1;
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

/// One monitor of a layout (version 6), in desktop pixels.
///
/// From the client ([`ClientMsg::Monitors`]): its screens, positioned relative to each other
/// as its system places them (any origin). From the server ([`ServerMsg::Layout`]): the
/// monitors of the remote desktop, the top left corner of their bounding box at 0,0; pointer
/// positions ([`InputEvent::MouseMove`]) are in that space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    /// Chosen by the client, kept by the server. Monitor 0 is the one of `Hello`'s size:
    /// its video uses the messages and packets of a single-monitor session.
    pub id: u8,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    /// Hz.
    pub refresh: u32,
    pub primary: bool,
}

/// (Version 6.) What a client says about the video of one monitor other than monitor 0
/// ([`ClientMsg::MonitorVideo`]); monitor 0 keeps the plain messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoMsg {
    FrameAck { frame_id: u32 },
    RequestKeyframe,
    InvalidateFrames { first: u32 },
}

impl VideoMsg {
    /// The message for the video of `monitor`: the single-monitor message for monitor 0.
    pub fn for_monitor(self, monitor: u8) -> ClientMsg {
        match (monitor, self) {
            (0, Self::FrameAck { frame_id }) => ClientMsg::FrameAck { frame_id },
            (0, Self::RequestKeyframe) => ClientMsg::RequestKeyframe,
            (0, Self::InvalidateFrames { first }) => ClientMsg::InvalidateFrames { first },
            (monitor, msg) => ClientMsg::MonitorVideo { monitor, msg },
        }
    }
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
    /// (Version 2.) Frames from `first` on could not be recovered: stop referencing them
    /// instead of sending an IDR. The client skips frames until one with the recovery flag
    /// (or a keyframe) arrives; the server answers with a keyframe if it cannot.
    InvalidateFrames {
        first: u32,
    },
    /// (Version 3.) Open this app (a name from [`ServerMsg::Apps`]) instead of the whole
    /// desktop. A running one is reused; when it exits the server sends
    /// [`ServerMsg::AppExited`].
    OpenApp(String),
    /// (Version 4.) About once a second: UDP packets received and lost since the last
    /// report, counted from their sequence numbers ([`link::LossCounter`]).
    LinkReport { received: u32, lost: u32 },
    /// (Version 4.) The client copied files ([`files`]); a new `id` for each copy.
    Files { id: u32, entries: Vec<FileEntry> },
    /// (Version 4.) Read a range of the server's copied files.
    FileRead(FileRead),
    /// (Version 4.) The answer to a [`ServerMsg::FileRead`].
    FileData(FileData),
    /// (Version 5, right after [`ServerMsg::Welcome`].) Picture formats the client decodes
    /// and shows besides 4:2:0. With `yuv444` and HEVC, the server may send 4:4:4 (Range
    /// Extensions, `chroma_format_idc` 3, full-range BT.709) from its next keyframe on; the
    /// stream itself says which it is. Only sent when the client wants something.
    Formats { yuv444: bool },
    /// (Version 6.) Show the desktop on these monitors (monitor 0 among them, with the size
    /// of `Hello` or of the last `Resize`). The server answers with [`ServerMsg::Layout`],
    /// which may have fewer monitors than asked for (one, if it cannot do more). A layout
    /// of one monitor goes back to a single-monitor session.
    Monitors(Vec<Monitor>),
    /// (Version 6.) Acknowledgement, keyframe request or invalidation for the video of a
    /// monitor other than 0 (see [`VideoMsg::for_monitor`]).
    MonitorVideo { monitor: u8, msg: VideoMsg },
}

/// One copied file or folder ([`files`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative to the copied items, `/`-separated (the first part is the copied item).
    pub path: String,
    /// Bytes (0 for a folder).
    pub size: u64,
    pub dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRead {
    /// The `id` of the file list.
    pub id: u32,
    pub index: u32,
    pub offset: u64,
    pub len: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileData {
    pub id: u32,
    pub index: u32,
    pub offset: u64,
    /// Fewer bytes than asked for only at the end of the file.
    pub data: Vec<u8>,
    /// The read failed (the list was replaced, the file is gone).
    pub error: Option<String>,
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
    /// (Version 3, right after [`ServerMsg::Welcome`].) The apps the client can open; the
    /// first is the desktop.
    Apps(Vec<String>),
    /// (Version 3.) The app the client opened is running.
    AppStarted(String),
    /// (Version 3.) The app the client opened ended (or could not start: the reason).
    AppExited {
        name: String,
        reason: Option<String>,
    },
    /// (Version 4.) Files were copied in the session ([`files`]).
    Files { id: u32, entries: Vec<FileEntry> },
    /// (Version 4.) Read a range of the client's copied files.
    FileRead(FileRead),
    /// (Version 4.) The answer to a [`ClientMsg::FileRead`].
    FileData(FileData),
    /// (Version 6, to clients that sent [`ClientMsg::Monitors`].) The monitors of the
    /// remote desktop, after each change; the video of those other than monitor 0 comes in
    /// [`fec::VIDEO2`] packets.
    Layout(Vec<Monitor>),
    /// (Version 6.) Monitor `monitor` (not 0, which has [`ServerMsg::Size`]) has a new
    /// size; its frames that follow have it, starting with a keyframe.
    MonitorSize {
        monitor: u8,
        width: u16,
        height: u16,
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
            ClientMsg::Formats { yuv444: true },
            ClientMsg::Monitors(vec![Monitor {
                id: 1,
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
                refresh: 60,
                primary: false,
            }]),
            VideoMsg::FrameAck { frame_id: 3 }.for_monitor(2),
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

    #[test]
    fn monitor_zero_keeps_the_single_monitor_messages() {
        assert_eq!(
            VideoMsg::FrameAck { frame_id: 9 }.for_monitor(0),
            ClientMsg::FrameAck { frame_id: 9 }
        );
        assert_eq!(
            VideoMsg::RequestKeyframe.for_monitor(0),
            ClientMsg::RequestKeyframe
        );
        assert_eq!(
            VideoMsg::InvalidateFrames { first: 4 }.for_monitor(1),
            ClientMsg::MonitorVideo {
                monitor: 1,
                msg: VideoMsg::InvalidateFrames { first: 4 }
            }
        );
    }

    /// Older peers decode the enums by variant index: the new variants must come last.
    #[test]
    fn new_variants_come_after_the_old_ones() {
        let index = |m: &ClientMsg| postcard::to_stdvec(m).unwrap()[0];
        assert_eq!(index(&ClientMsg::Monitors(Vec::new())), 14);
        assert_eq!(
            postcard::to_stdvec(&ServerMsg::Layout(Vec::new())).unwrap()[0],
            12
        );
    }
}

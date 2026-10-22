//! An existing X11 display as the session: Termux's termux-x11 (with XFCE or similar), or a
//! Linux desktop running on X.
//!
//! - Frames: the root window read with GetImage whenever the DAMAGE extension reports a
//!   change, at most `fps` times a second, BGRX in system memory (32-bit pixels).
//! - Pointer: the shape from XFIXES, for the client to draw.
//! - Input: XTEST fake events (key codes are evdev + 8, as on X servers using evdev/xkb).
//!
//! The size is the display's: a client's resize request changes nothing here, the frames
//! carry the real size and the front ends tell the client.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::xfixes::{ConnectionExt as _, CursorNotifyMask};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::frame::{CursorEvent, CursorShape, Frame, FrameData, FrameReceiver, Source};
use crate::input::{Input, InputBackend};

pub struct X11Session {
    pub size: (u32, u32),
    pub source: FixedSize,
    pub frames: FrameReceiver,
    pub cursor: watch::Receiver<Option<CursorEvent>>,
    pub input: XTestInput,
}

/// The display's size does not follow the client.
pub struct FixedSize;

impl Source for FixedSize {
    fn resize(&self, width: u32, height: u32) {
        debug!(width, height, "Resize asked; an X display keeps its size");
    }
}

/// Connects to `display` (or $DISPLAY). Besides the usual lookup: X servers that only
/// listen on the abstract socket, and Termux, which keeps the socket under $TMPDIR.
fn connect(display: Option<&str>) -> Result<RustConnection> {
    let e = match RustConnection::connect(display) {
        Ok((c, _)) => return Ok(c),
        Err(e) => e,
    };
    let name = display
        .map(str::to_owned)
        .or_else(|| std::env::var("DISPLAY").ok())
        .unwrap_or_else(|| ":0".into());
    let n: String = name
        .trim_start_matches(|c| c != ':')
        .trim_start_matches(':')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt;
        let path = format!("/tmp/.X11-unix/X{n}");
        if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(path.as_bytes())
            && let Ok(s) = std::os::unix::net::UnixStream::connect_addr(&addr)
        {
            let (stream, _) = DefaultStream::from_unix_stream(s)?;
            return Ok(RustConnection::connect_to_stream(stream, 0)?);
        }
    }
    if let Ok(dir) = std::env::var("TMPDIR") {
        let path = format!("{dir}/.X11-unix/X{n}");
        if let Ok(s) = std::os::unix::net::UnixStream::connect(&path) {
            let (stream, _) = DefaultStream::from_unix_stream(s)?;
            return Ok(RustConnection::connect_to_stream(stream, 0)?);
        }
    }
    Err(e).with_context(|| format!("connecting to the X display {name}"))
}

pub fn start(display: Option<&str>, fps: u32) -> Result<X11Session> {
    let conn = connect(display)?;
    let screen = &conn.setup().roots[0];
    let (root, w, h, depth) = (
        screen.root,
        u32::from(screen.width_in_pixels),
        u32::from(screen.height_in_pixels),
        screen.root_depth,
    );
    if depth != 24 && depth != 32 {
        bail!("the X display has depth {depth}; 24 or 32 is needed");
    }
    conn.damage_query_version(1, 1)?
        .reply()
        .context("the X server has no DAMAGE extension")?;
    conn.xfixes_query_version(4, 0)?
        .reply()
        .context("the X server has no XFIXES extension")?;
    let damage = conn.generate_id()?;
    conn.damage_create(damage, root, ReportLevel::NON_EMPTY)?;
    conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)?;
    conn.flush()?;
    info!(w, h, depth, "X display ready");

    let (tx, rx) = crate::frame::latest_frame();
    let (cursor_tx, cursor_rx) = watch::channel(None);
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    std::thread::Builder::new()
        .name("x11-capture".into())
        .spawn(move || {
            let mut dirty = true;
            let mut cursor_dirty = true;
            let mut last = Instant::now() - interval;
            loop {
                loop {
                    match conn.poll_for_event() {
                        Ok(Some(Event::DamageNotify(_))) => dirty = true,
                        Ok(Some(Event::XfixesCursorNotify(_))) => cursor_dirty = true,
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(e) => {
                            warn!(%e, "X connection lost");
                            return;
                        }
                    }
                }
                if cursor_dirty {
                    cursor_dirty = false;
                    if let Some(c) = cursor_shape(&conn) {
                        let _ = cursor_tx.send(Some(c));
                    }
                }
                if !dirty || last.elapsed() < interval {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                dirty = false;
                last = Instant::now();
                // Clear the damage first: changes during the read show up as new damage.
                let _ = conn.damage_subtract(damage, x11rb::NONE, x11rb::NONE);
                let image = conn
                    .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, w as u16, h as u16, !0)
                    .ok()
                    .and_then(|c| c.reply().ok());
                let Some(image) = image else {
                    warn!("GetImage failed");
                    continue;
                };
                tx.send(Frame {
                    width: w,
                    height: h,
                    data: FrameData::Cpu {
                        data: image.data,
                        stride: w * 4,
                    },
                    damage: None,
                    captured: last,
                });
            }
        })?;

    Ok(X11Session {
        size: (w, h),
        source: FixedSize,
        frames: rx,
        cursor: cursor_rx,
        input: XTestInput {
            conn: Mutex::new(connect(display)?),
            root,
        },
    })
}

/// The current pointer image (XFIXES gives premultiplied ARGB, rows top-down) in the RDP
/// layout: BGRA, rows bottom-up, straight alpha.
fn cursor_shape(conn: &RustConnection) -> Option<CursorEvent> {
    let c = conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
    if c.width == 0 || c.height == 0 {
        return Some(CursorEvent::Hidden);
    }
    let w = usize::from(c.width);
    let mut bgra = Vec::with_capacity(c.cursor_image.len() * 4);
    for row in c.cursor_image.chunks(w).rev() {
        for &p in row {
            let [b, g, r, a] = p.to_le_bytes();
            let un = |v: u8| {
                if a == 0 {
                    0
                } else {
                    (u16::from(v) * 255 / u16::from(a)).min(255) as u8
                }
            };
            bgra.extend_from_slice(&[un(b), un(g), un(r), a]);
        }
    }
    Some(CursorEvent::Shape(CursorShape {
        width: c.width,
        height: c.height,
        hot_x: c.xhot,
        hot_y: c.yhot,
        bgra_bottom_up: bgra,
    }))
}

/// Input through XTEST on the display's root window.
pub struct XTestInput {
    conn: Mutex<RustConnection>,
    root: u32,
}

// Core X event types for XTEST FakeInput.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

impl XTestInput {
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
        conn.flush()?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl InputBackend for XTestInput {
    async fn apply(&self, ev: &Input) -> Result<()> {
        match *ev {
            // X key codes are the evdev codes + 8.
            Input::Key(code, down) => {
                let kind = if down { KEY_PRESS } else { KEY_RELEASE };
                self.fake(kind, (code + 8).min(255) as u8, 0, 0)
            }
            Input::Keysym(sym, _) => {
                debug!(sym, "Text without a key is not sent to X displays");
                Ok(())
            }
            Input::Motion(x, y) => self.fake(MOTION_NOTIFY, 0, x as i16, y as i16),
            Input::Button(b, down) => {
                // BTN_LEFT, RIGHT, MIDDLE, SIDE, EXTRA to X buttons 1, 3, 2, 8, 9.
                let button = match b {
                    0x110 => 1,
                    0x111 => 3,
                    0x112 => 2,
                    0x113 => 8,
                    0x114 => 9,
                    _ => return Ok(()),
                };
                let kind = if down { BUTTON_PRESS } else { BUTTON_RELEASE };
                self.fake(kind, button, 0, 0)
            }
            Input::Scroll { axis, steps } => {
                // Buttons 4/5: up/down, 6/7: left/right (positive steps: down, right).
                let button = match (axis, steps > 0) {
                    (0, true) => 5,
                    (0, false) => 4,
                    (_, true) => 7,
                    (_, false) => 6,
                };
                for _ in 0..steps.unsigned_abs() {
                    self.fake(BUTTON_PRESS, button, 0, 0)?;
                    self.fake(BUTTON_RELEASE, button, 0, 0)?;
                }
                Ok(())
            }
        }
    }
}

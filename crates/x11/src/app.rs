use std::io::Write;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use glidedesk_client::gfx::{SharedSink, VideoCodec, VideoSink};
use glidedesk_client::session::Input;
use openh264::formats::YUVSource;
use rayon::prelude::*;
use tracing::{debug, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, PropMode,
    WindowClass,
};
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;

#[derive(Parser, Clone)]
#[command(version, about = "glidedesk minimal X11 client")]
struct Args {
    /// Server host name or address.
    host: String,
    #[arg(long)]
    user: String,
    #[arg(long, env = "GLIDEDESK_PASSWORD", hide_env_values = true)]
    password: String,
    /// Default 3391 (native) or 3389 (RDP).
    #[arg(long)]
    port: Option<u16>,
    /// Use RDP instead of the native protocol.
    #[arg(long)]
    rdp: bool,
    /// Window width; 0 is the whole screen (termux-x11 has no window manager to size it).
    #[arg(long, default_value_t = 0)]
    width: u16,
    #[arg(long, default_value_t = 0)]
    height: u16,
    /// No sound.
    #[arg(long)]
    mute: bool,
}

/// Connects to $DISPLAY. Besides the usual lookup: Linux X servers that only listen on the
/// abstract socket (x11rb does not try it), and Termux, which keeps the socket under
/// $TMPDIR (there is no /tmp).
fn connect() -> Result<RustConnection> {
    let e = match RustConnection::connect(None) {
        Ok((c, _)) => return Ok(c),
        Err(e) => e,
    };
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let n: String = display
        .trim_start_matches(|c| c != ':')
        .trim_start_matches(':')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let mut tried = Vec::new();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt;
        let name = format!("/tmp/.X11-unix/X{n}");
        if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            && let Ok(s) = std::os::unix::net::UnixStream::connect_addr(&addr)
        {
            let (stream, _) = DefaultStream::from_unix_stream(s)?;
            return Ok(RustConnection::connect_to_stream(stream, 0)?);
        }
        tried.push(format!("@{name}"));
    }
    if let Ok(dir) = std::env::var("TMPDIR") {
        let path = format!("{dir}/.X11-unix/X{n}");
        if let Ok(s) = std::os::unix::net::UnixStream::connect(&path) {
            let (stream, _) = DefaultStream::from_unix_stream(s)?;
            return Ok(RustConnection::connect_to_stream(stream, 0)?);
        }
        tried.push(path);
    }
    Err(e).with_context(|| format!("connecting to the X display (also tried {tried:?})"))
}

/// Linux evdev key code to PC/AT set-1 scan code (the inverse of the server's table).
fn evdev_to_scancode(code: u32) -> Option<(u8, bool)> {
    Some(match code {
        1..=88 => (code as u8, false),
        96 => (0x1c, true),  // KP Enter
        97 => (0x1d, true),  // Right Ctrl
        98 => (0x35, true),  // KP /
        99 => (0x37, true),  // Print Screen
        100 => (0x38, true), // AltGr
        102 => (0x47, true), // Home
        103 => (0x48, true), // Up
        104 => (0x49, true), // Page Up
        105 => (0x4b, true), // Left
        106 => (0x4d, true), // Right
        107 => (0x4f, true), // End
        108 => (0x50, true), // Down
        109 => (0x51, true), // Page Down
        110 => (0x52, true), // Insert
        111 => (0x53, true), // Delete
        119 => (0x46, true), // Pause
        125 => (0x5b, true), // Left Super
        126 => (0x5c, true), // Right Super
        127 => (0x5d, true), // Menu
        _ => return None,
    })
}

/// Planar YUV 4:2:0 (full-range BT.709) to BGRX rows of `width` pixels.
fn yuv_to_bgrx(yuv: &impl YUVSource, out: &mut Vec<u8>) {
    let (w, h) = yuv.dimensions();
    let (ys, us, vs) = yuv.strides();
    let (y, u, v) = (yuv.y(), yuv.u(), yuv.v());
    out.resize(w * h * 4, 0);
    out.par_chunks_mut(w * 4)
        .enumerate()
        .for_each(|(row, line)| {
            let (yr, cr) = (&y[row * ys..], row / 2);
            let (ur, vr) = (&u[cr * us..], &v[cr * vs..]);
            for (x, px) in line.chunks_exact_mut(4).enumerate() {
                let yy = i32::from(yr[x]) << 8;
                let uu = i32::from(ur[x / 2]) - 128;
                let vv = i32::from(vr[x / 2]) - 128;
                // ×256: 1.5748, 0.1873, 0.4681, 1.8556.
                let r = (yy + 403 * vv) >> 8;
                let g = (yy - 48 * uu - 120 * vv) >> 8;
                let b = (yy + 475 * uu) >> 8;
                px[0] = b.clamp(0, 255) as u8;
                px[1] = g.clamp(0, 255) as u8;
                px[2] = r.clamp(0, 255) as u8;
                px[3] = 0;
            }
        });
}

enum Unit {
    Reset,
    Data(Vec<u8>),
}

struct Sink {
    units: mpsc::Sender<Unit>,
    audio: Option<mpsc::Sender<Vec<u8>>>,
}

impl VideoSink for Sink {
    fn reset(&mut self, _w: u32, _h: u32, codec: VideoCodec) {
        if codec != VideoCodec::H264 {
            warn!(?codec, "Only H.264 can be decoded here");
        }
        let _ = self.units.send(Unit::Reset);
    }
    fn frame(&mut self, data: &[u8]) {
        let _ = self.units.send(Unit::Data(data.to_vec()));
    }
    fn audio(&mut self, pcm: &[u8]) {
        if let Some(a) = &self.audio {
            let _ = a.send(pcm.to_vec());
        }
    }
}

/// Session audio to `pacat` (PulseAudio's player, in Termux as `pulseaudio`).
fn audio_thread() -> Option<mpsc::Sender<Vec<u8>>> {
    let mut child = std::process::Command::new("pacat")
        .args([
            "--playback",
            "--raw",
            "--rate=48000",
            "--channels=2",
            "--format=s16le",
            "--latency-msec=40",
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| warn!(%e, "No audio (pacat not found)"))
        .ok()?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut stdin = child.stdin.take()?;
    std::thread::spawn(move || {
        while let Ok(pcm) = rx.recv() {
            if stdin.write_all(&pcm).is_err() {
                break;
            }
        }
        let _ = child.kill();
    });
    Some(tx)
}

/// The logo as 64x64 RGBA (scripts/gen-icons.py).
const ICON: &[u8] = include_bytes!("../../../assets/logo/glidedesk-64.rgba");

pub fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = Args::parse();
    let conn = Arc::new(connect()?);
    let screen = &conn.setup().roots[0];
    if args.width == 0 || args.height == 0 {
        args.width = screen.width_in_pixels;
        args.height = screen.height_in_pixels;
    }
    if screen.root_depth != 24 && screen.root_depth != 32 {
        bail!(
            "the X display has depth {}, 24 or 32 is needed",
            screen.root_depth
        );
    }
    let (root, visual, depth) = (screen.root, screen.root_visual, screen.root_depth);
    let win = conn.generate_id()?;
    conn.create_window(
        depth,
        win,
        root,
        0,
        0,
        args.width,
        args.height,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(
                EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::EXPOSURE
                    | EventMask::FOCUS_CHANGE,
            ),
    )?;
    let title = format!("glidedesk - {}", args.host);
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )?;
    // _NET_WM_ICON: width, height, then ARGB pixels.
    let net_wm_icon = conn.intern_atom(false, b"_NET_WM_ICON")?.reply()?.atom;
    let icon: Vec<u32> = [64, 64]
        .into_iter()
        .chain(
            ICON.chunks_exact(4)
                .map(|p| u32::from_be_bytes([p[3], p[0], p[1], p[2]])),
        )
        .collect();
    conn.change_property32(
        PropMode::REPLACE,
        win,
        net_wm_icon,
        AtomEnum::CARDINAL,
        &icon,
    )?;
    let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
    let wm_delete = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        wm_protocols,
        AtomEnum::ATOM,
        &[wm_delete],
    )?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;
    conn.map_window(win)?;
    conn.flush()?;

    // Decoder thread: decode, convert, draw.
    let (unit_tx, unit_rx) = mpsc::channel::<Unit>();
    let desktop = Arc::new(Mutex::new((u32::from(args.width), u32::from(args.height))));
    {
        let (conn, desktop) = (Arc::clone(&conn), Arc::clone(&desktop));
        std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || {
                let mut decoder = openh264::decoder::Decoder::new().ok();
                let mut bgrx = Vec::new();
                let max = conn.maximum_request_bytes();
                let (mut frames, mut busy, mut since) = (0u32, Duration::ZERO, Instant::now());
                while let Ok(unit) = unit_rx.recv() {
                    let data = match unit {
                        Unit::Reset => {
                            decoder = openh264::decoder::Decoder::new().ok();
                            continue;
                        }
                        Unit::Data(d) => d,
                    };
                    let t = Instant::now();
                    let Some(dec) = decoder.as_mut() else {
                        continue;
                    };
                    let Ok(Some(yuv)) = dec.decode(&data) else {
                        continue;
                    };
                    let (w, h) = yuv.dimensions();
                    yuv_to_bgrx(&yuv, &mut bgrx);
                    *desktop.lock().unwrap() = (w as u32, h as u32);
                    // PutImage in strips that fit the server's request size.
                    let rows = ((max.saturating_sub(64)) / (w * 4)).clamp(1, h);
                    let mut y0 = 0;
                    while y0 < h {
                        let n = rows.min(h - y0);
                        let _ = conn.put_image(
                            ImageFormat::Z_PIXMAP,
                            win,
                            gc,
                            w as u16,
                            n as u16,
                            0,
                            y0 as i16,
                            0,
                            depth,
                            &bgrx[y0 * w * 4..(y0 + n) * w * 4],
                        );
                        y0 += n;
                    }
                    let _ = conn.flush();
                    frames += 1;
                    busy += t.elapsed();
                    if since.elapsed() >= Duration::from_secs(5) {
                        debug!(
                            frames,
                            ms_per_frame = busy.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
                            "Display"
                        );
                        (frames, busy, since) = (0, Duration::ZERO, Instant::now());
                    }
                }
            })?;
    }

    let audio = if args.mute { None } else { audio_thread() };
    let sink: SharedSink = Arc::new(Mutex::new(Sink {
        units: unit_tx,
        audio,
    }));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    {
        let a = args.clone();
        runtime.spawn(async move {
            let r = if a.rdp {
                let opts = glidedesk_client::session::ConnectOptions {
                    host: a.host,
                    port: a.port.unwrap_or(3389),
                    username: a.user,
                    password: a.password,
                    width: a.width,
                    height: a.height,
                };
                glidedesk_client::session::run(opts, sink, rx).await
            } else {
                let opts = glidedesk_client::native::NativeOptions {
                    host: a.host,
                    port: a.port.unwrap_or(glidedesk_proto::DEFAULT_PORT),
                    username: a.user,
                    password: a.password,
                    width: a.width,
                    height: a.height,
                    codecs: vec![VideoCodec::H264],
                    refresh: 60,
                    pinned: None,
                };
                glidedesk_client::native::run(opts, sink, rx)
                    .await
                    .map(|_| ())
            };
            match r {
                Ok(()) => eprintln!("Session ended"),
                Err(e) => eprintln!("{e:#}"),
            }
            std::process::exit(0);
        });
    }

    // Resizes go out once the window has kept its size for a moment.
    let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(mut size) = resize_rx.recv() {
                while let Ok(s) = resize_rx.recv_timeout(Duration::from_millis(250)) {
                    size = s;
                }
                let _ = tx.send(Input::Resize {
                    width: u32::from(size.0).max(200) & !1,
                    height: u32::from(size.1).max(200) & !1,
                });
            }
        });
    }

    let mut held: std::collections::HashSet<(u8, bool)> = Default::default();
    let mut win_size = (args.width, args.height);
    let send = |i: Input| {
        let _ = tx.send(i);
    };
    loop {
        let ev = conn.wait_for_event()?;
        match ev {
            XEvent::ClientMessage(m) if m.data.as_data32()[0] == wm_delete => {
                send(Input::Disconnect);
                std::thread::sleep(Duration::from_millis(200));
                return Ok(());
            }
            XEvent::ConfigureNotify(c) => {
                if (c.width, c.height) != win_size {
                    win_size = (c.width, c.height);
                    let _ = resize_tx.send(win_size);
                }
            }
            XEvent::MotionNotify(m) => {
                let (dw, dh) = *desktop.lock().unwrap();
                let x = (i32::from(m.event_x).clamp(0, dw as i32 - 1)) as u16;
                let y = (i32::from(m.event_y).clamp(0, dh as i32 - 1)) as u16;
                send(Input::MouseMove { x, y });
            }
            XEvent::ButtonPress(b) | XEvent::ButtonRelease(b) => {
                let pressed = matches!(ev, XEvent::ButtonPress(_));
                let button = match b.detail {
                    1 => Some(0),
                    2 => Some(1),
                    3 => Some(2),
                    8 => Some(3),
                    9 => Some(4),
                    _ => None,
                };
                if let Some(button) = button {
                    send(Input::MouseButton { button, pressed });
                } else if pressed {
                    // 4/5 wheel up/down, 6/7 left/right.
                    let (vertical, units) = match b.detail {
                        4 => (true, 120),
                        5 => (true, -120),
                        6 => (false, -120),
                        7 => (false, 120),
                        _ => continue,
                    };
                    send(Input::Wheel { vertical, units });
                }
            }
            XEvent::KeyPress(k) | XEvent::KeyRelease(k) => {
                let pressed = matches!(ev, XEvent::KeyPress(_));
                if let Some((sc, ext)) = evdev_to_scancode(u32::from(k.detail).saturating_sub(8)) {
                    if pressed {
                        held.insert((sc, ext));
                    } else {
                        held.remove(&(sc, ext));
                    }
                    send(Input::Key {
                        scancode: u16::from(sc),
                        extended: ext,
                        pressed,
                    });
                }
            }
            XEvent::FocusOut(_) => {
                for (sc, ext) in held.drain() {
                    send(Input::Key {
                        scancode: u16::from(sc),
                        extended: ext,
                        pressed: false,
                    });
                }
            }
            XEvent::Error(e) => debug!(?e, "X error"),
            _ => {}
        }
    }
}

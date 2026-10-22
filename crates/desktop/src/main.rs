//! glidedesk desktop client: a window showing the remote desktop.
//!
//! Speaks the native low-latency protocol by default (RDP with `--rdp`). The desktop
//! follows the window size, the pointer is the local one with the remote shape, keys are
//! sent as scan codes, session audio plays on the default output.

mod audio;
mod keymap;
mod render;
mod video;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use glidedesk_client::gfx::{SharedSink, VideoCodec};
use glidedesk_client::session::Input;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{CustomCursor, Fullscreen, Window, WindowId};

use video::{Event, Picture};

#[derive(Parser, Clone)]
#[command(version, about = "glidedesk remote desktop client")]
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
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,
    #[arg(long)]
    fullscreen: bool,
    /// No sound.
    #[arg(long)]
    mute: bool,
}

/// Wait this long after the last window resize before asking the server for the new size
/// (each change rebuilds the encoder and starts with an IDR).
const RESIZE_SETTLE: Duration = Duration::from_millis(250);

struct App {
    args: Args,
    proxy: EventLoopProxy<Event>,
    instance: Option<wgpu::Instance>,
    window: Option<Arc<Window>>,
    renderer: Option<render::Renderer>,
    slot: Arc<Mutex<Option<Picture>>>,
    /// Size of the pictures being shown (the remote desktop).
    desktop: (u32, u32),
    input: Option<UnboundedSender<Input>>,
    pending_resize: Option<(PhysicalSize<u32>, Instant)>,
    pressed: HashSet<(u8, bool)>,
    cursor: Option<CustomCursor>,
    _audio: Option<cpal::Stream>,
    runtime: tokio::runtime::Runtime,
    stats: (u32, Duration, Instant),
}

impl App {
    fn send(&self, i: Input) {
        if let Some(tx) = &self.input {
            let _ = tx.send(i);
        }
    }

    fn start_session(&mut self, size: PhysicalSize<u32>) {
        let proxy = self.proxy.clone();
        let notify: video::Notify = Arc::new(move |e| {
            let _ = proxy.send_event(e);
        });
        let player = if self.args.mute {
            None
        } else {
            match audio::start() {
                Ok((p, stream)) => {
                    self._audio = Some(stream);
                    Some(p)
                }
                Err(e) => {
                    warn!(%e, "No audio");
                    None
                }
            }
        };
        let sink: SharedSink = Arc::new(Mutex::new(video::start(
            Arc::clone(&self.slot),
            Arc::clone(&notify),
            player,
        )));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.input = Some(tx);
        let a = self.args.clone();
        let (w, h) = (size.width.max(200) & !1, size.height.max(200) & !1);
        self.runtime.spawn(async move {
            let r = if a.rdp {
                let opts = glidedesk_client::session::ConnectOptions {
                    host: a.host,
                    port: a.port.unwrap_or(3389),
                    username: a.user,
                    password: a.password,
                    width: w as u16,
                    height: h as u16,
                };
                glidedesk_client::session::run(opts, sink, rx).await
            } else {
                let opts = glidedesk_client::native::NativeOptions {
                    host: a.host,
                    port: a.port.unwrap_or(glidedesk_proto::DEFAULT_PORT),
                    username: a.user,
                    password: a.password,
                    width: w as u16,
                    height: h as u16,
                    codecs: vec![VideoCodec::H264],
                    refresh: 60,
                    pinned: None,
                };
                glidedesk_client::native::run(opts, sink, rx)
                    .await
                    .map(|_| ())
            };
            notify(Event::Ended(match r {
                Ok(()) => "Session ended".into(),
                Err(e) => format!("{e:#}"),
            }));
        });
    }

    /// Window position to desktop pixels.
    fn to_desktop(&self, x: f64, y: f64) -> (u16, u16) {
        let Some(win) = &self.window else {
            return (0, 0);
        };
        let s = win.inner_size();
        let (dw, dh) = self.desktop;
        let fx =
            (x / f64::from(s.width.max(1)) * f64::from(dw)).clamp(0.0, f64::from(dw.max(1) - 1));
        let fy =
            (y / f64::from(s.height.max(1)) * f64::from(dh)).clamp(0.0, f64::from(dh.max(1) - 1));
        (fx as u16, fy as u16)
    }

    fn release_all(&mut self) {
        for (code, ext) in std::mem::take(&mut self.pressed) {
            self.send(Input::Key {
                scancode: u16::from(code),
                extended: ext,
                pressed: false,
            });
        }
    }
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes()
            .with_title(format!("glidedesk - {}", self.args.host))
            .with_inner_size(PhysicalSize::new(self.args.width, self.args.height));
        if self.args.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("creating the window: {e}");
                el.exit();
                return;
            }
        };
        let instance = self.instance.get_or_insert_with(|| {
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(
                Box::new(el.owned_display_handle()),
            ))
        });
        match render::Renderer::new(instance, Arc::clone(&window)) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("graphics: {e:#}");
                el.exit();
                return;
            }
        }
        let size = window.inner_size();
        self.desktop = (size.width, size.height);
        self.window = Some(window);
        self.start_session(size);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: Event) {
        match event {
            Event::Picture => {
                let pic = self.slot.lock().unwrap().take();
                if let (Some(r), Some(p)) = (self.renderer.as_mut(), pic) {
                    self.desktop = (p.width, p.height);
                    r.draw(Some(&p));
                    let s = &mut self.stats;
                    s.0 += 1;
                    s.1 += p.received.elapsed();
                    if s.2.elapsed() >= Duration::from_secs(5) {
                        info!(
                            fps = f64::from(s.0) / s.2.elapsed().as_secs_f64(),
                            received_to_present_ms =
                                s.1.as_secs_f64() * 1000.0 / f64::from(s.0.max(1)),
                            "Display"
                        );
                        *s = (0, Duration::ZERO, Instant::now());
                    }
                }
            }
            Event::Cursor(c) => {
                let Some(win) = &self.window else { return };
                match c {
                    glidedesk_proto::Cursor::Hidden => win.set_cursor_visible(false),
                    glidedesk_proto::Cursor::Shape {
                        width,
                        height,
                        hot_x,
                        hot_y,
                        rgba,
                    } => {
                        if let Ok(src) = CustomCursor::from_rgba(rgba, width, height, hot_x, hot_y)
                        {
                            let cursor = el.create_custom_cursor(src);
                            win.set_cursor(cursor.clone());
                            win.set_cursor_visible(true);
                            self.cursor = Some(cursor);
                        }
                    }
                }
            }
            Event::Ended(reason) => {
                eprintln!("{reason}");
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.send(Input::Disconnect);
                el.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                    r.draw(None);
                }
                self.pending_resize = Some((size, Instant::now()));
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    r.draw(None);
                }
            }
            WindowEvent::Focused(false) => self.release_all(),
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = self.to_desktop(position.x, position.y);
                self.send(Input::MouseMove { x, y });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(_) => return,
                };
                self.send(Input::MouseButton {
                    button,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    // Touchpads: about 40 pixels per notch.
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 * 3.0, p.y as f32 * 3.0),
                };
                if dy != 0.0 {
                    self.send(Input::Wheel {
                        vertical: true,
                        units: dy as i16,
                    });
                }
                if dx != 0.0 {
                    self.send(Input::Wheel {
                        vertical: false,
                        units: dx as i16,
                    });
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some((sc, ext)) = keymap::scancode(code)
                {
                    if pressed {
                        self.pressed.insert((sc, ext));
                    } else {
                        self.pressed.remove(&(sc, ext));
                    }
                    self.send(Input::Key {
                        scancode: u16::from(sc),
                        extended: ext,
                        pressed,
                    });
                } else if pressed && let Some(text) = event.text {
                    // Keys with no scan code (some media and IME keys): send their text.
                    for ch in text.chars() {
                        self.send(Input::Unicode { ch, pressed: true });
                        self.send(Input::Unicode { ch, pressed: false });
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Some((size, at)) = self.pending_resize {
            if at.elapsed() >= RESIZE_SETTLE {
                self.pending_resize = None;
                self.send(Input::Resize {
                    width: size.width.max(200) & !1,
                    height: size.height.max(200) & !1,
                });
            } else {
                el.set_control_flow(ControlFlow::WaitUntil(at + RESIZE_SETTLE));
                return;
            }
        }
        el.set_control_flow(ControlFlow::Wait);
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let event_loop = EventLoop::<Event>::with_user_event().build()?;
    let mut app = App {
        args,
        proxy: event_loop.create_proxy(),
        instance: None,
        window: None,
        renderer: None,
        slot: Arc::default(),
        desktop: (0, 0),
        input: None,
        pending_resize: None,
        pressed: HashSet::new(),
        cursor: None,
        _audio: None,
        runtime: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
        stats: (0, Duration::ZERO, Instant::now()),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

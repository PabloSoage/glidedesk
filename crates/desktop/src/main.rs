//! glidedesk desktop client: a window showing the remote desktop.
//!
//! Speaks the native low-latency protocol by default (RDP with `--rdp`). The desktop
//! follows the window size, the pointer is the local one with the remote shape, keys are
//! sent as scan codes, session audio plays on the default output.
//!
//! With `--monitors` (native protocol, servers from version 5) the desktop spans several
//! monitors of this computer: a borderless full-screen window on each, each showing a
//! monitor of the remote desktop (one window per monitor, not one across them all, so each
//! keeps its own scale and refresh rate). Pointer positions are mapped from each window to
//! the remote desktop's space.

// A window program: no console next to it. The log goes to a file (`platform::log_path`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod clip;
mod decode;
mod keymap;
mod launcher;
#[cfg(not(windows))]
mod linux;
mod platform;
mod profile;
mod render;
mod video;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use glidedesk_client::gfx::{SharedSink, VideoCodec};
use glidedesk_client::session::Input;
use glidedesk_client::trust::{self, Trust};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::monitor::MonitorHandle;
use winit::window::{CustomCursor, Fullscreen, Icon, Window, WindowId};

use video::{Event, Picture};

/// The logo as 64x64 RGBA (scripts/gen-icons.py).
const ICON: &[u8] = include_bytes!("../../../assets/logo/glidedesk-64.rgba");

#[derive(Parser, Clone)]
#[command(version, about = "glidedesk remote desktop client")]
struct Args {
    /// Server host name or address (or path to a .glidedesk profile). If omitted, opens launcher.
    host: Option<String>,
    /// Without a password, the saved credentials or the Windows sign-in dialog are used.
    #[arg(long)]
    user: Option<String>,
    #[arg(long, env = "GLIDEDESK_PASSWORD", hide_env_values = true)]
    password: Option<String>,
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
    /// Decode on the CPU even when the GPU could (H.264 only then).
    #[arg(long)]
    software_decode: bool,
    /// Decode and draw on this GPU (amd, intel, nvidia) instead of the default one.
    #[arg(long, value_parser = parse_vendor)]
    gpu: Option<u32>,
    /// SHA-256 fingerprint of the server certificate to pin (64 hex digits; colons allowed).
    /// Without it, the one trusted at the first connection is used.
    #[arg(long)]
    fingerprint: Option<String>,
    /// Video codec to ask for (the native protocol; RDP is always H.264).
    #[arg(long, value_enum, default_value_t = CodecChoice::Auto)]
    codec: CodecChoice,
    /// Ask for HEVC 4:4:4 (full chroma: sharp coloured text) from servers started with
    /// `--yuv444`. Native protocol; HEVC even without a GPU decoder (FFmpeg decodes 4:4:4 in
    /// software where the GPU cannot, e.g. D3D11VA on Windows).
    #[arg(long)]
    yuv444: bool,
    /// Do not share the clipboard.
    #[arg(long)]
    no_clipboard: bool,
    /// Open this app of the server (a name from its apps.conf) instead of the desktop; the
    /// session ends when the app does. Native protocol only.
    #[arg(long)]
    app: Option<String>,
    /// Make this program open .glidedesk files (double click) for the current user, and
    /// exit.
    #[arg(long)]
    register_files: bool,
    /// Show the desktop on several monitors of this computer, a full-screen window on each:
    /// `all`, or their numbers, such as `1,2` (numbered left to right; the log lists them).
    /// Native protocol, servers from version 5.
    #[arg(long, value_parser = parse_monitors)]
    monitors: Option<MonitorChoice>,
}

/// Which monitors of this computer show the desktop (`--monitors`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MonitorChoice {
    All,
    /// Numbered from 1, left to right (then top to bottom).
    Numbers(Vec<usize>),
}

/// `all`, or monitor numbers separated by commas.
pub fn parse_monitors(s: &str) -> Result<MonitorChoice, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("all") {
        return Ok(MonitorChoice::All);
    }
    let mut numbers = Vec::new();
    for part in s.split(',') {
        let n: usize = part
            .trim()
            .parse()
            .ok()
            .filter(|&n| n >= 1)
            .ok_or_else(|| format!("{part:?}: not a monitor number (all, or numbers: 1,2)"))?;
        if !numbers.contains(&n) {
            numbers.push(n);
        }
    }
    Ok(MonitorChoice::Numbers(numbers))
}

/// A position in a window of `win` pixels that shows monitor `m` of the remote layout, in
/// the remote desktop's pixels.
fn window_to_desktop(m: &glidedesk_proto::Monitor, win: (u32, u32), x: f64, y: f64) -> (u16, u16) {
    let (w, h) = (f64::from(m.width), f64::from(m.height));
    let fx = (x / f64::from(win.0.max(1)) * w).clamp(0.0, (w - 1.0).max(0.0));
    let fy = (y / f64::from(win.1.max(1)) * h).clamp(0.0, (h - 1.0).max(0.0));
    (
        (f64::from(m.x.max(0)) + fx) as u16,
        (f64::from(m.y.max(0)) + fy) as u16,
    )
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CodecChoice {
    /// HEVC where the GPU decodes it, else H.264.
    Auto,
    H264,
    Hevc,
}

/// A GPU maker's name as its PCI vendor id.
fn parse_vendor(s: &str) -> Result<u32, String> {
    match s.to_ascii_lowercase().as_str() {
        "amd" => Ok(0x1002),
        "intel" => Ok(0x8086),
        "nvidia" => Ok(decode::VENDOR_NVIDIA),
        _ => Err("amd, intel or nvidia".into()),
    }
}

/// A `.glidedesk` profile over the command line's defaults.
fn apply_profile(args: &mut Args, prof: profile::Profile) -> Result<()> {
    args.host = Some(prof.host);
    if let Some(u) = prof.user {
        args.user = Some(u);
    }
    if let Some(port) = prof.port {
        args.port = Some(port);
    }
    if let Some(rdp) = prof.rdp {
        args.rdp = rdp;
    }
    if let (Some(w), Some(h)) = (prof.width, prof.height) {
        args.width = w;
        args.height = h;
    }
    if let Some(fs) = prof.fullscreen {
        args.fullscreen = fs;
    }
    if let Some(m) = prof.mute {
        args.mute = m;
    }
    if let Some(sw) = prof.software_decode {
        args.software_decode = sw;
    }
    if let Some(fp) = prof.fingerprint {
        args.fingerprint = Some(fp);
    }
    if let Some(c) = prof.codec {
        args.codec = c;
    }
    if let Some(c) = prof.clipboard {
        args.no_clipboard = !c;
    }
    if let Some(app) = prof.app.filter(|a| !a.is_empty()) {
        args.app = Some(app);
    }
    if let Some(g) = prof.gpu {
        args.gpu =
            Some(parse_vendor(&g).map_err(|e| anyhow::anyhow!("gpu = {g:?} in the profile: {e}"))?);
    }
    if let Some(m) = prof.monitors.filter(|m| !m.trim().is_empty()) {
        args.monitors = Some(
            parse_monitors(&m)
                .map_err(|e| anyhow::anyhow!("monitors = {m:?} in the profile: {e}"))?,
        );
    }
    Ok(())
}

/// How to trust the server: the fingerprint given (`--fingerprint` or the profile), else the one
/// saved at the first connection, else ask the user and save the answer.
fn trust_for(a: &Args, host: &str, port: u16) -> Result<Trust, String> {
    if let Some(fp) = &a.fingerprint {
        return Ok(Trust {
            pinned: Some(trust::parse_fingerprint(fp)?),
            confirm: None,
        });
    }
    let (host, protocol) = (host.to_owned(), if a.rdp { "RDP" } else { "glidedesk" });
    Ok(Trust {
        pinned: profile::KnownHosts::load().get(&host, port),
        confirm: Some(Arc::new(move |fp| {
            let ok = platform::confirm(&format!(
                "First connection to {host}:{port} ({protocol}).

\n                 Server certificate SHA-256:
{}

\n                 Compare it with the one the server logs at start (or its owner gives you). Trust this server?",
                trust::hex(fp)
            ));
            if ok {
                let mut known = profile::KnownHosts::load();
                known.set(&host, port, fp);
                if let Err(e) = known.save() {
                    warn!(
                        error = format!("{e:#}"),
                        "Could not save the server fingerprint"
                    );
                }
            }
            ok
        })),
    })
}

/// Wait this long after the last window resize before asking the server for the new size
/// (each change rebuilds the encoder and starts with an IDR).
const RESIZE_SETTLE: Duration = Duration::from_millis(250);

/// The window on a further monitor of this computer (`--monitors`).
struct Screen {
    /// The remote monitor it shows (its id in the layout).
    monitor: u8,
    window: Arc<Window>,
    renderer: render::Renderer,
    slot: Arc<Mutex<Option<Picture>>>,
}

struct App {
    args: Args,
    launcher_state: Option<launcher::LauncherState>,
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
    clipboard: Option<clip::Clipboard>,
    _audio: Option<cpal::Stream>,
    runtime: tokio::runtime::Runtime,
    stats: (u32, Duration, Instant),
    /// The user closed the window: the session's end is no error.
    closing: bool,
    /// Windows on further monitors of this computer (`--monitors`).
    screens: Vec<Screen>,
    /// The monitors of the remote desktop as the server set them up; empty or one: the
    /// desktop is the window's.
    layout: Vec<glidedesk_proto::Monitor>,
}

impl App {
    fn send(&self, i: Input) {
        if let Some(tx) = &self.input {
            let _ = tx.send(i);
        }
    }

    /// With `--monitors`, a full-screen window on each chosen monitor of this computer: the
    /// main window on the first (the primary, if chosen), a new window on each of the
    /// others. Returns them as the layout to ask the server for, monitor 0 the main
    /// window's; empty for one window.
    fn open_screens(
        &mut self,
        el: &ActiveEventLoop,
        window: &Arc<Window>,
    ) -> Vec<glidedesk_proto::Monitor> {
        let Some(choice) = self.args.monitors.clone() else {
            return Vec::new();
        };
        if self.args.rdp {
            warn!("Several monitors need the native protocol: one window");
            return Vec::new();
        }
        let mut all: Vec<MonitorHandle> = el.available_monitors().collect();
        all.sort_by_key(|m| (m.position().x, m.position().y));
        let primary = el.primary_monitor();
        for (i, m) in all.iter().enumerate() {
            info!(
                number = i + 1,
                name = m.name().as_deref().unwrap_or(""),
                width = m.size().width,
                height = m.size().height,
                x = m.position().x,
                y = m.position().y,
                primary = primary.as_ref() == Some(m),
                "Monitor of this computer"
            );
        }
        let mut chosen: Vec<MonitorHandle> = match &choice {
            MonitorChoice::All => all.clone(),
            MonitorChoice::Numbers(numbers) => numbers
                .iter()
                .filter_map(|&n| {
                    let m = all.get(n - 1).cloned();
                    if m.is_none() {
                        warn!(number = n, monitors = all.len(), "No such monitor");
                    }
                    m
                })
                .collect(),
        };
        if let Some(p) = chosen.iter().position(|m| primary.as_ref() == Some(m)) {
            chosen[..=p].rotate_right(1);
        }
        let Some(first) = chosen.first() else {
            return Vec::new();
        };
        window.set_fullscreen(Some(Fullscreen::Borderless(Some(first.clone()))));
        let mut layout = Vec::new();
        for (i, m) in chosen.iter().enumerate() {
            let Ok(id) = u8::try_from(i) else { break };
            if i > 0 {
                match self.open_screen(el, id, m) {
                    Ok(s) => self.screens.push(s),
                    Err(e) => {
                        let error = format!("{e:#}");
                        warn!(number = i + 1, error, "No window on the monitor");
                        continue;
                    }
                }
            }
            layout.push(glidedesk_proto::Monitor {
                id,
                x: m.position().x,
                y: m.position().y,
                width: m.size().width.min(u32::from(u16::MAX)) as u16,
                height: m.size().height.min(u32::from(u16::MAX)) as u16,
                refresh: m
                    .refresh_rate_millihertz()
                    .map_or(60, |mhz| (mhz + 500) / 1000),
                primary: primary.as_ref() == Some(m),
            });
        }
        if layout.len() < 2 {
            return Vec::new();
        }
        layout
    }

    /// A borderless full-screen window on monitor `m`, for remote monitor `monitor`.
    fn open_screen(
        &self,
        el: &ActiveEventLoop,
        monitor: u8,
        m: &MonitorHandle,
    ) -> Result<Screen> {
        let attrs = Window::default_attributes()
            .with_title(format!(
                "glidedesk - {} ({})",
                self.args.host.as_deref().unwrap_or_default(),
                u32::from(monitor) + 1
            ))
            .with_window_icon(Icon::from_rgba(ICON.to_vec(), 64, 64).ok())
            .with_position(m.position())
            .with_inner_size(m.size())
            .with_fullscreen(Some(Fullscreen::Borderless(Some(m.clone()))));
        let window = Arc::new(el.create_window(attrs)?);
        let instance = self
            .instance
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no graphics instance"))?;
        let renderer = render::Renderer::new(instance, Arc::clone(&window), self.args.gpu)?;
        Ok(Screen {
            monitor,
            window,
            renderer,
            slot: Arc::default(),
        })
    }

    /// The session spans several monitors: their sizes are fixed, the windows' resizes are
    /// not sent.
    fn spanning(&self) -> bool {
        !self.screens.is_empty() || self.layout.len() > 1
    }

    fn start_session(&mut self, size: PhysicalSize<u32>, monitors: Vec<glidedesk_proto::Monitor>) {
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
        let screens = self
            .screens
            .iter()
            .map(|s| (s.monitor, Arc::clone(&s.slot)))
            .collect();
        let sink: SharedSink = Arc::new(Mutex::new(video::start(
            Arc::clone(&self.slot),
            Arc::clone(&notify),
            player,
            !self.args.software_decode,
            self.args.yuv444 && !self.args.rdp,
            screens,
        )));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if !self.args.rdp && !self.args.no_clipboard {
            self.clipboard = Some(clip::Clipboard::start(tx.clone()));
        }
        self.input = Some(tx);
        let a = self.args.clone();
        let (w, h) = (size.width.max(200) & !1, size.height.max(200) & !1);
        // The monitor's rate, so the session runs as fast as this screen shows frames.
        let refresh = self
            .window
            .as_ref()
            .and_then(|w| w.current_monitor())
            .and_then(|m| m.refresh_rate_millihertz())
            .map_or(60, |mhz| (mhz + 500) / 1000);
        // HEVC (fewer bits for the same picture) only where the GPU decodes it.
        let can_hevc = !self.args.rdp && !self.args.software_decode && decode::hardware_hevc();
        // 4:4:4 needs HEVC, whoever decodes it.
        let want_444 = self.args.yuv444 && !self.args.rdp;
        let hevc = match self.args.codec {
            CodecChoice::Auto => can_hevc || want_444,
            CodecChoice::H264 => false,
            CodecChoice::Hevc => {
                if !can_hevc && !want_444 {
                    warn!("HEVC asked for, but this client cannot decode it here: H.264");
                }
                can_hevc || want_444
            }
        };
        self.runtime.spawn(async move {
            let host_str = a.host.clone().unwrap_or_default();
            let default_port = if a.rdp {
                3389
            } else {
                glidedesk_proto::DEFAULT_PORT
            };
            let port_num = a.port.unwrap_or(default_port);
            let trust = match trust_for(&a, &host_str, port_num) {
                Ok(t) => t,
                Err(e) => {
                    notify(Event::Ended(e));
                    return;
                }
            };

            let r = if a.rdp {
                let opts = glidedesk_client::session::ConnectOptions {
                    host: host_str,
                    port: port_num,
                    username: a.user.unwrap_or_default(),
                    password: a.password.unwrap_or_default(),
                    width: w as u16,
                    height: h as u16,
                    trust,
                };
                glidedesk_client::session::run(opts, sink, rx).await
            } else {
                let opts = glidedesk_client::native::NativeOptions {
                    host: host_str,
                    port: port_num,
                    username: a.user.unwrap_or_default(),
                    password: a.password.unwrap_or_default(),
                    width: w as u16,
                    height: h as u16,
                    codecs: if hevc {
                        vec![VideoCodec::Hevc, VideoCodec::H264]
                    } else {
                        vec![VideoCodec::H264]
                    },
                    refresh,
                    trust,
                    app: a.app,
                    monitors,
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

    /// A position in the main window, or in `screen`'s, to desktop pixels; `None` where the
    /// window shows nothing yet.
    fn to_desktop(&self, screen: Option<usize>, x: f64, y: f64) -> Option<(u16, u16)> {
        let (monitor, win) = match screen {
            Some(i) => (self.screens[i].monitor, &self.screens[i].window),
            None => (0, self.window.as_ref()?),
        };
        let s = win.inner_size();
        if self.layout.len() > 1 {
            let m = self.layout.iter().find(|m| m.id == monitor)?;
            return Some(window_to_desktop(m, (s.width, s.height), x, y));
        }
        if screen.is_some() {
            return None;
        }
        let (dw, dh) = self.desktop;
        let fx =
            (x / f64::from(s.width.max(1)) * f64::from(dw)).clamp(0.0, f64::from(dw.max(1) - 1));
        let fy =
            (y / f64::from(s.height.max(1)) * f64::from(dh)).clamp(0.0, f64::from(dh.max(1) - 1));
        Some((fx as u16, fy as u16))
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
        let is_launcher = self.args.host.is_none();
        let (win_w, win_h, title) = if is_launcher {
            (520, 480, "glidedesk".to_string())
        } else {
            (
                self.args.width,
                self.args.height,
                format!(
                    "glidedesk - {}",
                    self.args.host.as_deref().unwrap_or_default()
                ),
            )
        };
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_window_icon(Icon::from_rgba(ICON.to_vec(), 64, 64).ok())
            .with_inner_size(PhysicalSize::new(win_w, win_h));
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_taskbar_icon(Icon::from_rgba(ICON.to_vec(), 64, 64).ok());
        }
        if !is_launcher && self.args.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                platform::show_error(&format!("Creating the window: {e}"));
                el.exit();
                return;
            }
        };
        platform::add_menu(&window, self.proxy.clone());
        let instance = self.instance.get_or_insert_with(|| {
            #[allow(unused_mut)]
            let mut desc = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(
                el.owned_display_handle(),
            ));
            // Direct3D 12 on Windows: Intel's Vulkan driver (HD 630) hangs creating the
            // device for a window as large as the screen. WGPU_BACKEND still overrides it.
            #[cfg(windows)]
            {
                desc.backends = wgpu::Backends::DX12;
            }
            wgpu::Instance::new(desc.with_env())
        });
        match render::Renderer::new(instance, Arc::clone(&window), self.args.gpu) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                platform::show_error(&format!("Graphics: {e:#}"));
                el.exit();
                return;
            }
        }
        let size = window.inner_size();
        self.desktop = (size.width, size.height);
        if is_launcher {
            let ren = self.renderer.as_ref().unwrap();
            self.launcher_state = Some(launcher::LauncherState::new(
                &window,
                ren.device(),
                ren.format(),
            ));
            window.request_redraw();
        } else {
            let monitors = self.open_screens(el, &window);
            let size = monitors.first().map_or(size, |m| {
                PhysicalSize::new(u32::from(m.width), u32::from(m.height))
            });
            self.start_session(size, monitors);
        }
        self.window = Some(window);
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
            Event::MonitorPicture(monitor) => {
                let Some(s) = self.screens.iter_mut().find(|s| s.monitor == monitor) else {
                    return;
                };
                let pic = s.slot.lock().unwrap().take();
                if let Some(p) = pic {
                    s.renderer.draw(Some(&p));
                }
            }
            Event::Layout(monitors) => {
                // Windows of monitors the server did not set up would show nothing.
                let asked = self.screens.len() + 1;
                self.screens.retain(|s| monitors.iter().any(|m| m.id == s.monitor));
                if monitors.len() < asked {
                    warn!(shown = monitors.len(), asked, "The server shows fewer monitors");
                }
                self.layout = monitors;
            }
            Event::Cursor(c) => {
                let Some(win) = &self.window else { return };
                let windows = std::iter::once(win).chain(self.screens.iter().map(|s| &s.window));
                match c {
                    // A transparent cursor rather than set_cursor_visible: on Windows a
                    // hidden cursor may not come back until the pointer leaves the window.
                    glidedesk_proto::Cursor::Hidden => {
                        if let Ok(src) = CustomCursor::from_rgba(vec![0; 4], 1, 1, 0, 0) {
                            let cursor = el.create_custom_cursor(src);
                            for w in windows {
                                w.set_cursor(cursor.clone());
                            }
                        }
                    }
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
                            for w in windows {
                                w.set_cursor(cursor.clone());
                            }
                            self.cursor = Some(cursor);
                        }
                    }
                }
            }
            Event::Clipboard(text) => {
                if let Some(c) = &self.clipboard {
                    c.remote(text);
                }
            }
            Event::Files(paths) => {
                if let Some(c) = &self.clipboard {
                    c.remote_files(paths);
                }
            }
            Event::LinkWarning(warning) => {
                // In the title bar: visible in a window, and in the task bar when full
                // screen. The log has it too (the session logs every change).
                if let Some(win) = &self.window {
                    let host = self.args.host.as_deref().unwrap_or_default();
                    win.set_title(&match warning {
                        Some(w) => format!("glidedesk - {host} ({w})"),
                        None => format!("glidedesk - {host}"),
                    });
                }
            }
            Event::Ended(reason) => {
                if reason.contains("wrong user name or password")
                    && let Some(h) = &self.args.host
                {
                    platform::forget_credentials(h);
                }
                if self.closing {
                    info!("{reason}");
                } else {
                    platform::show_error(&reason);
                }
                el.exit();
            }
            Event::ToggleFullscreen => {
                if let Some(win) = &self.window {
                    let full = win.fullscreen().is_some();
                    win.set_fullscreen((!full).then_some(Fullscreen::Borderless(None)));
                }
            }
            Event::ShowLog => platform::open_log(),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.as_ref() else {
            return;
        };

        if let Some(ls) = &mut self.launcher_state {
            let _ = ls.on_event(window, &event);
            match event {
                WindowEvent::CloseRequested => {
                    self.closing = true;
                    el.exit();
                }
                WindowEvent::RedrawRequested => {
                    let mut action = launcher::LauncherAction::None;
                    if let Some(r) = self.renderer.as_mut() {
                        r.render_frame(|dev, q, view| {
                            action = ls.draw(window, dev, q, view);
                        });
                    }
                    match action {
                        launcher::LauncherAction::None => {}
                        launcher::LauncherAction::Exit => {
                            self.closing = true;
                            el.exit();
                        }
                        launcher::LauncherAction::Connect(args) => {
                            let mut args = *args;
                            if args.password.is_none() {
                                let host_str = args.host.as_deref().unwrap_or_default();
                                let Some((user, password)) =
                                    platform::credentials(host_str, args.user.as_deref())
                                else {
                                    if !cfg!(windows) {
                                        platform::show_error(
                                            "--user and --password (or GLIDEDESK_PASSWORD) are needed",
                                        );
                                    }
                                    return;
                                };
                                args.user = Some(user);
                                args.password = Some(password);
                            }
                            if let Some(v) = args.gpu {
                                decode::use_gpu(v);
                            }
                            let window = Arc::clone(window);
                            // Drawing on the GPU chosen in the options.
                            if args.gpu.is_some()
                                && let Some(instance) = self.instance.as_ref()
                            {
                                self.renderer = None;
                                match render::Renderer::new(instance, Arc::clone(&window), args.gpu)
                                {
                                    Ok(r) => self.renderer = Some(r),
                                    Err(e) => {
                                        platform::show_error(&format!("Graphics: {e:#}"));
                                        el.exit();
                                        return;
                                    }
                                }
                            }
                            self.args = args;
                            self.launcher_state = None;

                            let wanted = PhysicalSize::new(self.args.width, self.args.height);
                            // Applied now, or later with a Resized event (the session then follows).
                            let size = window.request_inner_size(wanted).unwrap_or(wanted);
                            if self.args.fullscreen {
                                window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                            }
                            window.set_title(&format!(
                                "glidedesk - {}",
                                self.args.host.as_deref().unwrap_or_default()
                            ));
                            if let Some(r) = self.renderer.as_mut() {
                                r.resize(size.width, size.height);
                            }
                            self.desktop = (size.width, size.height);
                            let monitors = self.open_screens(el, &window);
                            let size = monitors.first().map_or(size, |m| {
                                PhysicalSize::new(u32::from(m.width), u32::from(m.height))
                            });
                            self.start_session(size, monitors);
                        }
                    }
                }
                WindowEvent::Resized(size) => {
                    if let Some(r) = self.renderer.as_mut() {
                        r.resize(size.width, size.height);
                    }
                    window.request_redraw();
                }
                _ => {
                    window.request_redraw();
                }
            }
            return;
        }

        // The main window, or the window on a further monitor (`screen`).
        let screen = self.screens.iter().position(|s| s.window.id() == id);
        if screen.is_none() && id != window.id() {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.closing = true;
                self.send(Input::Disconnect);
                el.exit();
            }
            WindowEvent::Resized(size) => {
                let renderer = match screen {
                    Some(i) => Some(&mut self.screens[i].renderer),
                    None => self.renderer.as_mut(),
                };
                if let Some(r) = renderer {
                    r.resize(size.width, size.height);
                    r.draw(None);
                }
                if screen.is_none() && !self.spanning() {
                    self.pending_resize = Some((size, Instant::now()));
                }
            }
            WindowEvent::RedrawRequested => {
                let renderer = match screen {
                    Some(i) => Some(&mut self.screens[i].renderer),
                    None => self.renderer.as_mut(),
                };
                if let Some(r) = renderer {
                    r.draw(None);
                }
            }
            WindowEvent::Focused(false) => self.release_all(),
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((x, y)) = self.to_desktop(screen, position.x, position.y) {
                    self.send(Input::MouseMove { x, y });
                }
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
    platform::init_logging();
    let args = match Args::try_parse() {
        Ok(a) => a,
        // Help, version and usage errors: in a dialog, as there is no console on Windows.
        Err(e) if !e.use_stderr() => {
            #[cfg(windows)]
            platform::show_message(&e.to_string());
            e.exit()
        }
        Err(e) => {
            platform::show_error(&e.to_string());
            e.exit()
        }
    };
    let mut args = args;
    if args.register_files {
        match platform::register_files() {
            Ok(msg) => {
                info!("{msg}");
                #[cfg(windows)]
                platform::show_message(&msg);
                #[cfg(not(windows))]
                println!("{msg}");
            }
            Err(e) => platform::show_error(&format!("Registering .glidedesk files: {e:#}")),
        }
        return Ok(());
    }
    let is_launcher = args.host.is_none();
    // A profile given as the host (double-clicked, or on the command line).
    if let Some(h) = args.host.clone()
        && h.ends_with(".glidedesk")
        && let Err(e) = profile::Profile::load_file(std::path::Path::new(&h))
            .and_then(|prof| apply_profile(&mut args, prof))
    {
        platform::show_error(&format!("{e:#}"));
        return Ok(());
    }
    if let Some(fp) = &args.fingerprint
        && let Err(e) = trust::parse_fingerprint(fp)
    {
        platform::show_error(&format!("--fingerprint: {e}"));
        return Ok(());
    }

    if !is_launcher {
        if let Some(v) = args.gpu {
            decode::use_gpu(v);
        }
        if args.password.is_none() {
            let host_str = args.host.as_deref().unwrap_or_default();
            let Some((user, password)) = platform::credentials(host_str, args.user.as_deref())
            else {
                if !cfg!(windows) {
                    platform::show_error(
                        "--user and --password (or GLIDEDESK_PASSWORD) are needed",
                    );
                }
                return Ok(());
            };
            args.user = Some(user);
            args.password = Some(password);
        }
    }
    let event_loop = EventLoop::<Event>::with_user_event().build()?;
    let mut app = App {
        args,
        launcher_state: None,
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
        clipboard: None,
        _audio: None,
        runtime: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
        stats: (0, Duration::ZERO, Instant::now()),
        closing: false,
        screens: Vec::new(),
        layout: Vec::new(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_choices() {
        assert_eq!(parse_monitors("all"), Ok(MonitorChoice::All));
        assert_eq!(parse_monitors(" 2, 1,2 "), Ok(MonitorChoice::Numbers(vec![2, 1])));
        assert!(parse_monitors("0").is_err());
        assert!(parse_monitors("1,x").is_err());
    }

    #[test]
    fn window_positions_land_on_their_monitor() {
        let m = glidedesk_proto::Monitor {
            id: 1,
            x: 1920,
            y: 0,
            width: 2560,
            height: 1440,
            refresh: 60,
            primary: false,
        };
        // A window of the monitor's size, and one at half its resolution (scaled).
        assert_eq!(window_to_desktop(&m, (2560, 1440), 10.0, 20.0), (1930, 20));
        assert_eq!(window_to_desktop(&m, (1280, 720), 640.0, 360.0), (3200, 720));
        assert_eq!(window_to_desktop(&m, (2560, 1440), 9999.0, -5.0), (4479, 0));
    }
}

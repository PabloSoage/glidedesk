// Builds without the GNOME session (X11 only) leave the NVIDIA DMA-BUF import and the
// GNOME-only test commands unused.
#![cfg_attr(not(feature = "gnome"), allow(dead_code, unused_imports))]

mod apps;
#[cfg(feature = "gnome")]
mod audio;
mod auth;
#[cfg(feature = "gnome")]
mod capture;
#[cfg(feature = "gnome")]
mod clipboard;
mod color;
mod csc;
#[cfg(target_os = "linux")]
mod daemon;
mod encoder;
mod ffenc;
mod filecopy;
mod frame;
mod gamestream;
#[cfg(feature = "gnome")]
mod gnome;
mod gpu;
#[cfg(target_os = "linux")]
mod handover;
mod input;
#[cfg(feature = "gnome")]
mod mutter;
mod native;
mod nvenc;
mod pipeline;
#[cfg(feature = "x11")]
mod pulse;
mod rate;
mod rdp;
mod region;
mod serve;
mod sound;
#[cfg(feature = "gnome")]
mod vaimport;
#[cfg(feature = "web")]
mod web;
#[cfg(target_os = "windows")]
mod winaudio;
#[cfg(target_os = "windows")]
mod winclip;
#[cfg(target_os = "windows")]
mod wincuda;
#[cfg(target_os = "windows")]
mod winservice;
#[cfg(target_os = "windows")]
mod winsession;
#[cfg(target_os = "windows")]
mod winvdisplay;
#[cfg(feature = "x11")]
mod x11session;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

/// What logins are checked against (`--auth`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum AuthMode {
    /// The password given to the host (`--password`).
    Password,
    /// The system's password of the account the host runs as.
    System,
}

#[derive(Parser)]
#[command(version, about = "glidedesk session host")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the session over RDP and the native protocol.
    Serve {
        /// Serve an existing X11 display (e.g. :0) instead of a headless GNOME session.
        #[cfg(feature = "x11")]
        #[arg(long, value_name = "DISPLAY")]
        x11: Option<String>,
        /// Refresh rate of the virtual monitor (Hz). Higher means fresher frames and lower
        /// latency at the cost of more encoding while things move.
        #[arg(long, env = "GLIDEDESK_REFRESH", default_value_t = 120)]
        refresh: u32,
        /// RDP address.
        #[arg(long, default_value = "0.0.0.0:3389")]
        bind: std::net::SocketAddr,
        /// Native protocol address (TCP and UDP).
        #[arg(long, default_value = "0.0.0.0:3391")]
        native_bind: std::net::SocketAddr,
        /// Only RDP.
        #[arg(long)]
        no_native: bool,
        /// Web client address: the page over HTTPS (TCP) and WebRTC media (UDP).
        #[cfg(feature = "web")]
        #[arg(long, default_value = "0.0.0.0:3390")]
        web_bind: std::net::SocketAddr,
        /// No web client.
        #[cfg(feature = "web")]
        #[arg(long)]
        no_web: bool,
        /// CUDA device index (NVENC).
        #[arg(long, env = "GLIDEDESK_GPU", default_value_t = 0)]
        gpu: usize,
        /// Video encoder: NVENC (NVIDIA), VA-API (AMD, Intel, others) or software (x264).
        #[arg(long, value_enum, env = "GLIDEDESK_ENCODER", default_value = "auto")]
        encoder: encoder::Choice,
        /// DRM render node for VA-API (default: the first /dev/dri/renderD*).
        #[arg(long, env = "GLIDEDESK_RENDER_NODE")]
        render_node: Option<String>,
        /// Highest video bitrate in bits per second (mobile data, metered or remote links).
        /// Without it the bitrate follows the link: frames are encoded at a constant quality
        /// and only their ceiling adapts to the measured delay.
        #[arg(long, env = "GLIDEDESK_MAX_BITRATE")]
        max_bitrate: Option<u32>,
        /// Keep the full frame rate when the bitrate ceiling drops. By default a low ceiling
        /// for the picture size (a slow WAN link) also lowers the frame rate, down to 15 fps,
        /// so each frame keeps enough bits to stay sharp.
        #[arg(long)]
        no_fps_adapt: bool,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Size before the client says what it wants.
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        /// RDP user name (defaults to $USER).
        #[arg(long)]
        user: Option<String>,
        /// The password clients log in with; read from GLIDEDESK_PASSWORD when not given.
        /// Needed unless `--auth system` (and even then for RDP, see there).
        #[arg(long, env = "GLIDEDESK_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// What a login is checked against: the password given to the host, or the system
        /// account the host runs as (PAM on Linux, through /etc/pam.d/glidedesk as in
        /// scripts/pam.d; Windows' own password check). RDP's NLA checks the password itself,
        /// so with `system` RDP still needs `--password`, and is off without it.
        #[arg(long, value_enum, default_value = "password")]
        auth: AuthMode,
        /// TLS certificate (PEM chain) for RDP, the native protocol and the web page instead
        /// of the self-signed one (e.g. from Let's Encrypt: browsers then show no warning).
        /// Copied into the state folder at start; restart after renewing it.
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<std::path::PathBuf>,
        /// The certificate's private key (PEM).
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<std::path::PathBuf>,
        /// Send AVC444 (full chroma) to clients that support it. Off by default: mstsc shows
        /// our AVC444v2 stream blurred (to fix).
        #[arg(long)]
        avc444: bool,
        /// With AVC444, always send both views (no luma-only frames during motion).
        #[arg(long)]
        no_hybrid: bool,
        /// Send HEVC 4:4:4 (full chroma: sharp coloured text) to native clients that ask for
        /// it (`glidedesk --yuv444`). NVENC, on GPUs that encode 4:4:4; others get 4:2:0.
        #[arg(long)]
        yuv444: bool,
        /// Quality by region: small updates (typing, menus: text and UI) are encoded at a
        /// lower QP than the rest, from the compositor's damage rectangles. NVENC only.
        #[arg(long)]
        region_quality: bool,
        /// Do not re-encode a still image to raise its quality after motion.
        #[arg(long)]
        no_refine: bool,
        /// Frames sent and not yet acknowledged by the client. Low values keep the client's
        /// queue short (lower latency); higher ones absorb network jitter.
        #[arg(long, default_value_t = 2)]
        max_in_flight: u32,
        /// Also serve Moonlight clients (GameStream: 47989/tcp, 47984/tcp, RTSP 48010/tcp,
        /// video 47998/udp, control and input 47999/udp, sound 48000/udp).
        #[arg(long)]
        gamestream: bool,
        /// GameStream's HTTP port; the others move with it as in Sunshine (HTTPS 5 below,
        /// RTSP 21 above, video, control and sound 9, 10 and 11 above). Add the host in
        /// Moonlight as `address:port` when it is not the usual one.
        #[arg(long, default_value_t = gamestream::HTTP_PORT, value_parser = clap::value_parser!(u16).range(1024..65500))]
        gamestream_port: u16,
        /// Run as a session of `glidedesk-host daemon`: take the native clients it logged
        /// in, over $XDG_RUNTIME_DIR/glidedesk/handover.sock, and open no port of its own
        /// (no RDP, web or GameStream). Started by glidedesk-session@.service.
        #[cfg(target_os = "linux")]
        #[arg(long, conflicts_with = "no_native")]
        handover: bool,
        /// Serve a virtual monitor that takes the client's size and refresh rate instead of
        /// the physical monitors (also for a PC without one). Needs the SudoVDA driver;
        /// without it the monitors are served.
        #[cfg(target_os = "windows")]
        #[arg(long)]
        virtual_display: bool,
    },
    /// Run as the system service (as root): log native clients in with their system
    /// account (PAM) and give each user a headless GNOME session of their own, started
    /// through systemd (scripts/systemd).
    #[cfg(target_os = "linux")]
    Daemon {
        /// Native protocol address (TCP and UDP).
        #[arg(long, default_value = "0.0.0.0:3391")]
        native_bind: std::net::SocketAddr,
        /// GPUs to spread sessions over, comma-separated: CUDA device indices (NVENC) or
        /// VA-API render nodes, e.g. `0,1` or `/dev/dri/renderD128,/dev/dri/renderD129`.
        /// A new session gets the one with the fewest sessions. Without it, every session
        /// uses the encoder's default GPU.
        #[arg(long, value_delimiter = ',', value_parser = daemon::parse_gpu)]
        gpus: Vec<daemon::GpuSlot>,
        /// Always give this user's sessions this GPU (an index into --gpus). Repeatable.
        #[arg(long, value_name = "USER=INDEX", value_parser = daemon::parse_pin)]
        pin_gpu: Vec<(String, usize)>,
        /// Folder of the TLS identity.
        #[arg(long, default_value = "/var/lib/glidedesk")]
        state_dir: std::path::PathBuf,
        /// TLS certificate (PEM chain) instead of the self-signed one.
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<std::path::PathBuf>,
        /// The certificate's private key (PEM).
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<std::path::PathBuf>,
        /// The systemd template unit of a user's session (`NAME@USER.service`).
        #[arg(long, default_value = "glidedesk-session")]
        session_unit: String,
    },
    /// Run as a Windows service (as SYSTEM), so that the lock screen, the sign-in screen and
    /// UAC prompts are seen and used too.
    #[cfg(target_os = "windows")]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Run only the GameStream front end (pairing, server info, app list), for testing
    /// Moonlight clients without a session.
    #[command(hide = true)]
    GamestreamTest {
        #[arg(long, default_value = "127.0.0.1")]
        ip: std::net::IpAddr,
        #[arg(long)]
        state_dir: std::path::PathBuf,
        #[arg(long, env = "GLIDEDESK_PASSWORD", hide_env_values = true)]
        password: String,
        /// HTTP port; HTTPS goes 5 below, as in GameStream.
        #[arg(long, default_value_t = gamestream::HTTP_PORT)]
        port: u16,
    },
    /// Pair a Moonlight client with the running host: give the PIN the client shows. Without
    /// a PIN, lists the clients waiting.
    Pair {
        pin: Option<String>,
        /// Part of the device name, when several clients are waiting.
        #[arg(long)]
        device: Option<String>,
        /// The host's password; read from GLIDEDESK_PASSWORD when not given.
        #[arg(long, env = "GLIDEDESK_PASSWORD", hide_env_values = true)]
        password: String,
        #[arg(long, default_value_t = gamestream::HTTP_PORT)]
        port: u16,
    },
    #[cfg(feature = "gnome")]
    /// Capture the session for a few seconds, resize half way, and report frame rates.
    CaptureTest {
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        /// Size to switch to half way through.
        #[arg(long, default_value_t = 1280)]
        resize_width: u32,
        #[arg(long, default_value_t = 800)]
        resize_height: u32,
        #[arg(long, default_value_t = 6)]
        seconds: u64,
    },
    #[cfg(feature = "gnome")]
    /// Capture and encode with NVENC, write the elementary stream and report timings.
    EncodeTest {
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        #[arg(long, default_value_t = 0)]
        gpu: usize,
        #[arg(long)]
        hevc: bool,
        #[arg(long, default_value_t = 20_000_000)]
        bitrate: u32,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        #[arg(long, default_value = "/tmp/glidedesk-test.bin")]
        out: std::path::PathBuf,
    },
}

#[cfg(target_os = "windows")]
#[derive(Subcommand)]
enum ServiceAction {
    /// Install the service and start it (again, if it was installed). The arguments after
    /// `--` are those of `serve`, e.g. `service install -- serve --user me --password ...`.
    Install {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Stop the service and remove it.
    Uninstall,
    /// The service itself, started by the service manager.
    #[command(hide = true)]
    Run,
    /// The host in the console session, started by the service.
    #[command(hide = true)]
    Helper,
}

#[tokio::main]
async fn main() -> Result<()> {
    // The workspace enables more than one rustls crypto provider; pick one explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // No colour codes in a log file (the Windows service's).
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    let command = Cli::parse().command;
    // The service's helper takes its `serve` arguments from the service's settings.
    #[cfg(target_os = "windows")]
    let command = match command {
        Command::Service {
            action: ServiceAction::Helper,
        } => {
            winservice::set_helper();
            let args = winservice::helper_args()?;
            Cli::try_parse_from(std::iter::once("glidedesk-host".to_string()).chain(args))?.command
        }
        c => c,
    };
    match command {
        Command::Serve {
            #[cfg(feature = "x11")]
            x11,
            refresh,
            bind,
            native_bind,
            no_native,
            #[cfg(feature = "web")]
            web_bind,
            #[cfg(feature = "web")]
            no_web,
            gpu,
            encoder,
            render_node,
            max_bitrate,
            no_fps_adapt,
            fps,
            width,
            height,
            user,
            password,
            avc444,
            no_hybrid,
            yuv444,
            region_quality,
            no_refine,
            max_in_flight,
            gamestream,
            gamestream_port,
            auth,
            tls_cert,
            tls_key,
            #[cfg(target_os = "linux")]
            handover,
            #[cfg(target_os = "windows")]
            virtual_display,
        } => {
            #[cfg(target_os = "linux")]
            let handover = handover.then(handover::own_socket);
            #[cfg(not(target_os = "linux"))]
            let handover: Option<std::path::PathBuf> = None;
            #[cfg(feature = "gnome")]
            capture::set_refresh_rate(refresh);
            #[cfg(feature = "x11")]
            let session = match x11 {
                Some(display) => serve::SessionKind::X11 {
                    display: (!display.is_empty()).then_some(display),
                },
                None => default_session()?,
            };
            #[cfg(not(feature = "x11"))]
            let session = default_session()?;
            #[cfg(windows)]
            let config = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
            #[cfg(not(windows))]
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
                });
            let state_dir = config.unwrap_or_else(|| ".".into()).join("glidedesk");
            if let (Some(cert), Some(key)) = (&tls_cert, &tls_key) {
                rdp::install_identity(&state_dir, cert, key)?;
            }
            let username = user
                .or_else(|| std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok())
                .unwrap_or_default();
            if auth == AuthMode::System {
                anyhow::ensure!(!username.is_empty(), "--auth system needs the account name (--user)");
                auth::use_system_accounts(username.clone());
                tracing::info!(user = username, "Logins are checked against the system account");
            }
            let rdp_password = password.clone();
            if auth == AuthMode::System && rdp_password.is_none() && handover.is_none() {
                tracing::warn!("No --password: RDP is off (its NLA needs the password itself)");
            }
            let password = match (auth, password) {
                (_, Some(p)) => p,
                // The daemon checks the password of the clients it hands over.
                (_, None) if handover.is_some() => String::new(),
                (AuthMode::System, None) => String::new(),
                (AuthMode::Password, None) => {
                    anyhow::bail!("--password (or GLIDEDESK_PASSWORD) is needed, or --auth system")
                }
            };
            #[cfg(target_os = "linux")]
            let native_listen = match handover.clone() {
                Some(path) => native::Listen::Handover(path),
                None => native::Listen::Tcp,
            };
            #[cfg(not(target_os = "linux"))]
            let native_listen = native::Listen::Tcp;
            let alone = handover.is_none();
            serve::serve(serve::ServeOptions {
                session,
                pipeline: pipeline::Options {
                    avc444,
                    yuv444,
                    region_quality,
                    hybrid: !no_hybrid,
                    refine: !no_refine,
                    max_in_flight,
                    max_bitrate,
                    fps_adapt: !no_fps_adapt,
                },
                bind,
                native_bind: (!no_native).then_some(native_bind),
                native_listen,
                #[cfg(feature = "web")]
                web_bind: (!no_web && alone).then_some(web_bind),
                gpu,
                encoder,
                render_node,
                bitrate: max_bitrate.map_or(rate::START_BPS, |m| m.min(rate::START_BPS)),
                fps: fps.max(refresh),
                width,
                height,
                username,
                password,
                rdp: alone && (rdp_password.is_some() || auth == AuthMode::Password),
                state_dir,
                gamestream: (gamestream && alone).then(|| {
                    (
                        std::net::SocketAddr::new(bind.ip(), gamestream_port),
                        std::net::SocketAddr::new(bind.ip(), gamestream_port - 5),
                    )
                }),
                #[cfg(target_os = "windows")]
                virtual_display,
            })
            .await
        }
        #[cfg(target_os = "linux")]
        Command::Daemon {
            native_bind,
            gpus,
            pin_gpu,
            state_dir,
            tls_cert,
            tls_key,
            session_unit,
        } => {
            if let (Some(cert), Some(key)) = (&tls_cert, &tls_key) {
                rdp::install_identity(&state_dir, cert, key)?;
            }
            daemon::run(daemon::Options {
                bind: native_bind,
                state_dir,
                gpus,
                pins: pin_gpu,
                session_unit,
            })
            .await
        }
        #[cfg(target_os = "windows")]
        Command::Service { action } => service(action),
        Command::GamestreamTest {
            ip,
            state_dir,
            password,
            port,
        } => {
            gamestream::run(gamestream::Front {
                http_bind: std::net::SocketAddr::new(ip, port),
                https_bind: std::net::SocketAddr::new(ip, port - 5),
                state_dir,
                password,
                auth_limiter: std::sync::Arc::new(auth::RateLimiter::new()),
                hevc: true,
                stream: None,
                apps: None,
                input: None,
            })
            .await
        }
        Command::Pair {
            pin,
            device,
            password,
            port,
        } => {
            let out =
                gamestream::send_pin(port, &password, pin.as_deref(), device.as_deref()).await?;
            if pin.is_none() && out.is_empty() {
                println!("No Moonlight client is waiting for a PIN.");
            } else {
                print!("{out}");
            }
            Ok(())
        }
        #[cfg(feature = "gnome")]
        Command::CaptureTest {
            width,
            height,
            resize_width,
            resize_height,
            seconds,
        } => capture_test(width, height, resize_width, resize_height, seconds).await,
        #[cfg(feature = "gnome")]
        Command::EncodeTest {
            width,
            height,
            gpu,
            hevc,
            bitrate,
            seconds,
            out,
        } => encode_test(width, height, gpu, hevc, bitrate, seconds, out).await,
    }
}

/// `service install|uninstall|run`.
#[cfg(target_os = "windows")]
fn service(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install { mut args } => {
            let parsed = Cli::try_parse_from(
                std::iter::once("glidedesk-host".to_string()).chain(args.iter().cloned()),
            )?;
            let Command::Serve { user, password, .. } = parsed.command else {
                anyhow::bail!("the service runs `serve`: service install -- serve [options]");
            };
            anyhow::ensure!(
                user.is_some(),
                "give --user: the service runs as SYSTEM, whose name is not one to log in with"
            );
            // The service does not see this shell's environment: keep a password that came
            // from GLIDEDESK_PASSWORD with the arguments.
            let given = args
                .iter()
                .any(|a| a == "--password" || a.starts_with("--password="));
            if let Some(p) = password
                && !given
            {
                args.extend(["--password".to_string(), p]);
            }
            winservice::install(&args)
        }
        ServiceAction::Uninstall => winservice::uninstall(),
        ServiceAction::Run => winservice::run(),
        ServiceAction::Helper => anyhow::bail!("`service helper` is started by the service"),
    }
}

/// The session served when no other is chosen: Windows desktop on Windows, headless GNOME on Linux.
fn default_session() -> Result<serve::SessionKind> {
    #[cfg(target_os = "windows")]
    return Ok(serve::SessionKind::Windows);
    #[cfg(feature = "gnome")]
    return Ok(serve::SessionKind::Gnome);
    #[cfg(all(not(target_os = "windows"), not(feature = "gnome")))]
    anyhow::bail!("this build serves X11 displays only: pass --x11 DISPLAY")
}

#[cfg(feature = "gnome")]
async fn encode_test(
    w: u32,
    h: u32,
    gpu: usize,
    hevc: bool,
    bitrate: u32,
    seconds: u64,
    out: std::path::PathBuf,
) -> Result<()> {
    use std::io::Write;
    if let Some(hz) = std::env::var("GLIDEDESK_REFRESH")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        capture::set_refresh_rate(hz);
    }
    let conn = zbus::Connection::session().await?;
    let session = mutter::Session::start(&conn, mutter::CursorMode::Embedded).await?;
    let (tx, rx) = capture::latest_frame();
    let (cursor, _cursor_rx) = tokio::sync::watch::channel(None);
    let ctx = cudarc::driver::CudaContext::new(gpu)?;
    let cap = capture::Capture::start(
        session.node_id,
        w,
        h,
        capture::Sinks {
            frames: tx,
            cursor: std::sync::Arc::new(cursor),
        },
        Some(std::sync::Arc::clone(&ctx)),
    )?;
    let codec = if hevc {
        nvenc::Codec::Hevc
    } else {
        nvenc::Codec::H264
    };
    let worker = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut enc = nvenc::Encoder::new(
            ctx,
            nvenc::EncoderConfig {
                codec,
                width: w,
                height: h,
                fps: 60,
                bitrate,
                refs: 1,
                invalidation: false,
                yuv444: false,
                qp_map: false,
            },
        )?;
        let mut file = std::fs::File::create(&out)?;
        let start = Instant::now();
        let (mut n, mut bytes, mut total_us, mut max_us) = (0u64, 0usize, 0u128, 0u128);
        while start.elapsed() < Duration::from_secs(seconds) {
            let Ok(f) = rx.recv_timeout(Duration::from_millis(200)) else {
                continue;
            };
            if (f.width, f.height) != enc.size() {
                enc.resize(f.width, f.height)?;
            }
            let t = Instant::now();
            let e = match &f.data {
                capture::FrameData::Cpu { data, stride } => {
                    enc.encode_host(data, *stride as usize)?
                }
                capture::FrameData::Gpu(buf) => enc.encode_device(buf)?,
                capture::FrameData::Va(_) => anyhow::bail!("VA surfaces need the VA-API encoder"),
            };
            let us = t.elapsed().as_micros();
            total_us += us;
            max_us = max_us.max(us);
            n += 1;
            bytes += e.data.len();
            file.write_all(&e.data)?;
            if n <= 3 || n % 60 == 0 {
                info!(
                    n,
                    kb = e.data.len() / 1024,
                    key = e.keyframe,
                    encode_us = us as u64,
                    capture_to_bitstream_us = f.captured.elapsed().as_micros() as u64,
                    "Encoded"
                );
            }
        }
        if n > 0 {
            info!(
                frames = n,
                avg_encode_us = (total_us / n as u128) as u64,
                max_encode_us = max_us as u64,
                avg_kb = bytes as u64 / n / 1024,
                out = %out.display(),
                "Encode test done"
            );
        }
        Ok(())
    });
    let r = worker.await?;
    drop(cap);
    session.remote.stop().await.ok();
    r
}

#[cfg(feature = "gnome")]
async fn capture_test(w: u32, h: u32, rw: u32, rh: u32, seconds: u64) -> Result<()> {
    let conn = zbus::Connection::session().await?;
    let session = mutter::Session::start(&conn, mutter::CursorMode::Metadata).await?;
    info!(node = session.node_id, "Virtual monitor stream ready");
    let (tx, rx) = capture::latest_frame();
    let (cursor, _cursor_rx) = tokio::sync::watch::channel(None);
    let cap = capture::Capture::start(
        session.node_id,
        w,
        h,
        capture::Sinks {
            frames: tx,
            cursor: std::sync::Arc::new(cursor),
        },
        None,
    )?;

    let total = Duration::from_secs(seconds);
    let start = Instant::now();
    let mut resized = false;
    let mut last = (0u32, 0u32);
    let mut count = 0u32;
    let mut window = Instant::now();
    while start.elapsed() < total {
        if !resized && start.elapsed() > total / 2 {
            cap.resize(rw, rh);
            resized = true;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(f) => {
                if (f.width, f.height) != last {
                    info!(
                        width = f.width,
                        height = f.height,
                        t_ms = start.elapsed().as_millis() as u64,
                        "Frame size"
                    );
                    last = (f.width, f.height);
                }
                count += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => return Err(e.into()),
        }
        if window.elapsed() >= Duration::from_secs(1) {
            info!(fps = count, "Frames in the last second");
            count = 0;
            window = Instant::now();
        }
    }
    drop(cap);
    session.remote.stop().await.ok();
    Ok(())
}

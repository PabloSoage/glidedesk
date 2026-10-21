mod capture;
mod color;
mod gpu;
mod input;
mod mutter;
mod nvenc;
mod rdp;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

#[derive(Parser)]
#[command(version, about = "glidedesk session host")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the session over RDP.
    Serve {
        /// Refresh rate of the virtual monitor (Hz). Higher means fresher frames and lower
        /// latency at the cost of more encoding while things move.
        #[arg(long, env = "GLIDEDESK_REFRESH", default_value_t = 120)]
        refresh: u32,
        #[arg(long, default_value = "0.0.0.0:3389")]
        bind: std::net::SocketAddr,
        #[arg(long, default_value_t = 0)]
        gpu: usize,
        /// Video bitrate in bits per second.
        #[arg(long, default_value_t = 30_000_000)]
        bitrate: u32,
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
        /// RDP password; read from GLIDEDESK_PASSWORD when not given.
        #[arg(long, env = "GLIDEDESK_PASSWORD", hide_env_values = true)]
        password: String,
        /// Send 4:2:0 only (AVC420) even to clients that support AVC444.
        #[arg(long)]
        no_avc444: bool,
        /// With AVC444, always send both views (no luma-only frames during motion).
        #[arg(long)]
        no_hybrid: bool,
        /// Frames sent and not yet acknowledged by the client. Low values keep the client's
        /// queue short (lower latency); higher ones absorb network jitter.
        #[arg(long, default_value_t = 2)]
        max_in_flight: u32,
    },
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Serve {
            refresh,
            bind,
            gpu,
            bitrate,
            fps,
            width,
            height,
            user,
            password,
            no_avc444,
            no_hybrid,
            max_in_flight,
        } => {
            capture::set_refresh_rate(refresh);
            let state_dir = std::env::var_os("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
                })
                .unwrap_or_else(|| ".".into())
                .join("glidedesk");
            rdp::serve(rdp::ServeOptions {
                avc444: !no_avc444,
                hybrid: !no_hybrid,
                max_in_flight,
                bind,
                gpu,
                bitrate,
                fps: fps.max(refresh),
                width,
                height,
                username: user
                    .or_else(|| std::env::var("USER").ok())
                    .unwrap_or_default(),
                password,
                state_dir,
            })
            .await
        }
        Command::CaptureTest {
            width,
            height,
            resize_width,
            resize_height,
            seconds,
        } => capture_test(width, height, resize_width, resize_height, seconds).await,
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
    let (tx, rx) = mpsc::sync_channel(1);
    let (cursor, _cursor_rx) = tokio::sync::watch::channel(None);
    let ctx = cudarc::driver::CudaContext::new(gpu)?;
    let cap = capture::Capture::start(
        session.node_id,
        w,
        h,
        capture::Sinks { frames: tx, cursor },
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

async fn capture_test(w: u32, h: u32, rw: u32, rh: u32, seconds: u64) -> Result<()> {
    let conn = zbus::Connection::session().await?;
    let session = mutter::Session::start(&conn, mutter::CursorMode::Metadata).await?;
    info!(node = session.node_id, "Virtual monitor stream ready");
    let (tx, rx) = mpsc::sync_channel(1);
    let (cursor, _cursor_rx) = tokio::sync::watch::channel(None);
    let cap = capture::Capture::start(
        session.node_id,
        w,
        h,
        capture::Sinks { frames: tx, cursor },
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

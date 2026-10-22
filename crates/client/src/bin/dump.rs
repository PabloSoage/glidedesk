//! Connects like a client (RDP, or the native protocol with `--native`), writes the video
//! stream to a file and reports frame rate.
//! A test tool for the client core (no decoder, no window).

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use glidedesk_client::gfx::{VideoCodec, VideoSink};
use glidedesk_client::session::{ConnectOptions, Input, run};

#[derive(Parser)]
struct Args {
    host: String,
    /// Default 3389, or 3391 with --native.
    #[arg(long)]
    port: Option<u16>,
    /// Use the native low-latency protocol instead of RDP.
    #[arg(long)]
    native: bool,
    /// With --native: ask for HEVC instead of H.264.
    #[arg(long)]
    hevc: bool,
    #[arg(long)]
    user: String,
    #[arg(long, env = "GLIDEDESK_PASSWORD", hide_env_values = true)]
    password: String,
    #[arg(long, default_value_t = 1280)]
    width: u16,
    #[arg(long, default_value_t = 720)]
    height: u16,
    #[arg(long, default_value_t = 8)]
    seconds: u64,
    /// Ask for this size half way through (WxH), to test resizing.
    #[arg(long)]
    resize: Option<String>,
    #[arg(long, default_value = "glidedesk-dump.h264")]
    out: std::path::PathBuf,
}

struct FileSink {
    file: std::fs::File,
    frames: u64,
    bytes: u64,
    resets: Vec<(u32, u32)>,
}

impl VideoSink for FileSink {
    fn reset(&mut self, width: u32, height: u32, codec: VideoCodec) {
        self.resets.push((width, height));
        eprintln!("reset {width}x{height} {codec:?}");
    }
    fn frame(&mut self, data: &[u8]) {
        self.frames += 1;
        self.bytes += data.len() as u64;
        let _ = self.file.write_all(data);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let a = Args::parse();
    let sink = Arc::new(Mutex::new(FileSink {
        file: std::fs::File::create(&a.out)?,
        frames: 0,
        bytes: 0,
        resets: Vec::new(),
    }));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let shared: glidedesk_client::gfx::SharedSink = sink.clone();
    let session = if a.native {
        let opts = glidedesk_client::native::NativeOptions {
            host: a.host,
            port: a.port.unwrap_or(glidedesk_proto::DEFAULT_PORT),
            username: a.user,
            password: a.password,
            width: a.width,
            height: a.height,
            codecs: if a.hevc {
                vec![VideoCodec::Hevc, VideoCodec::H264]
            } else {
                vec![VideoCodec::H264]
            },
            refresh: 60,
            pinned: None,
        };
        tokio::spawn(async move {
            glidedesk_client::native::run(opts, shared, rx)
                .await
                .map(|_| ())
        })
    } else {
        let opts = ConnectOptions {
            host: a.host,
            port: a.port.unwrap_or(3389),
            username: a.user,
            password: a.password,
            width: a.width,
            height: a.height,
        };
        tokio::spawn(run(opts, shared, rx))
    };
    let start = Instant::now();
    let total = Duration::from_secs(a.seconds);
    let mut resized = false;
    while start.elapsed() < total {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Keep moving the pointer so the remote cursor and input path get exercised.
        let t = start.elapsed().as_millis() as u16;
        let _ = tx.send(Input::MouseMove {
            x: 100 + t % 400,
            y: 100,
        });
        if !resized && start.elapsed() > total / 2 {
            if let Some((w, h)) = a.resize.as_deref().and_then(|s| s.split_once('x')) {
                let _ = tx.send(Input::Resize {
                    width: w.parse()?,
                    height: h.parse()?,
                });
            }
            resized = true;
        }
        if session.is_finished() {
            break;
        }
    }
    let _ = tx.send(Input::Disconnect);
    let _ = tokio::time::timeout(Duration::from_secs(3), session).await;
    let s = sink.lock().unwrap();
    println!(
        "frames {} ({:.1} fps), {:.2} MB, resets {:?}, out {}",
        s.frames,
        s.frames as f64 / start.elapsed().as_secs_f64(),
        s.bytes as f64 / 1e6,
        s.resets,
        a.out.display()
    );
    Ok(())
}

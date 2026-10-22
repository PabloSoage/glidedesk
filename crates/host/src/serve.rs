//! A remote session: the headless GNOME Shell's virtual monitor, captured and encoded once,
//! served to the RDP and the native front ends (one client at a time).
//!
//! Threads: the tokio runtime runs both front ends and the input task; the PipeWire threads
//! capture video and audio; the pipeline thread encodes the newest frame and hands it to the
//! output of whichever front end has the client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use tracing::{info, warn};

use crate::capture::{Capture, Sinks};
use crate::encoder;
use crate::input::InputForwarder;
use crate::mutter;
use crate::pipeline::{self, Output, Pipeline};

pub struct ServeOptions {
    pub pipeline: pipeline::Options,
    pub bind: SocketAddr,
    /// Native protocol (TCP + UDP); `None` turns it off.
    pub native_bind: Option<SocketAddr>,
    pub gpu: usize,
    /// Video encoder (NVENC, VA-API or software).
    pub encoder: encoder::Choice,
    /// DRM render node for VA-API; the first one when `None`.
    pub render_node: Option<String>,
    pub bitrate: u32,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub username: String,
    pub password: String,
    pub state_dir: PathBuf,
}

/// Output indexes in the pipeline.
const RDP: usize = 0;
const NATIVE: usize = 1;

pub async fn serve(opts: ServeOptions) -> Result<()> {
    let conn = zbus::Connection::session().await?;
    // GLIDEDESK_CURSOR=hidden|embedded for experiments; metadata (client-drawn) by default.
    let cursor_mode = match std::env::var("GLIDEDESK_CURSOR").as_deref() {
        Ok("hidden") => mutter::CursorMode::Hidden,
        Ok("embedded") => mutter::CursorMode::Embedded,
        _ => mutter::CursorMode::Metadata,
    };
    let session = mutter::Session::start(&conn, cursor_mode).await?;
    info!(
        node = session.node_id,
        ?cursor_mode,
        "Virtual monitor ready"
    );
    {
        let conn = conn.clone();
        tokio::spawn(async move { mutter::leave_startup_overview(&conn).await });
    }

    let shared = Arc::new(pipeline::Shared::default());
    let (w, h) = pipeline::clamp_size(opts.width, opts.height);
    *shared.current_size.lock().unwrap() = (w, h);
    let (backend, cuda) = encoder::select(
        opts.encoder,
        opts.gpu,
        opts.render_node.clone(),
        opts.fps,
        opts.bitrate,
    )?;
    info!(
        encoder = backend.name(),
        hevc = backend.supports_hevc(),
        "Video encoder"
    );
    let hevc = backend.supports_hevc();
    let (tx, rx) = crate::capture::latest_frame();
    let (cursor_tx, cursor_rx) = tokio::sync::watch::channel(None);
    let capture = Capture::start(
        session.node_id,
        w,
        h,
        Sinks {
            frames: tx,
            cursor: cursor_tx,
        },
        cuda,
    )?;

    crate::rdp::ensure_identity(&opts.state_dir)?;
    let (rdp_output, rdp_server) = crate::rdp::new(Arc::clone(&shared), RDP);
    let mut outputs: Vec<Box<dyn Output>> = vec![Box::new(rdp_output)];
    let native = match opts.native_bind {
        Some(bind) => {
            let (output, server) = crate::native::new(Arc::clone(&shared), NATIVE, bind).await?;
            outputs.push(Box::new(output));
            Some((server, bind))
        }
        None => None,
    };
    let pipeline = Pipeline::new(
        Arc::clone(&shared),
        capture,
        rx,
        backend,
        outputs,
        opts.pipeline,
    );
    let pipeline_thread = std::thread::Builder::new()
        .name("pipeline".into())
        .spawn(move || pipeline.run())?;

    let audio = match crate::audio::start() {
        Ok(a) => Some(a),
        Err(e) => {
            warn!(%e, "No audio");
            None
        }
    };
    let input = InputForwarder::spawn(session.remote.clone(), session.stream_path.clone());

    let rdp = rdp_server.run(crate::rdp::Front {
        bind: opts.bind,
        username: opts.username.clone(),
        password: opts.password.clone(),
        state_dir: opts.state_dir.clone(),
        cursor: cursor_rx.clone(),
        audio: audio.clone(),
        input: input.clone(),
        remote: session.remote.clone(),
    });
    let native = async {
        match native {
            Some((server, bind)) => {
                server
                    .run(crate::native::Front {
                        bind,
                        username: opts.username.clone(),
                        password: opts.password.clone(),
                        state_dir: opts.state_dir.clone(),
                        hevc,
                        cursor: cursor_rx.clone(),
                        audio: audio.clone(),
                        input: input.clone(),
                    })
                    .await
            }
            None => std::future::pending().await,
        }
    };
    let r = tokio::select! {
        r = rdp => r,
        r = native => r,
    };
    shared.stop.store(true, Ordering::Relaxed);
    let _ = pipeline_thread.join();
    session.remote.stop().await.ok();
    r
}

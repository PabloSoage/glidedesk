//! A remote session: frames from the session backend, encoded once, served to the RDP and
//! the native front ends (one client at a time).
//!
//! Session backends:
//! - GNOME (`gnome` feature): a virtual monitor of the headless GNOME Shell through Mutter's
//!   ScreenCast/RemoteDesktop and PipeWire; the desktop follows the client's size.
//! - X11 (`x11` feature): an existing X display (Termux's termux-x11, a Linux desktop),
//!   captured with GetImage/DAMAGE, input through XTEST; its size is the display's.
//!
//! Threads: the tokio runtime runs both front ends and the input task; the capture threads
//! produce frames; the pipeline thread encodes the newest frame and hands it to the output
//! of whichever front end has the client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

use crate::encoder;
use crate::frame::{CursorEvent, FrameReceiver, Source};
use crate::input::{InputBackend, InputForwarder};
use crate::pipeline::{self, Output, Pipeline};

/// Where the desktop comes from.
#[derive(Clone, Debug)]
pub enum SessionKind {
    #[cfg(feature = "gnome")]
    Gnome,
    #[cfg(feature = "x11")]
    X11 { display: Option<String> },
}

pub struct ServeOptions {
    pub session: SessionKind,
    pub pipeline: pipeline::Options,
    pub bind: SocketAddr,
    /// Native protocol (TCP + UDP); `None` turns it off.
    pub native_bind: Option<SocketAddr>,
    /// Web client (HTTPS + WebRTC); `None` turns it off.
    #[cfg(feature = "web")]
    pub web_bind: Option<SocketAddr>,
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

/// Output index of RDP in the pipeline (the others follow in the order they are added).
const RDP: usize = 0;

/// What a session backend provides to the front ends.
struct Parts {
    source: Box<dyn Source>,
    frames: FrameReceiver,
    cursor: watch::Receiver<Option<CursorEvent>>,
    input: Box<dyn InputBackend>,
    audio: Option<broadcast::Sender<Vec<u8>>>,
    clipboard: Option<Box<dyn ironrdp_server::CliprdrServerFactory>>,
    /// The same clipboard for the native front end.
    text_clipboard: Option<Arc<dyn crate::native::TextClipboard>>,
    /// Size the session starts with.
    size: (u32, u32),
    /// Run when the server stops.
    stop: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
}

#[cfg(feature = "gnome")]
async fn gnome(
    opts: &ServeOptions,
    cuda: Option<Arc<cudarc::driver::CudaContext>>,
) -> Result<Parts> {
    use crate::capture::{Capture, Sinks};
    use crate::mutter;

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
    let (w, h) = pipeline::clamp_size(opts.width, opts.height);
    let (tx, rx) = crate::frame::latest_frame();
    let (cursor_tx, cursor_rx) = watch::channel(None);
    let sinks = Sinks {
        frames: tx,
        cursor: Arc::new(cursor_tx),
    };
    let capture = Capture::start(session.node_id, w, h, sinks.clone(), cuda.clone())?;
    let followed = crate::gnome::Followed::new(capture, (w, h));
    let (live_tx, live) = watch::channel(Arc::new(mutter::Live {
        remote: session.remote.clone(),
        stream_path: session.stream_path.clone(),
    }));
    tokio::spawn(crate::gnome::supervise(
        conn.clone(),
        cursor_mode,
        live_tx,
        Arc::clone(&followed),
        sinks,
        cuda,
    ));
    let audio = match crate::audio::start() {
        Ok(a) => Some(a),
        Err(e) => {
            warn!(%e, "No audio");
            None
        }
    };
    let clipboard = crate::clipboard::ClipboardFactory::spawn(live.clone());
    let hub = clipboard.hub();
    let current = live.clone();
    Ok(Parts {
        source: Box::new(crate::gnome::FollowedSource(followed)),
        frames: rx,
        cursor: cursor_rx,
        input: Box::new(crate::input::MutterInput::new(live)),
        audio,
        clipboard: Some(Box::new(clipboard)),
        text_clipboard: Some(Arc::new(hub)),
        size: (w, h),
        stop: Some(Box::pin(async move {
            let remote = current.borrow().remote.clone();
            remote.stop().await.ok();
        })),
    })
}

#[cfg(feature = "x11")]
fn x11(display: Option<&str>, fps: u32) -> Result<Parts> {
    let s = crate::x11session::start(display, fps)?;
    Ok(Parts {
        size: s.size,
        source: Box::new(s.source),
        frames: s.frames,
        cursor: s.cursor,
        input: Box::new(s.input),
        audio: crate::pulse::start()
            .map_err(|e| warn!(%e, "No audio"))
            .ok(),
        clipboard: None,
        text_clipboard: None,
        stop: None,
    })
}

pub async fn serve(opts: ServeOptions) -> Result<()> {
    let (backend, cuda) = encoder::select(
        opts.encoder,
        opts.gpu,
        opts.render_node.clone(),
        opts.fps,
        opts.bitrate,
    )?;
    let hevc = backend.supports_hevc();
    info!(encoder = backend.name(), hevc, "Video encoder");

    let parts = match &opts.session {
        #[cfg(feature = "gnome")]
        SessionKind::Gnome => gnome(&opts, cuda).await?,
        #[cfg(feature = "x11")]
        SessionKind::X11 { display } => {
            drop(cuda);
            x11(display.as_deref(), opts.fps)?
        }
    };
    let shared = Arc::new(pipeline::Shared::default());
    *shared.current_size.lock().unwrap() = parts.size;

    crate::rdp::ensure_identity(&opts.state_dir)?;
    let (rdp_output, rdp_server) = crate::rdp::new(Arc::clone(&shared), RDP);
    let mut outputs: Vec<Box<dyn Output>> = vec![Box::new(rdp_output)];
    let native = match opts.native_bind {
        Some(bind) => {
            let (output, server) =
                crate::native::new(Arc::clone(&shared), outputs.len(), bind).await?;
            outputs.push(Box::new(output));
            Some((server, bind))
        }
        None => None,
    };
    #[cfg(feature = "web")]
    let web = match opts.web_bind {
        Some(bind) => {
            let (output, server) =
                crate::web::new(Arc::clone(&shared), outputs.len(), bind).await?;
            outputs.push(Box::new(output));
            Some((server, bind))
        }
        None => None,
    };
    let max_bitrate = opts.pipeline.max_bitrate;
    let pipeline = Pipeline::new(
        Arc::clone(&shared),
        parts.source,
        parts.frames,
        backend,
        outputs,
        opts.pipeline,
    );
    let pipeline_thread = std::thread::Builder::new()
        .name("pipeline".into())
        .spawn(move || pipeline.run())?;

    let input = InputForwarder::spawn(parts.input);
    let rdp = rdp_server.run(crate::rdp::Front {
        bind: opts.bind,
        username: opts.username.clone(),
        password: opts.password.clone(),
        state_dir: opts.state_dir.clone(),
        cursor: parts.cursor.clone(),
        audio: parts.audio.clone(),
        input: input.clone(),
        clipboard: parts.clipboard,
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
                        cursor: parts.cursor.clone(),
                        audio: parts.audio.clone(),
                        input: input.clone(),
                        clipboard: parts.text_clipboard.clone(),
                    })
                    .await
            }
            None => std::future::pending().await,
        }
    };
    #[cfg(feature = "web")]
    let web = async {
        match web {
            Some((server, bind)) => {
                server
                    .run(crate::web::Front {
                        bind,
                        username: opts.username.clone(),
                        password: opts.password.clone(),
                        state_dir: opts.state_dir.clone(),
                        hevc,
                        max_bitrate,
                        cursor: parts.cursor.clone(),
                        audio: parts.audio.clone(),
                        input: input.clone(),
                        clipboard: parts.text_clipboard.clone(),
                    })
                    .await
            }
            None => std::future::pending().await,
        }
    };
    #[cfg(not(feature = "web"))]
    let web = std::future::pending::<Result<()>>();
    let r = tokio::select! {
        r = rdp => r,
        r = native => r,
        r = web => r,
    };
    shared.stop.store(true, Ordering::Relaxed);
    let _ = pipeline_thread.join();
    if let Some(stop) = parts.stop {
        stop.await;
    }
    r
}

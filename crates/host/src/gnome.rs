//! The GNOME session kept alive.
//!
//! GNOME Shell shows a "Stop Screen Sharing" button in the top bar for every remote
//! desktop session (GNOME 46 has no way to hide it), and Mutter may also close a session
//! on its own. When the session closes, a new one is started on a new virtual monitor of
//! the same size, and the capture, input and clipboard move to it: clients stay connected
//! and only see the desktop come back.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cudarc::driver::CudaContext;
use futures_util::StreamExt;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::capture::{Capture, Sinks};
use crate::frame::Source;
use crate::mutter::{CursorMode, Live, Session};

/// The capture of whichever session is current.
pub struct Followed {
    capture: Mutex<Capture>,
    /// Size last asked for, for the next session's monitor.
    size: Mutex<(u32, u32)>,
}

impl Followed {
    pub fn new(capture: Capture, size: (u32, u32)) -> Arc<Self> {
        Arc::new(Self {
            capture: Mutex::new(capture),
            size: Mutex::new(size),
        })
    }
}

/// The pipeline's [`Source`]: forwards to the current capture.
pub struct FollowedSource(pub Arc<Followed>);

impl Source for FollowedSource {
    fn resize(&self, width: u32, height: u32) {
        *self.0.size.lock().unwrap() = (width, height);
        Source::resize(&*self.0.capture.lock().unwrap(), width, height);
    }

    fn set_refresh(&self, hz: u32) -> bool {
        self.0.capture.lock().unwrap().set_refresh(hz)
    }
}

/// Watches the current session and replaces it when Mutter closes it.
pub async fn supervise(
    conn: zbus::Connection,
    cursor: CursorMode,
    live: watch::Sender<Arc<Live>>,
    followed: Arc<Followed>,
    sinks: Sinks,
    cuda: Option<Arc<CudaContext>>,
) {
    loop {
        let remote = live.borrow().remote.clone();
        let Ok(mut closed) = remote.receive_closed().await else {
            warn!("Cannot watch the remote desktop session; it will not be restarted");
            return;
        };
        closed.next().await;
        warn!("Mutter closed the remote desktop session; starting a new one");
        let session = loop {
            match Session::start(&conn, cursor).await {
                Ok(s) => break s,
                Err(e) => {
                    warn!(%e, "Starting a new remote desktop session failed; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        let (w, h) = *followed.size.lock().unwrap();
        let (node, sinks, cuda) = (session.node_id, sinks.clone(), cuda.clone());
        match tokio::task::spawn_blocking(move || Capture::start(node, w, h, sinks, cuda)).await {
            Ok(Ok(capture)) => {
                // The old capture's thread ends as it is dropped.
                let old = std::mem::replace(&mut *followed.capture.lock().unwrap(), capture);
                tokio::task::spawn_blocking(move || drop(old));
            }
            Ok(Err(e)) => warn!(%e, "Capturing the new session failed"),
            Err(e) => warn!(%e, "Capturing the new session failed"),
        }
        info!(node, "New remote desktop session");
        live.send_replace(Arc::new(Live {
            remote: session.remote,
            stream_path: session.stream_path,
        }));
    }
}

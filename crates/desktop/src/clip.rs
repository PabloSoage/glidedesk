//! Clipboard shared with the session (native protocol): text, and files with servers from
//! protocol version 4.
//!
//! The local clipboard is polled (no portable change notification): new text, or a new
//! list of copied files, goes to the server. What the session copies is put on the local
//! clipboard (files once the client has fetched them, see `glidedesk_proto::files`). The
//! last text and file list seen either way are remembered, so neither side echoes the
//! other's copy back.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use glidedesk_client::session::Input;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

#[derive(Default)]
struct Seen {
    text: Option<String>,
    files: Option<Vec<PathBuf>>,
}

#[derive(Clone)]
pub struct Clipboard {
    last: Arc<Mutex<Seen>>,
    /// Kept open: on X11 what we put on the clipboard is served by this object and
    /// disappears with it.
    owner: Arc<Mutex<Option<arboard::Clipboard>>>,
}

/// The copied files, if the clipboard holds files.
fn file_list(cb: &mut arboard::Clipboard) -> Option<Vec<PathBuf>> {
    cb.get().file_list().ok().filter(|l| !l.is_empty())
}

impl Clipboard {
    /// Starts watching the local clipboard; new copies go to `input`.
    pub fn start(input: UnboundedSender<Input>) -> Self {
        let last: Arc<Mutex<Seen>> = Arc::default();
        let seen = Arc::clone(&last);
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let Ok(mut cb) = arboard::Clipboard::new() else {
                    return;
                };
                // What is on the clipboard at start is not a new copy.
                {
                    let mut s = seen.lock().unwrap();
                    s.files = file_list(&mut cb);
                    s.text = cb.get_text().ok();
                }
                loop {
                    std::thread::sleep(Duration::from_millis(400));
                    if let Some(files) = file_list(&mut cb) {
                        let mut s = seen.lock().unwrap();
                        if s.files.as_ref() != Some(&files) {
                            debug!(files = files.len(), "Local copy of files");
                            s.files = Some(files.clone());
                            if input.send(Input::Files(files)).is_err() {
                                return;
                            }
                        }
                        continue;
                    }
                    let Ok(text) = cb.get_text() else { continue };
                    let mut s = seen.lock().unwrap();
                    s.files = None;
                    if s.text.as_deref() != Some(text.as_str()) {
                        debug!(bytes = text.len(), "Local copy");
                        s.text = Some(text.clone());
                        if input.send(Input::Clipboard(text)).is_err() {
                            return;
                        }
                    }
                }
            })
            .ok();
        Self {
            last,
            owner: Arc::new(Mutex::new(arboard::Clipboard::new().ok())),
        }
    }

    /// Text copied in the session.
    pub fn remote(&self, text: String) {
        let mut s = self.last.lock().unwrap();
        if s.text.as_deref() == Some(text.as_str()) {
            return;
        }
        if let Some(cb) = self.owner.lock().unwrap().as_mut()
            && cb.set_text(text.clone()).is_ok()
        {
            s.text = Some(text);
            s.files = None;
        }
    }

    /// Files copied in the session, fetched to these paths.
    pub fn remote_files(&self, paths: Vec<PathBuf>) {
        let mut s = self.last.lock().unwrap();
        let Some(cb) = self.owner.lock().unwrap().as_mut() else {
            return;
        };
        match cb.set().file_list(&paths) {
            Ok(()) => s.files = Some(paths),
            Err(e) => warn!(%e, "Could not put the files on the clipboard"),
        }
    }
}

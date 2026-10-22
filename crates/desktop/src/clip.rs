//! Text clipboard shared with the session (native protocol).
//!
//! The local clipboard is polled (no portable change notification) and new text goes to the
//! server; text copied in the session is put on the local clipboard. The last text seen
//! either way is remembered, so neither side echoes the other's copy back.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use glidedesk_client::session::Input;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

#[derive(Clone)]
pub struct Clipboard {
    last: Arc<Mutex<Option<String>>>,
    /// Kept open: on X11 the text we put on the clipboard is served by this object and
    /// disappears with it.
    owner: Arc<Mutex<Option<arboard::Clipboard>>>,
}

impl Clipboard {
    /// Starts watching the local clipboard; new text goes to `input`.
    pub fn start(input: UnboundedSender<Input>) -> Self {
        let last: Arc<Mutex<Option<String>>> = Arc::default();
        let seen = Arc::clone(&last);
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let Ok(mut cb) = arboard::Clipboard::new() else {
                    return;
                };
                // What is on the clipboard at start is not a new copy.
                *seen.lock().unwrap() = cb.get_text().ok();
                loop {
                    std::thread::sleep(Duration::from_millis(400));
                    let Ok(text) = cb.get_text() else { continue };
                    let mut s = seen.lock().unwrap();
                    if s.as_deref() != Some(text.as_str()) {
                        debug!(bytes = text.len(), "Local copy");
                        *s = Some(text.clone());
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
        if s.as_deref() == Some(text.as_str()) {
            return;
        }
        if let Some(cb) = self.owner.lock().unwrap().as_mut()
            && cb.set_text(text.clone()).is_ok()
        {
            *s = Some(text);
        }
    }
}

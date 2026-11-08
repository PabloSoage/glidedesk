//! Clipboard between the clients (RDP and native) and the Windows session (text).
//!
//! A thread watches the clipboard sequence number. Text copied in Windows goes to native
//! clients at once and is announced to the RDP client (which asks for it when it pastes).
//! Text copied on an RDP client is fetched as soon as it is announced and put on the Windows
//! clipboard, as is a native client's.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::IntoOwned as _;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

/// How often the clipboard is checked for changes.
const POLL: Duration = Duration::from_millis(250);

#[derive(Debug)]
enum Msg {
    /// The RDP client copied; whether text is among its formats.
    Copied { text: bool },
    /// The RDP client wants our clipboard in this format.
    DataRequest(ClipboardFormatId),
    /// The RDP client's answer to our paste request.
    Data(Option<String>),
    /// A native client copied this text.
    NativeText(String),
}

type Events = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ServerEvent>>>>;

#[derive(Clone)]
pub struct WinClipboard {
    events: Events,
    to_thread: mpsc::Sender<Msg>,
    from_session: broadcast::Sender<String>,
}

impl WinClipboard {
    pub fn start() -> anyhow::Result<Self> {
        let events: Events = Arc::default();
        let (to_thread, rx) = mpsc::channel();
        let (from_session, _) = broadcast::channel(4);
        let (ev, to_native) = (Arc::clone(&events), from_session.clone());
        let clipboard = arboard::Clipboard::new()?;
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || run(clipboard, &rx, &ev, &to_native))?;
        info!("Clipboard enabled");
        Ok(Self {
            events,
            to_thread,
            from_session,
        })
    }
}

fn send(events: &Events, message: ClipboardMessage) {
    if let Some(tx) = events.lock().unwrap().as_ref() {
        let _ = tx.send(ServerEvent::Clipboard(message));
    }
}

fn sequence() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetClipboardSequenceNumber() }
}

fn run(
    mut clipboard: arboard::Clipboard,
    rx: &mpsc::Receiver<Msg>,
    events: &Events,
    to_native: &broadcast::Sender<String>,
) {
    let mut seen = sequence();
    let set = |clipboard: &mut arboard::Clipboard, text: String| {
        if let Err(e) = clipboard.set_text(text) {
            warn!(%e, "Could not set the Windows clipboard");
        }
        // Our own change: not announced back.
        sequence()
    };
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Msg::Copied { text }) => {
                if text {
                    send(
                        events,
                        ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT),
                    );
                }
            }
            Ok(Msg::Data(Some(text))) => seen = set(&mut clipboard, text),
            Ok(Msg::Data(None)) => debug!("The client sent no clipboard text"),
            Ok(Msg::NativeText(text)) => seen = set(&mut clipboard, text),
            Ok(Msg::DataRequest(format)) => {
                let text = (format == ClipboardFormatId::CF_UNICODETEXT)
                    .then(|| clipboard.get_text().ok())
                    .flatten();
                let response = match text {
                    Some(t) => FormatDataResponse::new_unicode_string(&t),
                    None => FormatDataResponse::new_error(),
                };
                send(
                    events,
                    ClipboardMessage::SendFormatData(response.into_owned()),
                );
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let now = sequence();
        if now == seen {
            continue;
        }
        seen = now;
        // Only text for now.
        let Ok(text) = clipboard.get_text() else {
            continue;
        };
        debug!("Windows clipboard changed");
        let _ = to_native.send(text);
        send(
            events,
            ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat::new(
                ClipboardFormatId::CF_UNICODETEXT,
            )]),
        );
    }
}

impl crate::native::TextClipboard for WinClipboard {
    fn set(&self, text: String) {
        let _ = self.to_thread.send(Msg::NativeText(text));
    }

    fn subscribe(&self) -> broadcast::Receiver<String> {
        self.from_session.subscribe()
    }
}

impl ServerEventSender for WinClipboard {
    fn set_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<ServerEvent>) {
        *self.events.lock().unwrap() = Some(sender);
    }
}

impl CliprdrBackendFactory for WinClipboard {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend {
            to_thread: self.to_thread.clone(),
        })
    }
}

impl CliprdrServerFactory for WinClipboard {}

#[derive(Debug)]
struct Backend {
    to_thread: mpsc::Sender<Msg>,
}

ironrdp_core::impl_as_any!(Backend);

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        debug!("Clipboard channel ready");
    }

    fn on_request_format_list(&mut self) {}

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let text = available_formats
            .iter()
            .any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT);
        let _ = self.to_thread.send(Msg::Copied { text });
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let _ = self.to_thread.send(Msg::DataRequest(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let text = if response.is_error() {
            None
        } else {
            response.to_unicode_string().ok()
        };
        let _ = self.to_thread.send(Msg::Data(text));
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

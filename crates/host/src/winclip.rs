//! Clipboard between the clients (RDP and native) and the Windows session: text, and files
//! with RDP clients (as `mstsc` does, see [`crate::filecopy`]).
//!
//! A thread watches the clipboard sequence number. Text copied in Windows goes to native
//! clients at once and is announced to the RDP client (which asks for it when it pastes).
//! Text copied on an RDP client is fetched as soon as it is announced and put on the Windows
//! clipboard, as is a native client's. Files copied in Windows are offered to the RDP client,
//! which reads them when it pastes; files copied on the RDP client are fetched at once into
//! a cache folder and put on the Windows clipboard (`CF_HDROP`) when they have all arrived.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId,
};
use ironrdp_core::IntoOwned as _;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

use crate::filecopy::{self, Download, Offer};

/// How often the clipboard is checked for changes.
const POLL: Duration = Duration::from_millis(250);

/// Files from the RDP client fetched at once at most (bigger copies are left alone).
const MAX_EAGER_BYTES: u64 = 4 << 30;

#[derive(Debug)]
enum Msg {
    /// The RDP client copied; whether text is among its formats, and the id of its file list
    /// format if it copied files.
    Copied {
        text: bool,
        files: Option<ClipboardFormatId>,
    },
    /// The RDP client's file list (answer to our paste of its file list format).
    FileList {
        files: Vec<FileDescriptor>,
        clip_data_id: Option<u32>,
    },
    /// Part of one of the RDP client's files (`None`: it failed).
    FileContents {
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// The RDP client reads one of our files.
    FileRequest(FileContentsRequest),
    /// The RDP client wants our clipboard in this format.
    DataRequest(ClipboardFormatId),
    /// The RDP client's answer to our paste request.
    Data(Option<String>),
    /// A native client copied this text.
    NativeText(String),
    /// A native client copied files, already fetched to these paths.
    NativeFiles(Vec<std::path::PathBuf>),
}

type Events = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ServerEvent>>>>;

#[derive(Clone)]
pub struct WinClipboard {
    events: Events,
    to_thread: mpsc::Sender<Msg>,
    from_session: broadcast::Sender<String>,
    /// Files copied in Windows (their paths), for native clients.
    files_from_session: broadcast::Sender<Vec<std::path::PathBuf>>,
}

impl WinClipboard {
    pub fn start() -> anyhow::Result<Self> {
        let events: Events = Arc::default();
        let (to_thread, rx) = mpsc::channel();
        let (from_session, _) = broadcast::channel(4);
        let (files_from_session, _) = broadcast::channel(4);
        let (ev, to_native) = (Arc::clone(&events), from_session.clone());
        let files_to_native = files_from_session.clone();
        let clipboard = arboard::Clipboard::new()?;
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || run(clipboard, &rx, &ev, &to_native, &files_to_native))?;
        info!("Clipboard enabled");
        Ok(Self {
            events,
            to_thread,
            from_session,
            files_from_session,
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
    files_to_native: &broadcast::Sender<Vec<std::path::PathBuf>>,
) {
    let mut seen = sequence();
    let set = |clipboard: &mut arboard::Clipboard, text: String| {
        if let Err(e) = clipboard.set_text(text) {
            warn!(%e, "Could not set the Windows clipboard");
        }
        // Our own change: not announced back.
        sequence()
    };
    // Files copied here, as offered to the RDP client.
    let mut offer = Offer::default();
    // The RDP client's files on their way here.
    let mut download: Option<Download> = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Msg::Copied { text, files }) => {
                download = None;
                if let Some(format) = files {
                    send(events, ClipboardMessage::SendInitiatePaste(format));
                } else if text {
                    send(
                        events,
                        ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT),
                    );
                }
            }
            Ok(Msg::FileList {
                files,
                clip_data_id,
            }) => {
                let total: u64 = files.iter().filter_map(|f| f.file_size).sum();
                if total > MAX_EAGER_BYTES {
                    warn!(
                        total,
                        "The client copied too much to fetch; not pasting it here"
                    );
                    continue;
                }
                let started = filecopy::new_download_dir()
                    .and_then(|dir| Download::new(&dir, &files, clip_data_id));
                match started {
                    Ok(d) => {
                        info!(
                            files = files.len(),
                            bytes = total,
                            "Fetching the client's files"
                        );
                        download = Some(d);
                        seen = step_download(&mut download, &mut clipboard, events).unwrap_or(seen);
                    }
                    Err(e) => warn!(error = format!("{e:#}"), "Cannot take the client's files"),
                }
            }
            Ok(Msg::FileContents { stream_id, data }) => {
                let Some(d) = download.as_mut() else { continue };
                if let Err(e) = d.on_response(stream_id, data.as_deref()) {
                    warn!(
                        error = format!("{e:#}"),
                        "Fetching the client's files failed"
                    );
                    download = None;
                    continue;
                }
                seen = step_download(&mut download, &mut clipboard, events).unwrap_or(seen);
            }
            Ok(Msg::FileRequest(req)) => {
                send(
                    events,
                    ClipboardMessage::SendFileContentsResponse(offer.answer(&req)),
                );
            }
            Ok(Msg::Data(Some(text))) => seen = set(&mut clipboard, text),
            Ok(Msg::Data(None)) => debug!("The client sent no clipboard text"),
            Ok(Msg::NativeText(text)) => seen = set(&mut clipboard, text),
            Ok(Msg::NativeFiles(paths)) => {
                download = None;
                if let Err(e) = clipboard.set().file_list(&paths) {
                    warn!(%e, "Could not put the files on the Windows clipboard");
                }
                seen = sequence();
            }
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
        if let Ok(paths) = clipboard.get().file_list()
            && !paths.is_empty()
        {
            if files_to_native.receiver_count() > 0 {
                let _ = files_to_native.send(paths.clone());
            }
            match Offer::new(&paths) {
                Ok(o) => {
                    debug!(files = o.paths.len(), "Windows clipboard has files");
                    send(
                        events,
                        ClipboardMessage::SendInitiateFileCopy(o.descriptors.clone()),
                    );
                    offer = o;
                }
                Err(e) => warn!(error = format!("{e:#}"), "Cannot offer the copied files"),
            }
            continue;
        }
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

/// Asks for the next part of the client's files, or, when they have all arrived, puts them on
/// the Windows clipboard (and returns its new sequence number, not to announce them back).
fn step_download(
    download: &mut Option<Download>,
    clipboard: &mut arboard::Clipboard,
    events: &Events,
) -> Option<u32> {
    let d = download.as_mut()?;
    match d.next_request() {
        Ok(Some(req)) => {
            send(events, ClipboardMessage::SendFileContentsRequest(req));
            None
        }
        Ok(None) => {
            let d = download.take()?;
            info!(
                bytes = d.bytes,
                items = d.top.len(),
                "The client's files are here"
            );
            if let Err(e) = clipboard.set().file_list(&d.top) {
                warn!(%e, "Could not put the files on the Windows clipboard");
            }
            Some(sequence())
        }
        Err(e) => {
            warn!(
                error = format!("{e:#}"),
                "Fetching the client's files failed"
            );
            *download = None;
            None
        }
    }
}

/// Capabilities for files on the clipboard (and long format names, which the file list
/// format needs).
pub(crate) fn file_capabilities() -> ClipboardGeneralCapabilityFlags {
    ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
        | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
        | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
        | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
        | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
}

/// The id of the file list format among the client's, if it copied files.
pub(crate) fn file_list_format(formats: &[ClipboardFormat]) -> Option<ClipboardFormatId> {
    formats
        .iter()
        .find(|f| {
            f.name()
                .is_some_and(|n| n.value() == ClipboardFormatName::FILE_LIST.value())
        })
        .map(|f| f.id())
}

impl crate::native::TextClipboard for WinClipboard {
    fn set(&self, text: String) {
        let _ = self.to_thread.send(Msg::NativeText(text));
    }

    fn subscribe(&self) -> broadcast::Receiver<String> {
        self.from_session.subscribe()
    }

    fn set_files(&self, paths: Vec<std::path::PathBuf>) {
        let _ = self.to_thread.send(Msg::NativeFiles(paths));
    }

    fn subscribe_files(&self) -> Option<broadcast::Receiver<Vec<std::path::PathBuf>>> {
        Some(self.files_from_session.subscribe())
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
        file_capabilities()
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
        let files = file_list_format(available_formats);
        let _ = self.to_thread.send(Msg::Copied { text, files });
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        let _ = self.to_thread.send(Msg::FileList {
            files: files.to_vec(),
            clip_data_id,
        });
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

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let _ = self.to_thread.send(Msg::FileRequest(request));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let _ = self.to_thread.send(Msg::FileContents {
            stream_id: response.stream_id(),
            data: (!response.is_error()).then(|| response.data().to_vec()),
        });
    }

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

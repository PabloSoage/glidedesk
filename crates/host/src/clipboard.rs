//! Clipboard between the clients (RDP and native) and the GNOME session: text, and files with
//! RDP clients (as `mstsc` does, see [`crate::filecopy`]).
//!
//! IronRDP's CLIPRDR callbacks are synchronous; they post to a task that talks to Mutter's
//! remote desktop clipboard API (`EnableClipboard`, `SetSelection`, `SelectionRead`,
//! `SelectionWrite`, as gnome-remote-desktop does). Data is delay-rendered both ways: only
//! the list of formats moves on copy, the text moves when someone pastes.
//!
//! - GNOME → client: `SelectionOwnerChanged` → format list to the client; the client's
//!   data request → `SelectionRead` → `CF_UNICODETEXT`.
//! - Client → GNOME: the client's format list → `SetSelection`; a GNOME app pasting →
//!   `SelectionTransfer` → data request to the client → `SelectionWrite`.
//!
//! Native clients get GNOME's text as soon as it is copied (read eagerly, it is small) and
//! send theirs the same way; the bridge keeps it to answer pastes in GNOME.
//!
//! Files: copied in GNOME (`x-special/gnome-copied-files`, `text/uri-list`), they are offered
//! to the RDP client, which reads them when it pastes. Copied on the RDP client, GNOME is told
//! there are files; when an app pastes them, they are fetched into a cache folder and the app
//! gets their paths once all have arrived.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId,
};
use ironrdp_core::IntoOwned as _;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::{OwnedVariant as OwnedValue, Variant as Value};

use crate::filecopy::{self, Download, Offer};
use crate::mutter::{LiveRx, RemoteDesktopSessionProxy};

/// MIME types offered to GNOME for client text, preferred first.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
/// MIME types for files, as Nautilus offers them.
const FILE_MIMES: &[&str] = &["x-special/gnome-copied-files", "text/uri-list"];

#[derive(Debug)]
enum FromClient {
    /// The client copied; whether text is among its formats, and the id of its file list
    /// format if it copied files.
    Copied {
        text: bool,
        files: Option<ClipboardFormatId>,
    },
    /// The client's file list (answer to our paste of its file list format).
    FileList {
        files: Vec<FileDescriptor>,
        clip_data_id: Option<u32>,
    },
    /// Part of one of the client's files (`None`: it failed).
    FileContents {
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// The client reads one of our files.
    FileRequest(FileContentsRequest),
    /// The client wants our clipboard in this format.
    DataRequest(ClipboardFormatId),
    /// The client's answer to our paste request.
    Data(Option<String>),
    /// A native client copied this text.
    NativeText(String),
}

/// The native front end's side of the clipboard.
#[derive(Clone)]
pub struct ClipboardHub {
    to_bridge: mpsc::UnboundedSender<FromClient>,
    /// Text copied in GNOME.
    pub from_session: tokio::sync::broadcast::Sender<String>,
}

impl ClipboardHub {
    /// A native client copied `text`.
    pub fn set(&self, text: String) {
        let _ = self.to_bridge.send(FromClient::NativeText(text));
    }
}

type Events = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ServerEvent>>>>;

pub struct ClipboardFactory {
    events: Events,
    to_bridge: mpsc::UnboundedSender<FromClient>,
    from_session: tokio::sync::broadcast::Sender<String>,
}

impl ClipboardFactory {
    /// Starts the bridge task on the current session (and each one replacing it) and
    /// returns the factory for IronRDP.
    pub fn spawn(mut live: LiveRx) -> Self {
        let events: Events = Arc::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (from_session, _) = tokio::sync::broadcast::channel(4);
        let (ev, to_native) = (Arc::clone(&events), from_session.clone());
        tokio::spawn(async move {
            loop {
                let remote = live.borrow_and_update().remote.clone();
                tokio::select! {
                    // The session's signals ended: wait for the next session.
                    () = bridge(remote, &mut rx, &ev, &to_native) => if live.changed().await.is_err() { return },
                    changed = live.changed() => if changed.is_err() { return },
                }
            }
        });
        Self {
            events,
            to_bridge: tx,
            from_session,
        }
    }

    /// The same clipboard for the native front end.
    pub fn hub(&self) -> ClipboardHub {
        ClipboardHub {
            to_bridge: self.to_bridge.clone(),
            from_session: self.from_session.clone(),
        }
    }
}

impl ServerEventSender for ClipboardFactory {
    fn set_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<ServerEvent>) {
        *self.events.lock().unwrap() = Some(sender);
    }
}

impl CliprdrBackendFactory for ClipboardFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend {
            to_bridge: self.to_bridge.clone(),
        })
    }
}

impl CliprdrServerFactory for ClipboardFactory {}

#[derive(Debug)]
struct Backend {
    to_bridge: mpsc::UnboundedSender<FromClient>,
}

ironrdp_core::impl_as_any!(Backend);

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
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
        let files = available_formats
            .iter()
            .find(|f| {
                f.name()
                    .is_some_and(|n| n.value() == ClipboardFormatName::FILE_LIST.value())
            })
            .map(|f| f.id());
        let _ = self.to_bridge.send(FromClient::Copied { text, files });
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        let _ = self.to_bridge.send(FromClient::FileList {
            files: files.to_vec(),
            clip_data_id,
        });
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let _ = self.to_bridge.send(FromClient::DataRequest(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let text = if response.is_error() {
            None
        } else {
            response.to_unicode_string().ok()
        };
        let _ = self.to_bridge.send(FromClient::Data(text));
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let _ = self.to_bridge.send(FromClient::FileRequest(request));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let _ = self.to_bridge.send(FromClient::FileContents {
            stream_id: response.stream_id(),
            data: (!response.is_error()).then(|| response.data().to_vec()),
        });
    }

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

fn send(events: &Events, message: ClipboardMessage) {
    if let Some(tx) = events.lock().unwrap().as_ref() {
        let _ = tx.send(ServerEvent::Clipboard(message));
    }
}

/// The "mime-types" option: Mutter sends it as `(as)`, the documentation says `as`.
fn mime_list(options: &HashMap<String, OwnedValue>) -> Vec<String> {
    let Some(v) = options.get("mime-types") else {
        return Vec::new();
    };
    if let Ok(list) = Vec::<String>::try_from(v.clone()) {
        return list;
    }
    <(Vec<String>,)>::try_from(v.clone())
        .map(|(list,)| list)
        .unwrap_or_default()
}

/// Bridges the clipboard of one remote desktop session; returns when its signals end. A
/// session that fails to enable it waits for the next one.
async fn bridge(
    remote: RemoteDesktopSessionProxy<'static>,
    from_client: &mut mpsc::UnboundedReceiver<FromClient>,
    events: &Events,
    to_native: &tokio::sync::broadcast::Sender<String>,
) {
    if let Err(e) = remote.enable_clipboard(HashMap::new()).await {
        warn!(%e, "EnableClipboard failed; no clipboard");
        return std::future::pending().await;
    }
    let (Ok(mut owner_changed), Ok(mut transfers)) = (
        remote.receive_selection_owner_changed().await,
        remote.receive_selection_transfer().await,
    ) else {
        warn!("Could not subscribe to the clipboard signals");
        return std::future::pending().await;
    };
    info!("Clipboard enabled");
    // GNOME pastes waiting for the client's text: Mutter serials.
    let mut waiting: Vec<u32> = Vec::new();
    // The MIME type GNOME's current clipboard offers text in.
    let mut gnome_text_mime: Option<String> = None;
    // Text a native client copied, to answer GNOME pastes without a round trip.
    let mut native_text: Option<String> = None;
    // Files copied in GNOME, as offered to the client.
    let mut offer = Arc::new(Offer::default());
    // Files copied on the client: its file list format, the fetch under way, the paths once
    // they have arrived, and the GNOME pastes waiting for them.
    let mut client_files: Option<ClipboardFormatId> = None;
    let mut download: Option<Download> = None;
    let mut fetched: Option<Vec<std::path::PathBuf>> = None;
    let mut waiting_files: Vec<(u32, String)> = Vec::new();

    loop {
        tokio::select! {
            Some(sig) = owner_changed.next() => {
                let Ok(args) = sig.args() else { continue };
                let mimes = mime_list(&args.options);
                let session_owns = args
                    .options
                    .get("session-is-owner")
                    .and_then(|v| bool::try_from(v.clone()).ok())
                    .unwrap_or(false);
                debug!(?mimes, session_owns, raw = ?args.options, "GNOME clipboard owner changed");
                if session_owns {
                    continue;
                }
                if let Some(mime) = FILE_MIMES.iter().find(|m| mimes.iter().any(|x| x == *m))
                    && let Some(list) = read_selection(&remote, mime).await
                {
                    let paths = filecopy::parse_uri_list(&list);
                    if !paths.is_empty() {
                        match tokio::task::spawn_blocking(move || Offer::new(&paths)).await {
                            Ok(Ok(o)) => {
                                debug!(files = o.paths.len(), "Offering GNOME files to the client");
                                send(events, ClipboardMessage::SendInitiateFileCopy(o.descriptors.clone()));
                                offer = Arc::new(o);
                                gnome_text_mime = None;
                                native_text = None;
                                continue;
                            }
                            Ok(Err(e)) => warn!(error = format!("{e:#}"), "Cannot offer the copied files"),
                            Err(e) => warn!(%e, "Cannot offer the copied files"),
                        }
                    }
                }
                gnome_text_mime = TEXT_MIMES.iter().find(|m| mimes.iter().any(|x| x == *m)).map(|m| m.to_string());
                native_text = None;
                if let Some(mime) = gnome_text_mime.clone()
                    && to_native.receiver_count() > 0
                    && let Some(t) = read_selection(&remote, &mime).await
                {
                    let _ = to_native.send(t);
                }
                if gnome_text_mime.is_some() {
                    debug!(mime = ?gnome_text_mime, "Offering GNOME text to the client");
                    send(events, ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]));
                }
            }
            Some(sig) = transfers.next() => {
                let Ok(args) = sig.args() else { continue };
                if let (true, Some(format)) = (FILE_MIMES.contains(&args.mime_type.as_str()), client_files) {
                    if let Some(paths) = &fetched {
                        let ok = write_selection(&remote, args.serial, &file_list_text(&args.mime_type, paths)).await;
                        let _ = remote.selection_write_done(args.serial, ok).await;
                    } else {
                        if waiting_files.is_empty() && download.is_none() {
                            send(events, ClipboardMessage::SendInitiatePaste(format));
                        }
                        waiting_files.push((args.serial, args.mime_type.to_string()));
                    }
                } else if let (true, Some(t)) = (TEXT_MIMES.contains(&args.mime_type.as_str()), &native_text) {
                    let ok = write_selection(&remote, args.serial, t).await;
                    let _ = remote.selection_write_done(args.serial, ok).await;
                } else if TEXT_MIMES.contains(&args.mime_type.as_str()) {
                    waiting.push(args.serial);
                    send(events, ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT));
                } else {
                    let _ = remote.selection_write_done(args.serial, false).await;
                }
            }
            Some(msg) = from_client.recv() => match msg {
                FromClient::Copied { text, files } => {
                    native_text = None;
                    client_files = files;
                    (download, fetched) = (None, None);
                    for (serial, _) in waiting_files.drain(..) {
                        let _ = remote.selection_write_done(serial, false).await;
                    }
                    // Mutter rejects an empty list.
                    if !text && files.is_none() {
                        continue;
                    }
                    let mut mimes: Vec<&str> = Vec::new();
                    if files.is_some() {
                        mimes.extend(FILE_MIMES);
                    }
                    if text {
                        mimes.extend(TEXT_MIMES);
                    }
                    let opts = HashMap::from([("mime-types", Value::from(mimes))]);
                    if let Err(e) = remote.set_selection(opts).await {
                        warn!(%e, "SetSelection failed");
                    }
                }
                FromClient::DataRequest(format) => {
                    let text = match (&gnome_text_mime, format == ClipboardFormatId::CF_UNICODETEXT) {
                        (Some(mime), true) => read_selection(&remote, mime).await,
                        _ => None,
                    };
                    let response = match text {
                        Some(t) => FormatDataResponse::new_unicode_string(&t),
                        None => FormatDataResponse::new_error(),
                    };
                    send(events, ClipboardMessage::SendFormatData(response.into_owned()));
                }
                FromClient::NativeText(text) => {
                    native_text = Some(text);
                    let mimes: Vec<&str> = TEXT_MIMES.to_vec();
                    let opts = HashMap::from([("mime-types", Value::from(mimes))]);
                    if let Err(e) = remote.set_selection(opts).await {
                        warn!(%e, "SetSelection failed");
                    }
                }
                FromClient::FileList { files, clip_data_id } => {
                    if waiting_files.is_empty() {
                        continue;
                    }
                    let total: u64 = files.iter().filter_map(|f| f.file_size).sum();
                    match filecopy::new_download_dir().and_then(|dir| Download::new(&dir, &files, clip_data_id)) {
                        Ok(d) => {
                            info!(files = files.len(), bytes = total, "Fetching the client's files");
                            download = Some(d);
                        }
                        Err(e) => warn!(error = format!("{e:#}"), "Cannot take the client's files"),
                    }
                    step_download(&remote, events, &mut download, &mut fetched, &mut waiting_files).await;
                }
                FromClient::FileContents { stream_id, data } => {
                    let Some(d) = download.as_mut() else { continue };
                    if let Err(e) = d.on_response(stream_id, data.as_deref()) {
                        warn!(error = format!("{e:#}"), "Fetching the client's files failed");
                        download = None;
                    }
                    step_download(&remote, events, &mut download, &mut fetched, &mut waiting_files).await;
                }
                FromClient::FileRequest(req) => {
                    let offer = Arc::clone(&offer);
                    if let Ok(response) = tokio::task::spawn_blocking(move || offer.answer(&req)).await {
                        send(events, ClipboardMessage::SendFileContentsResponse(response));
                    }
                }
                FromClient::Data(text) => {
                    for serial in waiting.drain(..) {
                        let ok = match &text {
                            Some(t) => write_selection(&remote, serial, t).await,
                            None => false,
                        };
                        let _ = remote.selection_write_done(serial, ok).await;
                    }
                }
            },
            else => break,
        }
    }
}

/// Asks the client for the next part of its files; when they have all arrived (or the fetch
/// failed), answers the GNOME pastes waiting for them.
async fn step_download(
    remote: &RemoteDesktopSessionProxy<'static>,
    events: &Events,
    download: &mut Option<Download>,
    fetched: &mut Option<Vec<std::path::PathBuf>>,
    waiting: &mut Vec<(u32, String)>,
) {
    let next = match download.as_mut().map(Download::next_request) {
        Some(Ok(Some(req))) => {
            send(events, ClipboardMessage::SendFileContentsRequest(req));
            return;
        }
        Some(Ok(None)) => download.take().map(|d| {
            info!(
                bytes = d.bytes,
                items = d.top.len(),
                "The client's files are here"
            );
            d.top
        }),
        Some(Err(e)) => {
            warn!(
                error = format!("{e:#}"),
                "Fetching the client's files failed"
            );
            *download = None;
            None
        }
        None => None,
    };
    *fetched = next;
    for (serial, mime) in waiting.drain(..) {
        let ok = match fetched.as_ref() {
            Some(paths) => write_selection(remote, serial, &file_list_text(&mime, paths)).await,
            None => false,
        };
        let _ = remote.selection_write_done(serial, ok).await;
    }
}

/// Paths as GNOME wants them for `mime`.
fn file_list_text(mime: &str, paths: &[std::path::PathBuf]) -> String {
    let uris: Vec<String> = paths.iter().map(|p| filecopy::file_uri(p)).collect();
    if mime == "x-special/gnome-copied-files" {
        format!("copy\n{}", uris.join("\n"))
    } else {
        uris.iter().map(|u| format!("{u}\r\n")).collect()
    }
}

/// Mutter hands out non-blocking pipes; the transfers here run on a blocking thread.
fn set_blocking(fd: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

async fn read_selection(remote: &RemoteDesktopSessionProxy<'static>, mime: &str) -> Option<String> {
    let fd = match remote.selection_read(mime).await {
        Ok(fd) => fd,
        Err(e) => {
            warn!(%e, mime, "SelectionRead failed");
            return None;
        }
    };
    let fd: std::os::fd::OwnedFd = fd.into();
    set_blocking(&fd);
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::from(fd);
        let mut buf = Vec::new();
        if let Err(e) = f.read_to_end(&mut buf) {
            warn!(%e, "Reading the GNOME clipboard failed");
            return None;
        }
        debug!(bytes = buf.len(), "Read the GNOME clipboard");
        Some(String::from_utf8_lossy(&buf).into_owned())
    })
    .await
    .ok()
    .flatten()
}

async fn write_selection(
    remote: &RemoteDesktopSessionProxy<'static>,
    serial: u32,
    text: &str,
) -> bool {
    let Ok(fd) = remote.selection_write(serial).await else {
        return false;
    };
    let fd: std::os::fd::OwnedFd = fd.into();
    set_blocking(&fd);
    let bytes = text.trim_end_matches('\0').as_bytes().to_vec();
    tokio::task::spawn_blocking(move || std::fs::File::from(fd).write_all(&bytes).is_ok())
        .await
        .unwrap_or(false)
}

impl crate::native::TextClipboard for ClipboardHub {
    fn set(&self, text: String) {
        ClipboardHub::set(self, text);
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.from_session.subscribe()
    }
}

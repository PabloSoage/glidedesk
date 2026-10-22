//! Clipboard between the clients (RDP and native) and the GNOME session (text).
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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::IntoOwned as _;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::{OwnedVariant as OwnedValue, Variant as Value};

use crate::mutter::{LiveRx, RemoteDesktopSessionProxy};

/// MIME types offered to GNOME for client text, preferred first.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

#[derive(Debug)]
enum FromClient {
    /// The client copied; whether text is among its formats.
    Copied { text: bool },
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
        let _ = self.to_bridge.send(FromClient::Copied { text });
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

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

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
                if let (true, Some(t)) = (TEXT_MIMES.contains(&args.mime_type.as_str()), &native_text) {
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
                FromClient::Copied { text } => {
                    native_text = None;
                    // Only text for now; Mutter rejects an empty list.
                    if !text {
                        continue;
                    }
                    let mimes: Vec<&str> = TEXT_MIMES.to_vec();
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

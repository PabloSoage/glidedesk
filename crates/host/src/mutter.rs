//! Mutter's private D-Bus remote desktop and screen cast APIs.
//!
//! These are the interfaces gnome-remote-desktop uses. `RecordVirtual` creates a virtual
//! monitor whose size follows the format negotiated on its PipeWire stream, which is how the
//! desktop follows the client window. No portal dialog is involved: the caller must be on
//! the compositor's own session bus.
//!
//! Several monitors are several `RecordVirtual` streams of one session ([`add_virtual`]),
//! placed with `org.gnome.Mutter.DisplayConfig` ([`DisplayConfigProxy`]).

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::proxy;
use zbus::{OwnedObjectPath, Variant as Value};

#[proxy(
    interface = "org.gnome.Mutter.RemoteDesktop",
    default_service = "org.gnome.Mutter.RemoteDesktop",
    default_path = "/org/gnome/Mutter/RemoteDesktop"
)]
pub trait RemoteDesktop {
    fn create_session(&self) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.RemoteDesktop.Session",
    default_service = "org.gnome.Mutter.RemoteDesktop"
)]
pub trait RemoteDesktopSession {
    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn session_id(&self) -> zbus::Result<String>;

    // Input goes without waiting for replies: Mutter answers each call from its main loop,
    // often a frame later, and waiting would queue every event behind the previous one.
    // D-Bus keeps the order of messages on one connection.
    #[zbus(no_reply)]
    fn notify_keyboard_keycode(&self, keycode: u32, state: bool) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn notify_keyboard_keysym(&self, keysym: u32, state: bool) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn notify_pointer_motion_absolute(&self, stream: &str, x: f64, y: f64) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn notify_pointer_button(&self, button: i32, state: bool) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn notify_pointer_axis(&self, dx: f64, dy: f64, flags: u32) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn notify_touch_down(&self, stream: &str, slot: u32, x: f64, y: f64) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn notify_touch_motion(&self, stream: &str, slot: u32, x: f64, y: f64) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn notify_touch_up(&self, slot: u32) -> zbus::Result<()>;

    fn enable_clipboard(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn disable_clipboard(&self) -> zbus::Result<()>;
    fn set_selection(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn selection_write(&self, serial: u32) -> zbus::Result<zbus::OwnedFd>;
    fn selection_write_done(&self, serial: u32, success: bool) -> zbus::Result<()>;
    fn selection_read(&self, mime_type: &str) -> zbus::Result<zbus::OwnedFd>;

    #[zbus(signal)]
    fn selection_owner_changed(
        &self,
        options: HashMap<String, zbus::OwnedVariant>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    fn selection_transfer(&self, mime_type: String, serial: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn closed(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
pub trait ScreenCast {
    fn create_session(&self, properties: HashMap<&str, Value<'_>>)
    -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Session",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
pub trait ScreenCastSession {
    fn record_virtual(&self, properties: HashMap<&str, Value<'_>>)
    -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Stream",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
pub trait ScreenCastStream {
    /// Starts a stream added to a session already started.
    fn start(&self) -> zbus::Result<()>;
    /// Ends the stream (a virtual one takes its monitor with it).
    fn stop(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn pipe_wire_stream_added(&self, node_id: u32) -> zbus::Result<()>;
}

/// A monitor in `GetCurrentState`: (connector, vendor, product, serial), modes (id, width,
/// height, refresh rate, preferred scale, supported scales, properties), properties.
pub type MonitorState = (
    (String, String, String, String),
    Vec<(String, i32, i32, f64, f64, Vec<f64>, HashMap<String, zbus::OwnedVariant>)>,
    HashMap<String, zbus::OwnedVariant>,
);
/// A logical monitor in `GetCurrentState`: x, y, scale, transform, primary, its monitors
/// (connector, vendor, product, serial), properties.
pub type LogicalMonitorState = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, zbus::OwnedVariant>,
);
/// A logical monitor for `ApplyMonitorsConfig`: x, y, scale, transform, primary, its
/// monitors (connector, mode id, properties).
pub type LogicalMonitorConfig<'a> = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, HashMap<&'a str, Value<'a>>)>,
);

#[proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
pub trait DisplayConfig {
    #[allow(clippy::type_complexity)]
    fn get_current_state(
        &self,
    ) -> zbus::Result<(
        u32,
        Vec<MonitorState>,
        Vec<LogicalMonitorState>,
        HashMap<String, zbus::OwnedVariant>,
    )>;

    /// `method`: 0 verify, 1 temporary, 2 persistent.
    fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<LogicalMonitorConfig<'_>>,
        properties: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

/// Cursor modes of `RecordVirtual`.
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum CursorMode {
    /// The cursor is not drawn; the client draws its own pointer.
    Hidden = 0,
    /// The cursor is drawn into the frames.
    Embedded = 1,
    /// The cursor is sent as PipeWire metadata.
    Metadata = 2,
}

/// The remote desktop session in use; replaced when Mutter closes one (see `gnome`).
pub struct Live {
    pub remote: RemoteDesktopSessionProxy<'static>,
    /// Stream object path, needed for absolute pointer motion.
    pub stream_path: String,
    /// Its screen cast session, where the streams of further monitors are added.
    pub screen_cast: ScreenCastSessionProxy<'static>,
}

pub type LiveRx = tokio::sync::watch::Receiver<std::sync::Arc<Live>>;

/// A started remote desktop session with one virtual monitor.
pub struct Session {
    pub remote: RemoteDesktopSessionProxy<'static>,
    /// PipeWire node of the virtual monitor's stream.
    pub node_id: u32,
    /// Stream object path, needed for absolute pointer motion.
    pub stream_path: String,
    pub screen_cast: ScreenCastSessionProxy<'static>,
}

impl Session {
    pub async fn start(conn: &zbus::Connection, cursor: CursorMode) -> Result<Self> {
        let rd = RemoteDesktopProxy::new(conn).await.context(
            "org.gnome.Mutter.RemoteDesktop is not on this bus (is gnome-shell running?)",
        )?;
        let rd_path = rd
            .create_session()
            .await
            .context("creating the remote desktop session")?;
        let remote = RemoteDesktopSessionProxy::builder(conn)
            .path(rd_path)
            .build()
            .await?;
        let session_id = remote.session_id().await?;

        let sc = ScreenCastProxy::new(conn).await?;
        let sc_path = sc
            .create_session(HashMap::from([(
                "remote-desktop-session-id",
                Value::from(session_id.as_str()),
            )]))
            .await
            .context("creating the screen cast session")?;
        let sc_session = ScreenCastSessionProxy::builder(conn)
            .path(sc_path)
            .build()
            .await?;
        let stream_path = sc_session
            .record_virtual(HashMap::from([
                ("cursor-mode", Value::from(cursor as u32)),
                ("is-platform", Value::from(true)),
            ]))
            .await
            .context("RecordVirtual")?;
        let stream = ScreenCastStreamProxy::builder(conn)
            .path(stream_path.clone())
            .build()
            .await?;
        let mut added = stream.receive_pipe_wire_stream_added().await?;

        remote
            .start()
            .await
            .context("starting the remote desktop session")?;
        let signal = tokio::time::timeout(std::time::Duration::from_secs(10), added.next())
            .await
            .context("Mutter did not announce the PipeWire stream")?
            .context("signal stream ended")?;
        let node_id = signal.args()?.node_id;
        Ok(Self {
            remote,
            node_id,
            stream_path: stream_path.to_string(),
            screen_cast: sc_session,
        })
    }
}

/// A virtual monitor added to a started session.
pub struct VirtualStream {
    pub stream: ScreenCastStreamProxy<'static>,
    /// Stream object path, for absolute pointer motion on it.
    pub path: String,
    /// PipeWire node of its stream.
    pub node_id: u32,
}

/// Adds a virtual monitor to the started session of `screen_cast`; Mutter creates it once
/// the PipeWire stream is connected and its size negotiated.
pub async fn add_virtual(
    conn: &zbus::Connection,
    screen_cast: &ScreenCastSessionProxy<'static>,
    cursor: CursorMode,
) -> Result<VirtualStream> {
    let path = screen_cast
        .record_virtual(HashMap::from([
            ("cursor-mode", Value::from(cursor as u32)),
            ("is-platform", Value::from(true)),
        ]))
        .await
        .context("RecordVirtual")?;
    let stream = ScreenCastStreamProxy::builder(conn)
        .path(path.clone())
        .build()
        .await?;
    let mut added = stream.receive_pipe_wire_stream_added().await?;
    stream.start().await.context("starting the stream")?;
    let signal = tokio::time::timeout(std::time::Duration::from_secs(10), added.next())
        .await
        .context("Mutter did not announce the PipeWire stream")?
        .context("signal stream ended")?;
    Ok(VirtualStream {
        stream,
        path: path.to_string(),
        node_id: signal.args()?.node_id,
    })
}

#[proxy(
    interface = "org.gnome.Shell",
    default_service = "org.gnome.Shell",
    default_path = "/org/gnome/Shell"
)]
pub trait Shell {
    #[zbus(property)]
    fn overview_active(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_overview_active(&self, active: bool) -> zbus::Result<()>;
}

/// GNOME Shell opens the activities overview when the first monitor appears (its startup
/// animation), which for a remote session is our virtual monitor. Close it once it shows up,
/// so a session starts on the desktop like a normal login. A request during the startup
/// animation can be undone by it (slow, software-rendered shells), so keep closing it until it
/// has stayed closed for a while.
pub async fn leave_startup_overview(conn: &zbus::Connection) {
    let Ok(shell) = ShellProxy::new(conn).await else {
        return;
    };
    let (mut closed_once, mut quiet) = (false, 0);
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if shell.overview_active().await.unwrap_or(false) {
            let _ = shell.set_overview_active(false).await;
            closed_once = true;
            quiet = 0;
        } else if closed_once {
            quiet += 1;
            if quiet >= 12 {
                return;
            }
        }
    }
}

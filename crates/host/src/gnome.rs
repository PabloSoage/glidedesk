//! The GNOME session kept alive.
//!
//! GNOME Shell shows a "Stop Screen Sharing" button in the top bar for every remote
//! desktop session (GNOME 46 has no way to hide it), and Mutter may also close a session
//! on its own. When the session closes, a new one is started on a new virtual monitor of
//! the same size, and the capture, input and clipboard move to it: clients stay connected
//! and only see the desktop come back.
//!
//! Several monitors ([`GnomeDisplays`]): a `RecordVirtual` stream per monitor after the
//! first, in the same session, placed with `ApplyMonitorsConfig`. Whether Mutter keeps the
//! positions asked for with virtual monitors is not known for every version, so they are
//! read back from `GetCurrentState`, and clients are told the positions Mutter has.
//! Absolute pointer and touch positions go to the stream of the monitor they fall on
//! ([`locate`]). The pointer image needs nothing more: a stream whose monitor the pointer
//! is not on sends no pointer metadata.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cudarc::driver::CudaContext;
use futures_util::StreamExt;
use glidedesk_proto::Monitor;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::capture::{Capture, CursorEvent, Sinks};
use crate::frame::Source;
use crate::layout::{Displays, MonitorStream};
use crate::mutter::{
    self, CursorMode, DisplayConfigProxy, Live, LogicalMonitorConfig, LogicalMonitorState,
    MonitorState, ScreenCastSessionProxy, ScreenCastStreamProxy, Session,
};

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

/// Watches the current session and replaces it when Mutter closes it. `lost` is bumped
/// then: the monitors after the first went with the old session.
pub async fn supervise(
    conn: zbus::Connection,
    cursor: CursorMode,
    live: watch::Sender<Arc<Live>>,
    followed: Arc<Followed>,
    sinks: Sinks,
    cuda: Option<Arc<CudaContext>>,
    lost: watch::Sender<u64>,
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
            screen_cast: session.screen_cast,
        }));
        lost.send_modify(|n| *n = n.wrapping_add(1));
    }
}

// ---------------------------------------------------------------------------------------
// Several monitors

/// Where a monitor is in the desktop, for absolute pointer and touch positions.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Its stream; `None` for monitor 0, whose stream is the session's
    /// ([`Live::stream_path`]).
    pub stream: Option<String>,
}

/// Every monitor, monitor 0 first; empty with one monitor (positions are then monitor 0's).
pub type Placements = Arc<Mutex<Vec<Placement>>>;

/// The stream a desktop position is on (`None`: monitor 0's), and the position on that
/// stream. Off every monitor (beside a shorter one), the nearest.
pub fn locate(placements: &[Placement], x: f64, y: f64) -> (Option<String>, f64, f64) {
    let inside = |p: &&Placement| {
        x >= p.x && x < p.x + p.width && y >= p.y && y < p.y + p.height
    };
    let distance = |p: &Placement| {
        let dx = (p.x - x).max(x - (p.x + p.width)).max(0.0);
        let dy = (p.y - y).max(y - (p.y + p.height)).max(0.0);
        dx * dx + dy * dy
    };
    let Some(p) = placements.iter().find(inside).or_else(|| {
        placements
            .iter()
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
    }) else {
        return (None, x, y);
    };
    (
        p.stream.clone(),
        (x - p.x).clamp(0.0, (p.width - 1.0).max(0.0)),
        (y - p.y).clamp(0.0, (p.height - 1.0).max(0.0)),
    )
}

/// Vendor and product Mutter gives the monitors of `RecordVirtual` (and their serials
/// count up as it creates them: "0x000001", "0x000002"...).
const VIRTUAL_VENDOR: &str = "MetaVendor";
const VIRTUAL_PRODUCT: &str = "Virtual remote monitor";

/// One of Mutter's virtual monitors in `GetCurrentState`.
#[derive(Clone, Debug, PartialEq)]
struct Virtual {
    connector: String,
    /// Current mode: id and size in pixels.
    mode: String,
    width: i32,
    height: i32,
    /// Its logical monitor.
    x: i32,
    y: i32,
    scale: f64,
}

/// Mutter's virtual monitors that are ours, for a layout of `count`: the oldest (monitor
/// 0's, created with the session) and the `count - 1` newest (the ones just added; any in
/// between belong to streams being closed). Fewer while Mutter has not created them all.
fn ours(monitors: &[MonitorState], logical: &[LogicalMonitorState], count: usize) -> Vec<Virtual> {
    let mut all: Vec<(u64, Virtual)> = monitors
        .iter()
        .filter(|((_, vendor, product, _), _, _)| {
            vendor == VIRTUAL_VENDOR && product == VIRTUAL_PRODUCT
        })
        .filter_map(|((connector, _, _, serial), modes, _)| {
            let (mode, width, height) = modes
                .iter()
                .find(|m| {
                    m.6.get("is-current")
                        .and_then(|v| bool::try_from(v).ok())
                        .unwrap_or(false)
                })
                .map(|m| (m.0.clone(), m.1, m.2))?;
            let (x, y, scale) = logical
                .iter()
                .find(|l| l.5.iter().any(|c| c.0 == *connector))
                .map(|l| (l.0, l.1, l.2))?;
            let seq = u64::from_str_radix(serial.trim_start_matches("0x"), 16).unwrap_or(u64::MAX);
            Some((
                seq,
                Virtual {
                    connector: connector.clone(),
                    mode,
                    width,
                    height,
                    x,
                    y,
                    scale,
                },
            ))
        })
        .collect();
    all.sort_by_key(|(seq, _)| *seq);
    let mut all: Vec<Virtual> = all.into_iter().map(|(_, v)| v).collect();
    if count >= 1 && all.len() > count {
        let newest = all.split_off(all.len() - (count - 1));
        all.truncate(1);
        all.extend(newest);
    }
    all
}

/// The configuration that puts our monitors where `layout` has them (same order as
/// [`ours`]); other logical monitors stay as they are.
fn monitors_config<'a>(
    ours: &[Virtual],
    layout: &[Monitor],
    monitors: &[MonitorState],
    logical: &[LogicalMonitorState],
) -> Vec<LogicalMonitorConfig<'a>> {
    let primary = layout.iter().position(|m| m.primary).unwrap_or(0);
    let mut config: Vec<LogicalMonitorConfig<'a>> = ours
        .iter()
        .zip(layout)
        .enumerate()
        .map(|(i, (v, m))| {
            (
                m.x,
                m.y,
                1.0,
                0,
                i == primary,
                vec![(v.connector.clone(), v.mode.clone(), HashMap::new())],
            )
        })
        .collect();
    let current_mode = |connector: &str| {
        monitors
            .iter()
            .find(|((c, _, _, _), _, _)| c == connector)
            .and_then(|(_, modes, _)| {
                modes.iter().find(|m| {
                    m.6.get("is-current")
                        .and_then(|v| bool::try_from(v).ok())
                        .unwrap_or(false)
                })
            })
            .map(|m| m.0.clone())
    };
    for l in logical {
        if l.5.iter().any(|c| ours.iter().any(|v| v.connector == c.0)) {
            continue;
        }
        let members = l
            .5
            .iter()
            .filter_map(|c| Some((c.0.clone(), current_mode(&c.0)?, HashMap::new())))
            .collect();
        config.push((l.0, l.1, l.2, l.3, false, members));
    }
    config
}

/// Our monitors as clients are told about them: where Mutter has them, with the bounding
/// box at 0,0.
fn placed(ours: &[Virtual], layout: &[Monitor]) -> Vec<Monitor> {
    let min_x = ours.iter().map(|v| v.x).min().unwrap_or(0);
    let min_y = ours.iter().map(|v| v.y).min().unwrap_or(0);
    ours.iter()
        .zip(layout)
        .map(|(v, m)| {
            let scale = if v.scale > 0.0 { v.scale } else { 1.0 };
            Monitor {
                x: v.x - min_x,
                y: v.y - min_y,
                width: (f64::from(v.width) / scale).round() as u16,
                height: (f64::from(v.height) / scale).round() as u16,
                ..*m
            }
        })
        .collect()
}

/// Puts the monitors of `layout` (all created, monitor 0 first) where it has them, and
/// returns where Mutter has them.
async fn place(conn: &zbus::Connection, layout: &[Monitor]) -> Result<Vec<Monitor>> {
    let dc = DisplayConfigProxy::new(conn)
        .await
        .context("org.gnome.Mutter.DisplayConfig")?;
    // Mutter creates a virtual monitor once its stream's format is negotiated.
    let mut tries = 0;
    let (serial, monitors, logical) = loop {
        let (serial, monitors, logical, _) = dc.get_current_state().await?;
        if ours(&monitors, &logical, layout.len()).len() >= layout.len() || tries >= 50 {
            break (serial, monitors, logical);
        }
        tries += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let mine = ours(&monitors, &logical, layout.len());
    if mine.len() < layout.len() {
        bail!(
            "Mutter shows {} of the {} virtual monitors",
            mine.len(),
            layout.len()
        );
    }
    let config = monitors_config(&mine, layout, &monitors, &logical);
    debug!(?config, "Monitor configuration");
    if let Err(e) = dc
        .apply_monitors_config(serial, 1, config, HashMap::new())
        .await
    {
        warn!(%e, "Mutter did not take the monitor positions; keeping its own");
    }
    let (_, monitors, logical, _) = dc.get_current_state().await?;
    let mine = ours(&monitors, &logical, layout.len());
    if mine.len() < layout.len() {
        bail!("a virtual monitor went away while placing them");
    }
    let got = placed(&mine, layout);
    for (want, have) in layout.iter().zip(&got) {
        if (want.x, want.y) != (have.x, have.y) {
            warn!(
                monitor = want.id,
                asked = ?(want.x, want.y),
                got = ?(have.x, have.y),
                "Monitor placed elsewhere than asked"
            );
        }
    }
    Ok(got)
}

/// A further monitor set up: its stream, to stop, and its capture.
struct Extra {
    stream: ScreenCastStreamProxy<'static>,
    capture: Arc<Mutex<Capture>>,
}

/// A further monitor's capture as its pipeline's [`Source`] (its size is the layout's).
struct MonitorCapture(Arc<Mutex<Capture>>);

impl Source for MonitorCapture {
    fn resize(&self, width: u32, height: u32) {
        self.0.lock().unwrap().resize(width, height);
    }
}

/// The monitors after the first of the GNOME session (see the module documentation).
pub struct GnomeDisplays {
    conn: zbus::Connection,
    live: watch::Receiver<Arc<Live>>,
    cursor_mode: CursorMode,
    /// The session's pointer: every stream reports it while it is on its monitor.
    cursor: Arc<watch::Sender<Option<CursorEvent>>>,
    cuda: Option<Arc<CudaContext>>,
    placements: Placements,
    extra: tokio::sync::Mutex<Vec<Extra>>,
    lost: watch::Sender<u64>,
}

impl GnomeDisplays {
    /// `lost` is the sender [`supervise`] bumps.
    pub fn new(
        conn: zbus::Connection,
        live: watch::Receiver<Arc<Live>>,
        cursor_mode: CursorMode,
        sinks: &Sinks,
        cuda: Option<Arc<CudaContext>>,
        placements: Placements,
        lost: watch::Sender<u64>,
    ) -> Self {
        Self {
            conn,
            live,
            cursor_mode,
            cursor: Arc::clone(&sinks.cursor),
            cuda,
            placements,
            extra: tokio::sync::Mutex::default(),
            lost,
        }
    }

    /// Adds a virtual monitor of `m`'s size and starts capturing it.
    async fn add(
        &self,
        screen_cast: &ScreenCastSessionProxy<'static>,
        m: &Monitor,
    ) -> Result<(Extra, MonitorStream, String)> {
        let v = mutter::add_virtual(&self.conn, screen_cast, self.cursor_mode).await?;
        let (tx, rx) = crate::frame::latest_frame();
        let sinks = Sinks {
            frames: tx,
            cursor: Arc::clone(&self.cursor),
        };
        let (node, w, h, cuda) = (
            v.node_id,
            u32::from(m.width),
            u32::from(m.height),
            self.cuda.clone(),
        );
        let started =
            tokio::task::spawn_blocking(move || Capture::start(node, w, h, sinks, cuda)).await;
        let capture = match started {
            Ok(Ok(c)) => Arc::new(Mutex::new(c)),
            Ok(Err(e)) => {
                let _ = v.stream.stop().await;
                return Err(e);
            }
            Err(e) => {
                let _ = v.stream.stop().await;
                return Err(e.into());
            }
        };
        info!(monitor = m.id, node, w, h, "Monitor added");
        Ok((
            Extra {
                stream: v.stream,
                capture: Arc::clone(&capture),
            },
            MonitorStream {
                monitor: m.id,
                source: Box::new(MonitorCapture(capture)),
                frames: rx,
            },
            v.path,
        ))
    }
}

#[async_trait::async_trait]
impl Displays for GnomeDisplays {
    async fn apply(&self, layout: &[Monitor]) -> Result<(Vec<MonitorStream>, Vec<Monitor>)> {
        let mut extra = self.extra.lock().await;
        self.placements.lock().unwrap().clear();
        for e in extra.drain(..) {
            // Its virtual monitor goes with it (an error: the session already closed).
            let _ = e.stream.stop().await;
            // The capture's thread ends as it is dropped (once its pipeline let it go).
            tokio::task::spawn_blocking(move || drop(e.capture));
        }
        let Some(first) = layout.first() else {
            return Ok((Vec::new(), Vec::new()));
        };
        let first = Monitor {
            x: 0,
            y: 0,
            ..*first
        };
        if layout.len() == 1 {
            return Ok((Vec::new(), vec![first]));
        }
        let screen_cast = self.live.borrow().screen_cast.clone();
        let (mut streams, mut created, mut paths) = (Vec::new(), vec![layout[0]], vec![None]);
        for m in &layout[1..] {
            match self.add(&screen_cast, m).await {
                Ok((e, s, path)) => {
                    extra.push(e);
                    streams.push(s);
                    created.push(*m);
                    paths.push(Some(path));
                }
                Err(e) => {
                    warn!(monitor = m.id, error = format!("{e:#}"), "Adding a monitor failed");
                }
            }
        }
        if created.len() == 1 {
            return Ok((Vec::new(), vec![first]));
        }
        let placed = match place(&self.conn, &created).await {
            Ok(p) => p,
            Err(e) => {
                // Where Mutter put them is unknown: the layout asked for, unchecked.
                warn!(error = format!("{e:#}"), "Placing the monitors failed");
                created.clone()
            }
        };
        *self.placements.lock().unwrap() = placed
            .iter()
            .zip(paths)
            .map(|(m, stream)| Placement {
                x: f64::from(m.x),
                y: f64::from(m.y),
                width: f64::from(m.width),
                height: f64::from(m.height),
                stream,
            })
            .collect();
        Ok((streams, placed))
    }

    fn lost(&self) -> watch::Receiver<u64> {
        self.lost.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, w: f64, h: f64, stream: Option<&str>) -> Placement {
        Placement {
            x,
            y: 0.0,
            width: w,
            height: h,
            stream: stream.map(str::to_owned),
        }
    }

    #[test]
    fn positions_go_to_the_monitor_they_are_on() {
        // Monitor 0 on the right of a shorter one.
        let p = [at(1280.0, 1920.0, 1080.0, None), at(0.0, 1280.0, 720.0, Some("/s/1"))];
        assert_eq!(locate(&p, 1280.0, 10.0), (None, 0.0, 10.0));
        assert_eq!(locate(&p, 1279.0, 10.0), (Some("/s/1".into()), 1279.0, 10.0));
        // Below the shorter monitor: the nearest, clamped onto it.
        assert_eq!(locate(&p, 100.0, 900.0), (Some("/s/1".into()), 100.0, 719.0));
        // One monitor: positions as they are.
        assert_eq!(locate(&[], 5.0, 6.0), (None, 5.0, 6.0));
    }

    fn current() -> HashMap<String, zbus::OwnedVariant> {
        HashMap::from([(
            "is-current".to_owned(),
            zbus::OwnedVariant::try_from(zbus::Variant::from(true)).unwrap(),
        )])
    }

    fn virtual_monitor(connector: &str, serial: &str, w: i32) -> MonitorState {
        (
            (
                connector.into(),
                VIRTUAL_VENDOR.into(),
                VIRTUAL_PRODUCT.into(),
                serial.into(),
            ),
            vec![(format!("{w}x1080@60"), w, 1080, 60.0, 1.0, vec![1.0], current())],
            HashMap::new(),
        )
    }

    fn logical_monitor(connector: &str, x: i32) -> LogicalMonitorState {
        (
            x,
            0,
            1.0,
            0,
            x == 0,
            vec![(connector.into(), VIRTUAL_VENDOR.into(), VIRTUAL_PRODUCT.into(), String::new())],
            HashMap::new(),
        )
    }

    #[test]
    fn our_virtual_monitors_in_creation_order() {
        // Mutter lists them in its own order; one (0x000002) belongs to a stream closing.
        let monitors = [
            virtual_monitor("Meta-3", "0x000004", 1280),
            virtual_monitor("Meta-0", "0x000001", 1920),
            virtual_monitor("Meta-1", "0x000002", 800),
            virtual_monitor("Meta-2", "0x000003", 1024),
        ];
        let logical = [
            logical_monitor("Meta-0", 0),
            logical_monitor("Meta-1", 1920),
            logical_monitor("Meta-2", 2720),
            logical_monitor("Meta-3", 3744),
        ];
        let got: Vec<String> = ours(&monitors, &logical, 3)
            .into_iter()
            .map(|v| v.connector)
            .collect();
        assert_eq!(got, ["Meta-0", "Meta-2", "Meta-3"]);
        // Not all created yet: what there is.
        assert_eq!(ours(&monitors[..2], &logical, 3).len(), 2);

        let mine = ours(&monitors, &logical, 3);
        let layout = [
            Monitor {
                id: 0,
                x: 1024,
                y: 0,
                width: 1920,
                height: 1080,
                refresh: 60,
                primary: true,
            },
            Monitor {
                id: 1,
                x: 0,
                y: 0,
                width: 1024,
                height: 1080,
                refresh: 60,
                primary: false,
            },
            Monitor {
                id: 2,
                x: 2944,
                y: 0,
                width: 1280,
                height: 1080,
                refresh: 60,
                primary: false,
            },
        ];
        let config = monitors_config(&mine, &layout, &monitors, &logical);
        // Ours where the layout has them, and the closing one left as it is.
        assert_eq!(config.len(), 4);
        assert_eq!((config[0].0, config[0].4, config[0].5[0].0.as_str()), (1024, true, "Meta-0"));
        assert_eq!((config[1].0, config[1].5[0].1.as_str()), (0, "1024x1080@60"));
        assert_eq!((config[3].0, config[3].4, config[3].5[0].0.as_str()), (1920, false, "Meta-1"));
        // Read back: Mutter's positions, from 0,0.
        let got = placed(&mine, &layout);
        assert_eq!(
            got.iter().map(|m| (m.id, m.x, m.width)).collect::<Vec<_>>(),
            [(0, 0, 1920), (1, 2720, 1024), (2, 3744, 1280)]
        );
    }
}

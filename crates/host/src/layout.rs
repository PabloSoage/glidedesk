//! Several monitors (ADR-0006, "Varias pantallas"): what a client asks for, set up as one
//! stream per monitor.
//!
//! Monitor 0 is the session's own capture and pipeline, as with a single monitor. For each
//! further monitor the session backend ([`Displays`], GNOME: a Mutter virtual monitor per
//! stream) creates a capture, and [`Manager`] gives it an encoder and a pipeline thread with
//! one output per front end ([`MonitorOutputs`]; front ends without them get
//! [`NoOutput`]). The layout set up, with the positions the session really has, goes out
//! through [`pipeline::Shared::layout`]; pointer positions are in its space (the top left
//! corner of the monitors' bounding box at 0,0).
//!
//! A frame per monitor rather than one spanning all: no pixels are wasted where the
//! monitors do not form a rectangle, and each stays within the encoders' 4096 pixels.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use glidedesk_proto::Monitor;
use tracing::{info, warn};

use crate::encoder;
use crate::frame::{FrameReceiver, Source};
use crate::pipeline::{self, MonitorOutputs, NoOutput, Output, Pipeline};

/// Most monitors a session shows: each is an encoder session (GeForce drivers allow 8 in
/// all) and a share of the link.
pub const MAX_MONITORS: usize = 4;

/// The capture of a further monitor, for its pipeline.
pub struct MonitorStream {
    pub monitor: u8,
    pub source: Box<dyn Source>,
    pub frames: FrameReceiver,
}

/// A session that can show more than one monitor.
#[async_trait::async_trait]
pub trait Displays: Send + Sync {
    /// Shows `layout` (from [`normalize`]: monitor 0 first, with the size it is being
    /// captured at): replaces any monitors set up before after the first, and places them
    /// all. Returns the captures of the monitors after the first that could be created, and
    /// where the session put every monitor (the layout clients are told; monitors that
    /// could not be created are left out). A layout of one removes the others.
    async fn apply(&self, layout: &[Monitor]) -> Result<(Vec<MonitorStream>, Vec<Monitor>)>;
}

/// What a client asked for, as the session will try it: monitor 0 (the one with id 0, else
/// the primary, else the first) first, at most `max` monitors with distinct ids, sizes the
/// encoders take ([`pipeline::clamp_size`]), and the bounding box at 0,0.
pub fn normalize(asked: &[Monitor], max: usize) -> Vec<Monitor> {
    let first = asked
        .iter()
        .position(|m| m.id == 0)
        .or_else(|| asked.iter().position(|m| m.primary))
        .unwrap_or(0);
    let mut out: Vec<Monitor> = Vec::new();
    let order = asked
        .get(first)
        .into_iter()
        .chain(asked.iter().enumerate().filter(|&(i, _)| i != first).map(|(_, m)| m));
    for m in order {
        if out.len() >= max.max(1) {
            break;
        }
        if out.iter().any(|o| o.id == m.id) {
            continue;
        }
        let (w, h) = pipeline::clamp_size(u32::from(m.width), u32::from(m.height));
        out.push(Monitor {
            width: w as u16,
            height: h as u16,
            ..*m
        });
    }
    if let Some(m) = out.first_mut() {
        m.id = 0;
    }
    let (min_x, min_y) = (
        out.iter().map(|m| m.x).min().unwrap_or(0),
        out.iter().map(|m| m.y).min().unwrap_or(0),
    );
    for m in &mut out {
        m.x -= min_x;
        m.y -= min_y;
    }
    out
}

/// Width and height of the monitors' bounding box (the layout from 0,0).
pub fn bounds(layout: &[Monitor]) -> (u32, u32) {
    let right = layout
        .iter()
        .map(|m| m.x.max(0) as u32 + u32::from(m.width))
        .max();
    let bottom = layout
        .iter()
        .map(|m| m.y.max(0) as u32 + u32::from(m.height))
        .max();
    (right.unwrap_or(0), bottom.unwrap_or(0))
}

/// Creates the encoder of a further monitor's stream.
pub type EncoderFactory = Arc<dyn Fn() -> Result<Box<dyn encoder::Backend>> + Send + Sync>;

/// The pipeline of a further monitor, running.
struct Running {
    monitor: u8,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Result<()>>,
}

/// Sets up the layouts clients ask for ([`pipeline::Shared::request_layout`]).
pub struct Manager {
    pub shared: Arc<pipeline::Shared>,
    /// `None`: the session shows one monitor.
    pub displays: Option<Arc<dyn Displays>>,
    pub encoders: EncoderFactory,
    /// Per front end, in the order of the pipeline's outputs.
    pub outputs: Vec<Option<Arc<dyn MonitorOutputs>>>,
    pub opts: pipeline::Options,
}

impl Manager {
    pub async fn run(self) {
        let mut requests = self.shared.layout_request.subscribe();
        let mut running: Vec<Running> = Vec::new();
        loop {
            if requests.changed().await.is_err() {
                break;
            }
            let asked = requests.borrow_and_update().clone();
            let max = if self.displays.is_some() {
                MAX_MONITORS
            } else {
                1
            };
            let mut wanted = normalize(&asked, max);
            let had_extras = !running.is_empty();
            stop_all(std::mem::take(&mut running)).await;
            let (w, h) = *self.shared.current_size.lock().unwrap();
            if wanted.len() > 1 {
                // Monitor 0 keeps the size it is captured at (its client asked for it at
                // login, or with a resize).
                wanted[0].width = w as u16;
                wanted[0].height = h as u16;
            }
            let single = || {
                vec![Monitor {
                    id: 0,
                    x: 0,
                    y: 0,
                    width: w as u16,
                    height: h as u16,
                    refresh: asked.first().map_or(60, |m| m.refresh),
                    primary: true,
                }]
            };
            let layout = match &self.displays {
                Some(d) if wanted.len() > 1 || had_extras => {
                    let target = if wanted.len() > 1 {
                        wanted.clone()
                    } else {
                        single()
                    };
                    match d.apply(&target).await {
                        Ok((streams, placed)) => {
                            let placed = self.start(streams, placed, &mut running).await;
                            if placed.is_empty() { single() } else { placed }
                        }
                        Err(e) => {
                            warn!(error = format!("{e:#}"), "Setting up the monitors failed");
                            single()
                        }
                    }
                }
                _ => single(),
            };
            *self.shared.areas.lock().unwrap() = if layout.len() > 1 {
                layout
                    .iter()
                    .map(|m| (m.id, u64::from(m.width) * u64::from(m.height)))
                    .collect()
            } else {
                Vec::new()
            };
            if layout.len() > 1 || asked.len() > 1 {
                info!(monitors = layout.len(), asked = asked.len(), ?layout, "Monitor layout");
            }
            self.shared.layout.send_replace(layout);
        }
        stop_all(running).await;
    }

    /// Starts a pipeline for each further monitor's stream; returns the layout without the
    /// monitors that could not get one.
    async fn start(
        &self,
        streams: Vec<MonitorStream>,
        mut placed: Vec<Monitor>,
        running: &mut Vec<Running>,
    ) -> Vec<Monitor> {
        for s in streams {
            let monitor = s.monitor;
            let outputs: Vec<Box<dyn Output>> = self
                .outputs
                .iter()
                .map(|o| match o {
                    Some(o) => o.output(monitor),
                    None => Box::new(NoOutput) as Box<dyn Output>,
                })
                .collect();
            let (shared, encoders, opts) = (
                Arc::clone(&self.shared),
                Arc::clone(&self.encoders),
                self.opts.clone(),
            );
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let started = tokio::task::spawn_blocking(move || {
                let backend = encoders()?;
                let pipeline = Pipeline::for_monitor(
                    shared, monitor, flag, s.source, s.frames, backend, outputs, opts,
                );
                std::thread::Builder::new()
                    .name(format!("pipeline-{monitor}"))
                    .spawn(move || pipeline.run())
                    .map_err(anyhow::Error::from)
            })
            .await;
            match started {
                Ok(Ok(thread)) => running.push(Running {
                    monitor,
                    stop,
                    thread,
                }),
                Ok(Err(e)) => {
                    warn!(monitor, error = format!("{e:#}"), "No encoder for the monitor");
                    placed.retain(|m| m.id != monitor);
                }
                Err(e) => {
                    warn!(monitor, %e, "No encoder for the monitor");
                    placed.retain(|m| m.id != monitor);
                }
            }
        }
        placed
    }
}

/// Ends further monitors' pipelines (their captures go with them).
async fn stop_all(running: Vec<Running>) {
    for r in &running {
        r.stop.store(true, Ordering::Relaxed);
    }
    for r in running {
        let monitor = r.monitor;
        match tokio::task::spawn_blocking(move || r.thread.join()).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => warn!(monitor, error = format!("{e:#}"), "Monitor pipeline failed"),
            _ => warn!(monitor, "Monitor pipeline panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u8, x: i32, y: i32, w: u16, h: u16, primary: bool) -> Monitor {
        Monitor {
            id,
            x,
            y,
            width: w,
            height: h,
            refresh: 60,
            primary,
        }
    }

    #[test]
    fn layouts_start_at_the_origin_with_monitor_zero_first() {
        // A second screen left of the primary, as Windows reports it.
        let asked = [m(1, -1920, 0, 1920, 1080, false), m(0, 0, 0, 2560, 1440, true)];
        let got = normalize(&asked, MAX_MONITORS);
        assert_eq!(
            got,
            vec![m(0, 1920, 0, 2560, 1440, true), m(1, 0, 0, 1920, 1080, false)]
        );
        assert_eq!(bounds(&got), (4480, 1440));
    }

    #[test]
    fn layouts_are_limited_and_sizes_clamped() {
        let asked: Vec<Monitor> = (0..6)
            .map(|i| m(i + 10, i32::from(i) * 1000, 0, 999, 8000, i == 2))
            .collect();
        let got = normalize(&asked, 3);
        assert_eq!(got.len(), 3);
        // No id 0: the primary becomes monitor 0.
        assert_eq!((got[0].id, got[0].primary), (0, true));
        assert!(got.iter().all(|m| m.width == 998 && m.height == 4096));
        assert_eq!(got.iter().map(|m| m.x).min(), Some(0));
        // Repeated ids count once; nothing asked, nothing set up.
        let twice = [m(0, 0, 0, 800, 600, true), m(0, 800, 0, 800, 600, false)];
        assert_eq!(normalize(&twice, 4).len(), 1);
        assert!(normalize(&[], 4).is_empty());
    }
}

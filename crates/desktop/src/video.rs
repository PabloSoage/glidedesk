//! Decoding: access units from the session go to a decoder thread ([`crate::decode`]: the
//! GPU's decoder, or FFmpeg in software), which keeps only the newest decoded picture and
//! wakes the window to show it. With several monitors each has a decoder thread of its own,
//! started when the server starts its video.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use glidedesk_client::gfx::{VideoCodec, VideoSink};
use tracing::{debug, info, warn};

use crate::decode::Decoder;

/// A decoded picture, full-range BT.709: the Y plane and the chroma.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    /// Bytes per row of the Y plane.
    pub y_stride: u32,
    pub chroma: Chroma,
    /// When its access unit arrived (for the latency statistics).
    pub received: Instant,
}

/// The chroma of a [`Picture`].
pub enum Chroma {
    /// 4:2:0 as NV12: U and V interleaved, half the width and height.
    Nv12 { uv: Vec<u8>, stride: u32 },
    /// 4:4:4: U and V planes as large as the Y plane, `stride` bytes per row each.
    Planar { u: Vec<u8>, v: Vec<u8>, stride: u32 },
}

/// What the session hands to the window besides pictures.
pub enum Event {
    /// A new picture is in the slot.
    Picture,
    /// A new picture is in the slot of the window of this further monitor.
    MonitorPicture(u8),
    /// The monitors of the remote desktop, as the server set them up.
    Layout(Vec<glidedesk_proto::Monitor>),
    Cursor(glidedesk_proto::Cursor),
    /// Text copied in the session.
    Clipboard(String),
    /// The link is poor (a short line), or good again.
    LinkWarning(Option<String>),
    /// Files copied in the session, fetched to these paths.
    Files(Vec<std::path::PathBuf>),
    Ended(String),
    /// Window menu entries (Windows).
    #[cfg_attr(not(windows), allow(dead_code))]
    ToggleFullscreen,
    #[cfg_attr(not(windows), allow(dead_code))]
    ShowLog,
}

pub type Notify = Arc<dyn Fn(Event) + Send + Sync>;

enum Unit {
    Reset(VideoCodec),
    Data(Vec<u8>, Instant),
}

/// The session's [`VideoSink`]: forwards to the decoder thread, the pointer to the window,
/// audio to the player.
pub struct Sink {
    units: mpsc::Sender<Unit>,
    /// Access units sent to the decoder thread and not decoded yet.
    queued: Arc<AtomicUsize>,
    notify: Notify,
    audio: Option<crate::audio::Player>,
    /// Ask native servers for HEVC 4:4:4 (`--yuv444`).
    yuv444: bool,
    hardware: bool,
    /// The windows of further monitors (native protocol, version 6).
    monitors: Vec<MonitorVideo>,
}

/// The video of a further monitor: its window's slot, and its decoder thread once the server
/// starts the monitor's video.
struct MonitorVideo {
    monitor: u8,
    slot: Arc<Mutex<Option<Picture>>>,
    decoder: Option<(mpsc::Sender<Unit>, Arc<AtomicUsize>)>,
}

impl VideoSink for Sink {
    fn reset(&mut self, _width: u32, _height: u32, codec: VideoCodec) {
        let _ = self.units.send(Unit::Reset(codec));
    }

    fn frame(&mut self, data: &[u8]) {
        self.queued.fetch_add(1, Ordering::Relaxed);
        if self
            .units
            .send(Unit::Data(data.to_vec(), Instant::now()))
            .is_err()
        {
            self.queued.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn backlog(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    fn yuv444(&self) -> bool {
        self.yuv444
    }

    fn cursor(&mut self, cursor: &glidedesk_proto::Cursor) {
        (self.notify)(Event::Cursor(cursor.clone()));
    }

    fn audio(&mut self, pcm: &[u8]) {
        if let Some(a) = &self.audio {
            a.push(pcm);
        }
    }

    fn clipboard(&mut self, text: &str) {
        (self.notify)(Event::Clipboard(text.to_owned()));
    }

    fn link_warning(&mut self, warning: Option<&str>) {
        (self.notify)(Event::LinkWarning(warning.map(str::to_owned)));
    }

    fn files(&mut self, paths: Vec<std::path::PathBuf>) {
        (self.notify)(Event::Files(paths));
    }

    fn layout(&mut self, monitors: &[glidedesk_proto::Monitor]) {
        (self.notify)(Event::Layout(monitors.to_vec()));
    }

    fn reset_monitor(&mut self, monitor: u8, _width: u32, _height: u32, codec: VideoCodec) {
        let Some(m) = self.monitors.iter_mut().find(|m| m.monitor == monitor) else {
            warn!(monitor, "Video for a monitor with no window");
            return;
        };
        let (units, _) = m.decoder.get_or_insert_with(|| {
            spawn(
                monitor,
                Arc::clone(&m.slot),
                Arc::clone(&self.notify),
                self.hardware,
            )
        });
        let _ = units.send(Unit::Reset(codec));
    }

    fn frame_monitor(&mut self, monitor: u8, data: &[u8]) {
        let Some((units, queued)) = self
            .monitors
            .iter()
            .find(|m| m.monitor == monitor)
            .and_then(|m| m.decoder.as_ref())
        else {
            return;
        };
        queued.fetch_add(1, Ordering::Relaxed);
        if units.send(Unit::Data(data.to_vec(), Instant::now())).is_err() {
            queued.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn backlog_monitor(&self, monitor: u8) -> usize {
        self.monitors
            .iter()
            .find(|m| m.monitor == monitor)
            .and_then(|m| m.decoder.as_ref())
            .map_or(0, |(_, queued)| queued.load(Ordering::Relaxed))
    }
}

/// Starts the decoder thread; pictures land in `slot`. `hardware`: use the GPU's decoder
/// when there is one. `yuv444`: ask for HEVC 4:4:4 (decoded on the GPU where FFmpeg can,
/// else in software).
/// `monitors`: the windows of further monitors and their slots, by monitor id (their
/// decoders start with their video).
pub fn start(
    slot: Arc<Mutex<Option<Picture>>>,
    notify: Notify,
    audio: Option<crate::audio::Player>,
    hardware: bool,
    yuv444: bool,
    monitors: Vec<(u8, Arc<Mutex<Option<Picture>>>)>,
) -> Sink {
    let (units, queued) = spawn(0, slot, Arc::clone(&notify), hardware);
    Sink {
        units,
        queued,
        notify,
        audio,
        yuv444,
        hardware,
        monitors: monitors
            .into_iter()
            .map(|(monitor, slot)| MonitorVideo {
                monitor,
                slot,
                decoder: None,
            })
            .collect(),
    }
}

/// A decoder thread for the video of `monitor`: where its access units go, and how many
/// wait.
fn spawn(
    monitor: u8,
    slot: Arc<Mutex<Option<Picture>>>,
    notify: Notify,
    hardware: bool,
) -> (mpsc::Sender<Unit>, Arc<AtomicUsize>) {
    let (tx, rx) = mpsc::channel::<Unit>();
    let queued = Arc::new(AtomicUsize::new(0));
    let q = Arc::clone(&queued);
    let name = if monitor == 0 {
        "decoder".to_owned()
    } else {
        format!("decoder-{monitor}")
    };
    std::thread::Builder::new()
        .name(name)
        .spawn(move || decode_loop(&rx, &slot, &notify, &q, hardware, monitor))
        .expect("spawning the decoder thread");
    (tx, queued)
}

fn decode_loop(
    rx: &mpsc::Receiver<Unit>,
    slot: &Mutex<Option<Picture>>,
    notify: &Notify,
    queued: &AtomicUsize,
    hardware: bool,
    monitor: u8,
) {
    let mut decoder: Option<Decoder> = None;
    let (mut frames, mut on_gpu, mut decode_us, mut since) = (0u32, 0u32, 0u128, Instant::now());
    while let Ok(unit) = rx.recv() {
        match unit {
            Unit::Reset(codec) => {
                // A new stream starts with an IDR; a fresh decoder drops stale state.
                decoder = match Decoder::new(codec, hardware) {
                    Ok(d) => {
                        info!(
                            monitor,
                            ?codec,
                            hardware = d.hardware().unwrap_or("none"),
                            "Decoder"
                        );
                        Some(d)
                    }
                    Err(e) => {
                        warn!(%e, "No decoder");
                        None
                    }
                };
            }
            Unit::Data(data, received) => {
                let Some(dec) = decoder.as_mut() else {
                    queued.fetch_sub(1, Ordering::Relaxed);
                    continue;
                };
                let t = Instant::now();
                let decoded = dec.decode(&data, received);
                queued.fetch_sub(1, Ordering::Relaxed);
                match decoded {
                    Ok(Some((pic, gpu))) => {
                        decode_us += t.elapsed().as_micros();
                        frames += 1;
                        on_gpu += u32::from(gpu);
                        *slot.lock().unwrap() = Some(pic);
                        notify(if monitor == 0 {
                            Event::Picture
                        } else {
                            Event::MonitorPicture(monitor)
                        });
                    }
                    Ok(None) => {}
                    Err(e) => debug!(%e, "Decode error"),
                }
                if since.elapsed().as_secs() >= 5 {
                    debug!(
                        frames,
                        on_gpu,
                        decode_ms = decode_us as f64 / f64::from(frames.max(1)) / 1000.0,
                        "Decoder"
                    );
                    on_gpu = 0;
                    (frames, decode_us, since) = (0, 0, Instant::now());
                }
            }
        }
    }
}

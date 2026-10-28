//! Decoding: access units from the session go to a decoder thread ([`crate::decode`]: the
//! GPU's decoder, or FFmpeg in software), which keeps only the newest decoded picture and
//! wakes the window to show it.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use glidedesk_client::gfx::{VideoCodec, VideoSink};
use tracing::{debug, info, warn};

use crate::decode::Decoder;

/// A decoded picture: NV12 (Y plane, interleaved UV plane at half size), full-range BT.709.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
    /// Bytes per row of each plane.
    pub y_stride: u32,
    pub uv_stride: u32,
    /// When its access unit arrived (for the latency statistics).
    pub received: Instant,
}

/// What the session hands to the window besides pictures.
pub enum Event {
    /// A new picture is in the slot.
    Picture,
    Cursor(glidedesk_proto::Cursor),
    /// Text copied in the session.
    Clipboard(String),
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
    notify: Notify,
    audio: Option<crate::audio::Player>,
}

impl VideoSink for Sink {
    fn reset(&mut self, _width: u32, _height: u32, codec: VideoCodec) {
        let _ = self.units.send(Unit::Reset(codec));
    }

    fn frame(&mut self, data: &[u8]) {
        let _ = self.units.send(Unit::Data(data.to_vec(), Instant::now()));
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
}

/// Starts the decoder thread; pictures land in `slot`. `hardware`: use the GPU's decoder
/// when there is one.
pub fn start(
    slot: Arc<Mutex<Option<Picture>>>,
    notify: Notify,
    audio: Option<crate::audio::Player>,
    hardware: bool,
) -> Sink {
    let (tx, rx) = mpsc::channel::<Unit>();
    let n = Arc::clone(&notify);
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || decode_loop(&rx, &slot, &n, hardware))
        .expect("spawning the decoder thread");
    Sink {
        units: tx,
        notify,
        audio,
    }
}

fn decode_loop(
    rx: &mpsc::Receiver<Unit>,
    slot: &Mutex<Option<Picture>>,
    notify: &Notify,
    hardware: bool,
) {
    let mut decoder: Option<Decoder> = None;
    let (mut frames, mut on_gpu, mut decode_us, mut since) = (0u32, 0u32, 0u128, Instant::now());
    while let Ok(unit) = rx.recv() {
        match unit {
            Unit::Reset(codec) => {
                // A new stream starts with an IDR; a fresh decoder drops stale state.
                decoder = match Decoder::new(codec, hardware) {
                    Ok(d) => {
                        info!(?codec, hardware = d.hardware().unwrap_or("none"), "Decoder");
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
                    continue;
                };
                let t = Instant::now();
                match dec.decode(&data, received) {
                    Ok(Some((pic, gpu))) => {
                        decode_us += t.elapsed().as_micros();
                        frames += 1;
                        on_gpu += u32::from(gpu);
                        *slot.lock().unwrap() = Some(pic);
                        notify(Event::Picture);
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

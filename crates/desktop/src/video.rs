//! Decoding: access units from the session go to a decoder thread (OpenH264, software), which
//! keeps only the newest decoded picture and wakes the window to show it.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use glidedesk_client::gfx::{VideoCodec, VideoSink};
use openh264::formats::YUVSource;
use tracing::{debug, warn};

/// A decoded picture: planar YUV 4:2:0, full-range BT.709.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
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

/// Starts the decoder thread; pictures land in `slot`.
pub fn start(
    slot: Arc<Mutex<Option<Picture>>>,
    notify: Notify,
    audio: Option<crate::audio::Player>,
) -> Sink {
    let (tx, rx) = mpsc::channel::<Unit>();
    let n = Arc::clone(&notify);
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || decode_loop(&rx, &slot, &n))
        .expect("spawning the decoder thread");
    Sink {
        units: tx,
        notify,
        audio,
    }
}

fn decode_loop(rx: &mpsc::Receiver<Unit>, slot: &Mutex<Option<Picture>>, notify: &Notify) {
    let mut decoder = openh264::decoder::Decoder::new().ok();
    let (mut frames, mut decode_us, mut since) = (0u32, 0u128, Instant::now());
    while let Ok(unit) = rx.recv() {
        match unit {
            Unit::Reset(codec) => {
                if codec != VideoCodec::H264 {
                    warn!(?codec, "Only H.264 can be decoded here");
                }
                // A new stream starts with an IDR; a fresh decoder drops stale state.
                decoder = openh264::decoder::Decoder::new().ok();
            }
            Unit::Data(data, received) => {
                let Some(dec) = decoder.as_mut() else {
                    continue;
                };
                let t = Instant::now();
                match dec.decode(&data) {
                    Ok(Some(yuv)) => {
                        let (w, h) = yuv.dimensions();
                        let (ys, us, _) = yuv.strides();
                        let pic = Picture {
                            width: w as u32,
                            height: h as u32,
                            y: yuv.y()[..ys * h].to_vec(),
                            u: yuv.u()[..us * h.div_ceil(2)].to_vec(),
                            v: yuv.v()[..us * h.div_ceil(2)].to_vec(),
                            y_stride: ys as u32,
                            uv_stride: us as u32,
                            received,
                        };
                        decode_us += t.elapsed().as_micros();
                        frames += 1;
                        *slot.lock().unwrap() = Some(pic);
                        notify(Event::Picture);
                    }
                    Ok(None) => {}
                    Err(e) => debug!(%e, "Decode error"),
                }
                if since.elapsed().as_secs() >= 5 {
                    debug!(
                        frames,
                        decode_ms = decode_us as f64 / f64::from(frames.max(1)) / 1000.0,
                        "Decoder"
                    );
                    (frames, decode_us, since) = (0, 0, Instant::now());
                }
            }
        }
    }
}

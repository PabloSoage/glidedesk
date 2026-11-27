//! Video of the phone server: the size the encoder works at, the encoder's parameter sets in
//! front of each keyframe, and the bitrate, from the client's losses and delays.
//!
//! MediaCodec encodes whatever the display draws, at the display's pace: frames cannot be
//! held back waiting for acknowledgements as the desktop host does (a skipped P-frame would
//! break the chain), so the link is kept clear by lowering the encoder's bitrate instead.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Sizes are multiples of this (phone encoders want 8 or 16; scrcpy uses 8 as well).
pub const ALIGN: u32 = 8;
/// Long sides tried, in turn, when the encoder refuses a size (scrcpy's
/// `MAX_SIZE_FALLBACK`, `server/.../video/SurfaceEncoder.java`).
pub const FALLBACK: [u32; 6] = [2560, 1920, 1600, 1280, 1024, 800];

/// MediaCodec's `BUFFER_FLAG_*` bits, as Kotlin hands them over.
pub const FLAG_KEY_FRAME: i32 = 1;
pub const FLAG_CODEC_CONFIG: i32 = 2;

/// Rounded down to [`ALIGN`] (a hair over, so 863.99999 from float scaling counts as 864).
fn align(v: f64) -> u32 {
    (((v + 1e-6) as u32) / ALIGN * ALIGN).max(ALIGN)
}

/// The size to encode for a client asking for `client`: with `own_display` the client's
/// size (a display of its own is created at it), otherwise the phone's `display` scaled down
/// to fit in it. Never more than `max_long` on the long side.
pub fn video_size(
    display: (u32, u32),
    client: (u32, u32),
    max_long: u32,
    own_display: bool,
) -> (u32, u32) {
    let (w, h) = if own_display { client } else { display };
    let (w, h) = (f64::from(w.max(1)), f64::from(h.max(1)));
    let mut scale = f64::from(max_long.max(ALIGN)) / w.max(h);
    if !own_display {
        let (cw, ch) = (f64::from(client.0.max(1)), f64::from(client.1.max(1)));
        scale = scale.min(cw / w).min(ch / h);
    }
    let scale = scale.min(1.0);
    (align(w * scale), align(h * scale))
}

/// The next smaller long side after an encoder refused `size`, if any.
pub fn smaller(size: (u32, u32)) -> Option<u32> {
    let long = size.0.max(size.1);
    FALLBACK.into_iter().find(|&f| f < long)
}

/// The encoder's parameter sets (SPS/PPS, and VPS for HEVC) come in a buffer of their own,
/// once; decoders need them with every keyframe (a client joins at any keyframe).
#[derive(Default)]
pub struct ParameterSets {
    config: Vec<u8>,
}

impl ParameterSets {
    /// One encoder output buffer: the access unit to send, or `None` for the parameter
    /// sets themselves (kept for the keyframes).
    pub fn frame<'a>(&mut self, data: &'a [u8], flags: i32) -> Option<Cow<'a, [u8]>> {
        if flags & FLAG_CODEC_CONFIG != 0 {
            self.config = data.to_vec();
            return None;
        }
        if flags & FLAG_KEY_FRAME != 0 && !self.config.is_empty() {
            let mut au = Vec::with_capacity(self.config.len() + data.len());
            au.extend_from_slice(&self.config);
            au.extend_from_slice(data);
            return Some(Cow::Owned(au));
        }
        Some(Cow::Borrowed(data))
    }

    /// A new encoder: its parameter sets come again.
    pub fn clear(&mut self) {
        self.config.clear();
    }
}

/// Lowest bitrate the control goes to.
pub const MIN_BPS: u32 = 500_000;
/// Starting bitrate (capped by the user's maximum).
pub const START_BPS: u32 = 8_000_000;
/// Delay over the lowest seen that means packets are queuing somewhere.
const QUEUING: Duration = Duration::from_millis(40);
/// How long the link must stay clean before the bitrate goes up again.
const CLEAN: Duration = Duration::from_secs(3);
/// Frames remembered for their acknowledgement.
const SENT_KEPT: usize = 128;

/// The encoder's bitrate, from the client's loss reports (protocol version 4) and the
/// delay between sending a frame and its acknowledgement.
#[derive(Debug)]
pub struct RateControl {
    max: u32,
    bps: u32,
    /// Last value handed to the encoder.
    reported: u32,
    sent: VecDeque<(u32, Instant)>,
    delays: Vec<Duration>,
    /// Lowest send-to-acknowledge delay seen lately: the link without queues.
    floor: Option<Duration>,
    loss: Option<f32>,
    last_cut: Instant,
}

impl RateControl {
    pub fn new(max: u32, now: Instant) -> Self {
        let max = max.max(MIN_BPS);
        let bps = START_BPS.min(max);
        Self {
            max,
            bps,
            reported: bps,
            sent: VecDeque::new(),
            delays: Vec::new(),
            floor: None,
            loss: None,
            last_cut: now,
        }
    }

    pub fn bps(&self) -> u32 {
        self.bps
    }

    pub fn sent(&mut self, frame_id: u32, now: Instant) {
        if self.sent.len() >= SENT_KEPT {
            self.sent.pop_front();
        }
        self.sent.push_back((frame_id, now));
    }

    pub fn acked(&mut self, frame_id: u32, now: Instant) {
        let Some(i) = self.sent.iter().position(|&(id, _)| id == frame_id) else {
            return;
        };
        let (_, at) = self.sent[i];
        self.sent.drain(..=i);
        self.delays.push(now.saturating_duration_since(at));
    }

    /// Share of packets lost in the client's latest report.
    pub fn loss(&mut self, fraction: f32) {
        self.loss = Some(self.loss.map_or(fraction, |l| l.max(fraction)));
    }

    /// About once a second: the new bitrate for the encoder, when it changed enough to set.
    pub fn tick(&mut self, now: Instant) -> Option<u32> {
        let delays = std::mem::take(&mut self.delays);
        let mut queuing = false;
        if let Some(&low) = delays.iter().min() {
            // The floor follows a slower route up slowly, a faster one at once.
            let floor = self
                .floor
                .map_or(low, |f| (f + Duration::from_millis(2)).min(low));
            self.floor = Some(floor);
            let mean = delays.iter().sum::<Duration>() / delays.len() as u32;
            queuing = mean > floor + QUEUING;
        }
        let loss = self.loss.take().unwrap_or(0.0);
        let next = if loss > 0.10 {
            self.last_cut = now;
            f64::from(self.bps) * 0.7
        } else if loss > 0.02 || queuing {
            self.last_cut = now;
            f64::from(self.bps) * 0.85
        } else if now.saturating_duration_since(self.last_cut) >= CLEAN {
            f64::from(self.bps) * 1.06
        } else {
            f64::from(self.bps)
        };
        self.bps = (next.round() as u32).clamp(MIN_BPS, self.max);
        let change = self.bps.abs_diff(self.reported);
        (change > self.reported / 50).then(|| {
            self.reported = self.bps;
            self.bps
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_fits_the_display_in_the_client() {
        // A portrait phone in a landscape window: the height decides.
        assert_eq!(video_size((1080, 2400), (1920, 1080), 1920, false), (480, 1080));
        // A large window: the display's own size, capped on the long side.
        assert_eq!(video_size((1080, 2400), (3000, 3000), 1920, false), (864, 1920));
        assert_eq!(video_size((1080, 2400), (3000, 3000), 4096, false), (1080, 2400));
        // Never scaled up.
        assert_eq!(video_size((720, 1600), (4000, 4000), 4096, false), (720, 1600));
    }

    #[test]
    fn own_display_takes_the_client_size() {
        assert_eq!(video_size((1080, 2400), (1920, 1080), 1920, true), (1920, 1080));
        assert_eq!(video_size((1080, 2400), (1366, 768), 1920, true), (1360, 768));
        assert_eq!(video_size((1080, 2400), (2560, 1440), 1920, true), (1920, 1080));
        assert_eq!(video_size((1080, 2400), (0, 0), 1920, true), (8, 8));
    }

    #[test]
    fn fallback_sizes() {
        assert_eq!(smaller((1080, 2400)), Some(1920));
        assert_eq!(smaller((864, 1920)), Some(1600));
        assert_eq!(smaller((800, 600)), None);
    }

    #[test]
    fn parameter_sets_go_before_keyframes() {
        let mut p = ParameterSets::default();
        assert_eq!(p.frame(&[1, 2], FLAG_CODEC_CONFIG), None);
        assert_eq!(p.frame(&[9], FLAG_KEY_FRAME).as_deref(), Some(&[1, 2, 9][..]));
        assert_eq!(p.frame(&[8], 0).as_deref(), Some(&[8][..]));
        p.clear();
        assert_eq!(p.frame(&[7], FLAG_KEY_FRAME).as_deref(), Some(&[7][..]));
    }

    #[test]
    fn rate_falls_on_losses_and_climbs_when_clean() {
        let t0 = Instant::now();
        let mut r = RateControl::new(16_000_000, t0);
        assert_eq!(r.bps(), START_BPS);
        r.loss(0.2);
        assert_eq!(r.tick(t0 + Duration::from_secs(1)), Some(5_600_000));
        // Clean, but not for long enough yet.
        assert_eq!(r.tick(t0 + Duration::from_secs(2)), None);
        let up = r.tick(t0 + Duration::from_secs(5));
        assert!(up.is_some_and(|b| b > 5_600_000));
        // Never past the maximum, nor under the minimum.
        let mut t = t0 + Duration::from_secs(5);
        for _ in 0..100 {
            t += Duration::from_secs(1);
            r.tick(t);
        }
        assert_eq!(r.bps(), 16_000_000);
        for _ in 0..100 {
            t += Duration::from_secs(1);
            r.loss(0.5);
            r.tick(t);
        }
        assert_eq!(r.bps(), MIN_BPS);
    }

    #[test]
    fn queuing_delay_lowers_the_rate() {
        let t0 = Instant::now();
        let mut r = RateControl::new(16_000_000, t0);
        for id in 0..10u32 {
            let at = t0 + Duration::from_millis(u64::from(id) * 10);
            r.sent(id, at);
            r.acked(id, at + Duration::from_millis(5));
        }
        r.tick(t0 + Duration::from_secs(1));
        let before = r.bps();
        let t1 = t0 + Duration::from_secs(1);
        for id in 10..20u32 {
            let at = t1 + Duration::from_millis(u64::from(id) * 10);
            r.sent(id, at);
            r.acked(id, at + Duration::from_millis(80));
        }
        assert!(r.tick(t1 + Duration::from_secs(1)).is_some());
        assert!(r.bps() < before);
    }
}

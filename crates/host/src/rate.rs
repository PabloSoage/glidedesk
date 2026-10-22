//! Link-adaptive bitrate: no set bandwidth, unlike Moonlight.
//!
//! Frames are encoded at a constant quality, so a still or simple desktop costs little
//! whatever the link. Only the ceiling a frame may reach follows the link, from how long
//! frames take from sending to the client's acknowledgement:
//!
//! - The least delay seen recently is the link's floor (round trip plus decoding).
//! - When the typical delay rises clearly above the floor, frames are queuing somewhere
//!   (Wi-Fi, a mobile link, a slow uplink): the ceiling drops at once.
//! - While the delay stays near the floor and frames actually use the ceiling, it climbs
//!   back step by step, up to the user's limit (`--max-bitrate`) if any.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Send times of frames until the client acknowledges them.
#[derive(Default)]
pub struct DelayMeter {
    sent: VecDeque<(u32, Instant, usize)>,
    /// Delay and size of each acknowledged frame since the last `take`.
    acked: Vec<(Duration, usize)>,
}

impl DelayMeter {
    pub fn sent(&mut self, id: u32, bytes: usize) {
        self.sent.push_back((id, Instant::now(), bytes));
        if self.sent.len() > 1024 {
            self.sent.pop_front();
        }
    }

    /// Frame `id` was acknowledged; earlier frames that never were are forgotten.
    pub fn acked(&mut self, id: u32) {
        while let Some(&(sid, t, bytes)) = self.sent.front() {
            // Later than `id` (ids wrap): wait for its own acknowledgement.
            if (id.wrapping_sub(sid) as i32) < 0 {
                break;
            }
            self.sent.pop_front();
            if sid == id {
                self.acked.push((t.elapsed(), bytes));
                break;
            }
        }
    }

    /// A new connection: its frame ids start over.
    pub fn clear(&mut self) {
        self.sent.clear();
        self.acked.clear();
    }

    pub fn take(&mut self) -> Vec<(Duration, usize)> {
        std::mem::take(&mut self.acked)
    }
}

/// Lowest ceiling: a usable picture on a poor mobile link.
const MIN_BPS: u32 = 1_000_000;
/// Where a new session starts: fine for Wi-Fi and wired LANs, and quick to come down.
pub const START_BPS: u32 = 50_000_000;
/// Highest ceiling without a user limit.
const TOP_BPS: u32 = 400_000_000;
/// Delay above the floor that means frames are queuing.
const QUEUING: Duration = Duration::from_millis(25);
/// How often the delays are looked at.
const PERIOD: Duration = Duration::from_millis(200);
/// Time for a cut to show in the delays before the next one.
const CUT_HOLD: Duration = Duration::from_millis(400);
/// Clean time before each step up.
const RAISE_AFTER: Duration = Duration::from_secs(1);
/// The floor is the least delay over this long, so a slower route is learned.
const FLOOR_WINDOW: Duration = Duration::from_secs(10);

pub struct Rate {
    bps: u32,
    max: u32,
    /// Least delay of each period, for the floor.
    mins: VecDeque<(Instant, Duration)>,
    delays: Vec<Duration>,
    bytes: usize,
    period_start: Instant,
    last_cut: Instant,
    clean_since: Instant,
}

impl Rate {
    pub fn new(max: Option<u32>) -> Self {
        let max = max.unwrap_or(TOP_BPS).max(MIN_BPS);
        let now = Instant::now();
        Self {
            bps: START_BPS.min(max),
            max,
            mins: VecDeque::new(),
            delays: Vec::new(),
            bytes: 0,
            period_start: now,
            last_cut: now,
            clean_since: now,
        }
    }

    pub fn bps(&self) -> u32 {
        self.bps
    }

    /// Takes acknowledged frames; returns the new ceiling when it changes.
    pub fn update(&mut self, acked: Vec<(Duration, usize)>) -> Option<u32> {
        self.update_at(acked, Instant::now())
    }

    fn update_at(&mut self, acked: Vec<(Duration, usize)>, now: Instant) -> Option<u32> {
        for (d, b) in acked {
            self.delays.push(d);
            self.bytes += b;
        }
        let elapsed = now.duration_since(self.period_start);
        if elapsed < PERIOD {
            return None;
        }
        let mut delays = std::mem::take(&mut self.delays);
        let used_bps = (self.bytes as f64 * 8.0 / elapsed.as_secs_f64()) as u64;
        self.bytes = 0;
        self.period_start = now;
        if delays.is_empty() {
            // Nothing moved on screen: no news about the link.
            self.clean_since = now;
            return None;
        }
        delays.sort();
        let typical = delays[delays.len() / 2];
        self.mins.push_back((now, delays[0]));
        while self
            .mins
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > FLOOR_WINDOW)
        {
            self.mins.pop_front();
        }
        let floor = self.mins.iter().map(|(_, d)| *d).min().unwrap_or(delays[0]);

        if typical > floor + QUEUING {
            self.clean_since = now;
            if now.duration_since(self.last_cut) < CUT_HOLD {
                return None;
            }
            self.last_cut = now;
            let bps = (self.bps / 10 * 7).max(MIN_BPS);
            return self.set(bps);
        }
        if typical > floor + QUEUING / 2 {
            self.clean_since = now;
            return None;
        }
        // Only a ceiling that frames actually reach says anything about the link.
        if now.duration_since(self.clean_since) >= RAISE_AFTER
            && used_bps * 2 >= u64::from(self.bps)
        {
            self.clean_since = now;
            let bps = self
                .bps
                .saturating_add((self.bps / 10).max(1_000_000))
                .min(self.max);
            return self.set(bps);
        }
        None
    }

    fn set(&mut self, bps: u32) -> Option<u32> {
        (bps != self.bps).then(|| {
            self.bps = bps;
            bps
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn meter_matches_ids_and_forgets_lost_frames() {
        let mut m = DelayMeter::default();
        for id in [u32::MAX - 1, u32::MAX, 0, 1] {
            m.sent(id, 100);
        }
        m.acked(u32::MAX);
        m.acked(1);
        let acked = m.take();
        assert_eq!(acked.len(), 2);
        assert!(m.sent.is_empty());
        // An id never sent changes nothing.
        m.sent(5, 1);
        m.acked(4);
        assert_eq!(m.sent.len(), 1);
    }

    #[test]
    fn rate_cuts_on_queuing_and_climbs_back() {
        let t0 = Instant::now();
        let mut r = Rate::new(Some(60_000_000));
        assert_eq!(r.bps(), 50_000_000);
        let big = 2_000_000; // bytes per period: well above half the ceiling
        // Clean link at 10 ms: the floor.
        let mut t = t0;
        for _ in 0..3 {
            t += PERIOD;
            r.update_at(vec![(ms(10), big)], t);
        }
        // Queuing: 80 ms.
        t += PERIOD;
        assert_eq!(r.update_at(vec![(ms(80), big)], t), Some(35_000_000));
        // Within the hold: no second cut yet.
        t += PERIOD;
        assert_eq!(r.update_at(vec![(ms(80), big)], t), None);
        t += PERIOD;
        assert_eq!(r.update_at(vec![(ms(80), big)], t), Some(24_500_000));
        // Clean again: steps up after a second, never above the user's limit.
        let mut last = r.bps();
        for _ in 0..200 {
            t += PERIOD;
            if let Some(b) = r.update_at(vec![(ms(10), big)], t) {
                assert!(b > last);
                last = b;
            }
        }
        assert_eq!(r.bps(), 60_000_000);
    }

    #[test]
    fn rate_holds_while_frames_do_not_use_the_ceiling() {
        let t0 = Instant::now();
        let mut r = Rate::new(None);
        let mut t = t0;
        for _ in 0..50 {
            t += PERIOD;
            assert_eq!(r.update_at(vec![(ms(5), 1000)], t), None);
        }
        assert_eq!(r.bps(), START_BPS);
    }
}

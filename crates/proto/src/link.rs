//! How the media path is doing, seen from both ends (protocol version 4).
//!
//! - The client counts the UDP packets it gets against their sequence numbers
//!   ([`LossCounter`]) and reports the share lost about once a second
//!   ([`crate::ClientMsg::LinkReport`]).
//! - The server sizes the Reed-Solomon parity to the losses ([`FecControl`]) and paces its
//!   bursts to the bitrate ceiling ([`pacing`]).
//! - The client tells the user when the link is poor ([`Quality`]), from the losses, the
//!   frames that could not be rebuilt and the round trip of its pings.

use std::time::Duration;

/// Counts received packets against the gaps in their sequence numbers.
///
/// The server numbers every packet it sends to a client (video, audio, sync) from 1 up, so
/// the packets that should have arrived in a period are the ones between the highest number
/// seen at its start and the highest seen at its end. Packets that arrive late, out of
/// order, are counted in the period they arrive in, which can only make that period look a
/// little better than it was.
#[derive(Debug, Default)]
pub struct LossCounter {
    /// Highest sequence number seen.
    highest: u64,
    /// `highest` when the period started.
    base: u64,
    received: u64,
}

/// One period of [`LossCounter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossSample {
    pub received: u32,
    pub lost: u32,
}

impl LossSample {
    /// Share of the packets lost, 0 to 1.
    pub fn fraction(&self) -> f32 {
        let total = u64::from(self.received) + u64::from(self.lost);
        if total == 0 {
            0.0
        } else {
            self.lost as f32 / total as f32
        }
    }
}

impl LossCounter {
    /// A packet with sequence number `seq` arrived (already checked against replays).
    pub fn packet(&mut self, seq: u64) {
        if self.highest == 0 && self.base == 0 {
            // The first packet: the period starts just before it.
            self.base = seq.saturating_sub(1);
        }
        self.highest = self.highest.max(seq);
        self.received += 1;
    }

    /// Ends the period; `None` if nothing was expected in it.
    pub fn take(&mut self) -> Option<LossSample> {
        let expected = self.highest.saturating_sub(self.base);
        let received = self.received;
        self.base = self.highest;
        self.received = 0;
        if expected == 0 && received == 0 {
            return None;
        }
        let lost = expected.saturating_sub(received);
        Some(LossSample {
            received: received.min(u64::from(u32::MAX)) as u32,
            lost: lost.min(u64::from(u32::MAX)) as u32,
        })
    }
}

/// Parity of the first frames, before any report.
pub const FEC_START: u32 = 20;
/// Least parity: one lost packet in ten is still rebuilt on a clean link.
pub const FEC_MIN: u32 = 10;
/// Most parity: past this, losses are better met with a lower bitrate.
pub const FEC_MAX: u32 = 60;

/// Parity per video block, in percent of its data shards, from the client's loss reports.
///
/// Losses come in bursts (Wi-Fi retries, a microwave oven), so the parity covers well over
/// the share seen: it goes up at once to `2.5 × loss + 5 %`, and comes down by one point a
/// report while the link stays cleaner than that.
#[derive(Debug)]
pub struct FecControl {
    percent: u32,
}

impl Default for FecControl {
    fn default() -> Self {
        Self {
            percent: FEC_START,
        }
    }
}

impl FecControl {
    pub fn percent(&self) -> u32 {
        self.percent
    }

    /// Takes one loss report; returns the new parity when it changes.
    pub fn report(&mut self, sample: LossSample) -> Option<u32> {
        // 2.5 × loss in percent, rounded up, in integers (no float rounding at the steps).
        let total = (u64::from(sample.received) + u64::from(sample.lost)).max(1);
        let scaled = (u64::from(sample.lost) * 250).div_ceil(total);
        let needed = (scaled.min(u64::from(FEC_MAX)) as u32 + 5).clamp(FEC_MIN, FEC_MAX);
        let next = if needed > self.percent {
            needed
        } else {
            self.percent.saturating_sub(1).max(needed)
        };
        (next != self.percent).then(|| {
            self.percent = next;
            next
        })
    }
}

/// How fast a frame's packets leave after the first burst, and how many go in that burst,
/// for a bitrate ceiling of `ceiling_bps`.
///
/// A frame paced at four times the ceiling takes a quarter of its share of the link: the
/// average frame still leaves well within a frame interval, and the queue of a Wi-Fi
/// access point or a mobile link sees a steady stream instead of hundreds of packets at
/// once. The burst sent at full speed is what that rate carries in 4 ms (a typical
/// P-frame on a fast link, a few packets on a slow one).
pub fn pacing(ceiling_bps: u32, shard: usize) -> (f64, usize) {
    let rate = (f64::from(ceiling_bps) * 4.0).clamp(20e6, 1e9);
    let burst = (rate * 0.004 / 8.0 / shard as f64) as usize;
    (rate, burst.clamp(8, 64))
}

/// What the client shows about the link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Good,
    /// Noticeable: some frames need recovery, or the round trip adds visible delay.
    Poor,
    /// The picture stalls or lags.
    Bad,
}

/// One second of the client's view of the link.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinkStats {
    /// Share of the UDP packets lost, 0 to 1.
    pub loss: f32,
    /// Frames that could not be rebuilt.
    pub frames_lost: u32,
    /// Round trip of the control channel, if a ping came back.
    pub rtt: Option<Duration>,
}

impl LinkStats {
    pub fn quality(&self) -> Quality {
        let rtt = self.rtt.unwrap_or_default();
        if self.loss >= 0.10 || self.frames_lost >= 10 || rtt >= Duration::from_millis(200) {
            Quality::Bad
        } else if self.loss >= 0.02 || self.frames_lost >= 2 || rtt >= Duration::from_millis(80)
        {
            Quality::Poor
        } else {
            Quality::Good
        }
    }

    /// A short line for a title bar or a banner; `None` when the link is good.
    pub fn warning(&self) -> Option<String> {
        let what = match self.quality() {
            Quality::Good => return None,
            Quality::Poor => "poor connection",
            Quality::Bad => "bad connection",
        };
        let mut parts = Vec::new();
        if self.loss >= 0.005 {
            parts.push(format!("{:.0}% lost", self.loss * 100.0));
        }
        if let Some(rtt) = self.rtt
            && rtt >= Duration::from_millis(40)
        {
            parts.push(format!("{} ms", rtt.as_millis()));
        }
        Some(if parts.is_empty() {
            what.to_owned()
        } else {
            format!("{what}: {}", parts.join(", "))
        })
    }
}

/// Keeps the shown quality from flickering: worse shows at once, better only after
/// `HOLD` seconds in a row.
#[derive(Debug)]
pub struct QualityFilter {
    shown: Quality,
    better_for: u32,
}

impl Default for QualityFilter {
    fn default() -> Self {
        Self {
            shown: Quality::Good,
            better_for: 0,
        }
    }
}

impl QualityFilter {
    const HOLD: u32 = 3;

    /// The quality being shown.
    pub fn shown(&self) -> Quality {
        self.shown
    }

    /// One second's quality; returns the one to show when it changes.
    pub fn update(&mut self, q: Quality) -> Option<Quality> {
        if q > self.shown {
            self.shown = q;
            self.better_for = 0;
            return Some(q);
        }
        if q < self.shown {
            self.better_for += 1;
            if self.better_for >= Self::HOLD {
                self.shown = q;
                self.better_for = 0;
                return Some(q);
            }
        } else {
            self.better_for = 0;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_gaps_as_losses() {
        let mut c = LossCounter::default();
        assert_eq!(c.take(), None);
        for seq in [5, 6, 8, 9, 10] {
            c.packet(seq);
        }
        // 5..=10 expected, 7 missing.
        assert_eq!(
            c.take(),
            Some(LossSample {
                received: 5,
                lost: 1
            })
        );
        // A late packet in the next period does not make losses negative.
        c.packet(7);
        c.packet(11);
        assert_eq!(
            c.take(),
            Some(LossSample {
                received: 2,
                lost: 0
            })
        );
        assert_eq!(c.take(), None);
    }

    #[test]
    fn fec_follows_losses_up_at_once_and_down_slowly() {
        let mut f = FecControl::default();
        let clean = LossSample {
            received: 1000,
            lost: 0,
        };
        let lossy = LossSample {
            received: 900,
            lost: 100,
        };
        // 10 % lost: 2.5 × 10 + 5 = 30 %.
        assert_eq!(f.report(lossy), Some(30));
        assert_eq!(f.report(clean), Some(29));
        for _ in 0..100 {
            f.report(clean);
        }
        assert_eq!(f.percent(), FEC_MIN);
        let terrible = LossSample {
            received: 1,
            lost: 1,
        };
        assert_eq!(f.report(terrible), Some(FEC_MAX));
    }

    #[test]
    fn pacing_scales_with_the_ceiling() {
        let (slow, slow_burst) = pacing(1_000_000, 1100);
        assert_eq!(slow, 20e6);
        assert_eq!(slow_burst, 9);
        let (fast, fast_burst) = pacing(100_000_000, 1100);
        assert_eq!(fast, 400e6);
        assert_eq!(fast_burst, 64);
        assert_eq!(pacing(u32::MAX, 1100).0, 1e9);
    }

    #[test]
    fn quality_and_warning() {
        let good = LinkStats {
            loss: 0.001,
            frames_lost: 0,
            rtt: Some(Duration::from_millis(5)),
        };
        assert_eq!(good.quality(), Quality::Good);
        assert_eq!(good.warning(), None);
        let poor = LinkStats {
            loss: 0.03,
            frames_lost: 1,
            rtt: Some(Duration::from_millis(90)),
        };
        assert_eq!(poor.quality(), Quality::Poor);
        assert_eq!(
            poor.warning().as_deref(),
            Some("poor connection: 3% lost, 90 ms")
        );
        let bad = LinkStats {
            loss: 0.0,
            frames_lost: 12,
            rtt: None,
        };
        assert_eq!(bad.warning().as_deref(), Some("bad connection"));
    }

    #[test]
    fn filter_shows_worse_at_once_and_better_later() {
        let mut f = QualityFilter::default();
        assert_eq!(f.update(Quality::Good), None);
        assert_eq!(f.update(Quality::Bad), Some(Quality::Bad));
        assert_eq!(f.update(Quality::Good), None);
        assert_eq!(f.update(Quality::Good), None);
        assert_eq!(f.update(Quality::Good), Some(Quality::Good));
    }
}

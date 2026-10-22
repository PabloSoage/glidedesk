//! Session audio to RDP clients (`rdpsnd`): the 20 ms PCM blocks of the session's audio
//! source (48 kHz stereo S16), with flow control on the client's Wave Confirms.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{
    NegotiatedFormat, RdpsndError, RdpsndServerHandler, RdpsndServerMessage,
};
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::broadcast;
use tracing::{debug, info};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// Bytes per 20 ms block of 48 kHz stereo S16.
pub const BLOCK: usize = (RATE as usize / 50) * CHANNELS as usize * 2;
const BLOCK_MS: u32 = 20;
/// Above this much audio queued beyond the client's own buffering, blocks are dropped until
/// it is back under `QUEUE_LOW_MS`.
const QUEUE_HIGH_MS: u32 = 160;
const QUEUE_LOW_MS: u32 = 80;

type Events = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ServerEvent>>>>;

pub struct SoundFactory {
    events: Events,
    audio: broadcast::Sender<Vec<u8>>,
}

impl SoundFactory {
    pub fn new(audio: broadcast::Sender<Vec<u8>>) -> Self {
        Self {
            events: Arc::default(),
            audio,
        }
    }
}

impl ServerEventSender for SoundFactory {
    fn set_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<ServerEvent>) {
        *self.events.lock().unwrap() = Some(sender);
    }
}

impl SoundServerFactory for SoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(Sound {
            events: Arc::clone(&self.events),
            audio: self.audio.clone(),
            task: None,
            flow: Arc::default(),
        })
    }
}

#[derive(Debug)]
struct Sound {
    events: Events,
    audio: broadcast::Sender<Vec<u8>>,
    task: Option<tokio::task::JoinHandle<()>>,
    flow: Arc<Mutex<Flow>>,
}

/// How much audio is queued between the capture and the client's speakers, from the Wave
/// Confirms (MS-RDPEA 2.2.3.8). Windows and FreeRDP confirm each wave twice: on receipt with
/// the wave's own timestamp, and once played with the timestamp plus the time they held it.
/// Without this the delay only grows: the capture clock and the client's sound card drift,
/// and audio that waits behind video on a busy link is never caught up.
#[derive(Debug)]
struct Flow {
    /// `wTimeStamp` of the wave sent under each block number (IronRDP numbers waves 0, 1, ...
    /// in the order they are sent, wrapping at 256).
    /// The flag says whether its receipt was counted.
    sent: [Option<(u16, bool)>; 256],
    next_block: u8,
    /// Waves sent and not yet received.
    unreceived: u32,
    /// Time the client held the last played wave before playing it (its buffer).
    held_ms: u32,
    /// The client's own buffering: the least audio queued during the first `LEARN` after
    /// the first confirm. mstsc keeps far more than FreeRDP; counting it as delay would drop
    /// blocks all the time (choppy sound).
    floor_ms: Option<u32>,
    learn_until: Option<std::time::Instant>,
}

/// How long the client's own buffering is measured at the start.
const LEARN: Duration = Duration::from_secs(3);

impl Default for Flow {
    fn default() -> Self {
        Self {
            sent: [None; 256],
            next_block: 0,
            unreceived: 0,
            held_ms: 0,
            floor_ms: None,
            learn_until: None,
        }
    }
}

impl Flow {
    fn sent(&mut self, ts: u16) {
        self.sent[usize::from(self.next_block)] = Some((ts, false));
        self.next_block = self.next_block.wrapping_add(1);
        self.unreceived += 1;
    }

    fn confirm(&mut self, block_no: u8, timestamp: u16) {
        let Some((sent, received)) = &mut self.sent[usize::from(block_no)] else {
            return;
        };
        let held = timestamp.wrapping_sub(*sent);
        if held > 5000 {
            return;
        }
        // A client that only confirms once played (as the specification says) also received it.
        if !std::mem::replace(received, true) {
            self.unreceived = self.unreceived.saturating_sub(1);
        }
        // The receipt confirm echoes the timestamp; the played one adds the time held.
        if held > 0 {
            self.held_ms = u32::from(held);
        }
        let now = std::time::Instant::now();
        let until = *self.learn_until.get_or_insert(now + LEARN);
        if now < until {
            let q = self.queued_ms();
            self.floor_ms = Some(self.floor_ms.map_or(q, |f| f.min(q)));
        }
    }

    /// Audio between the capture and the speakers, in ms.
    fn queued_ms(&self) -> u32 {
        self.unreceived * BLOCK_MS + self.held_ms
    }

    /// Delay beyond the client's own buffering: what drift and congestion added.
    fn excess_ms(&self) -> u32 {
        self.queued_ms().saturating_sub(self.floor_ms.unwrap_or(0))
    }
}

impl std::fmt::Debug for SoundFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SoundFactory")
    }
}

static FORMATS: std::sync::LazyLock<Vec<AudioFormat>> = std::sync::LazyLock::new(|| {
    vec![AudioFormat {
        format: WaveFormat::PCM,
        n_channels: CHANNELS,
        n_samples_per_sec: RATE,
        n_avg_bytes_per_sec: RATE * u32::from(CHANNELS) * 2,
        n_block_align: CHANNELS * 2,
        bits_per_sample: 16,
        data: None,
    }]
});

impl RdpsndServerHandler for Sound {
    fn get_formats(&self) -> &[AudioFormat] {
        &FORMATS
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        debug!(?common, "Audio formats in common");
        common.first()
    }

    fn start(&mut self, _format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let mut rx = self.audio.subscribe();
        let events = Arc::clone(&self.events);
        *self.flow.lock().unwrap() = Flow::default();
        let flow = Arc::clone(&self.flow);
        info!("Audio playback started");
        self.task = Some(tokio::spawn(async move {
            let mut ts: u32 = 0;
            let (mut blocks, mut loud, mut since) = (0u32, 0u32, std::time::Instant::now());
            let (mut draining, mut dropped) = (false, 0u32);
            loop {
                match rx.recv().await {
                    Ok(block) => {
                        // The capture keeps its pace (ts advances) while blocks are dropped,
                        // so the client's clock follows the capture again.
                        ts = ts.wrapping_add(BLOCK_MS);
                        let (queued, floor) = {
                            let f = flow.lock().unwrap();
                            (f.excess_ms(), f.floor_ms)
                        };
                        if queued > QUEUE_HIGH_MS {
                            draining = true;
                        } else if queued <= QUEUE_LOW_MS {
                            draining = false;
                        }
                        if draining {
                            dropped += 1;
                            // Forget the oldest waiting block as if it had been received, so
                            // a client that never confirms cannot stop the audio for good.
                            let mut f = flow.lock().unwrap();
                            f.unreceived = f.unreceived.saturating_sub(1);
                            f.held_ms = f.held_ms.saturating_sub(BLOCK_MS);
                            continue;
                        }
                        blocks += 1;
                        if block
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .any(|s| i16::from_le_bytes(*s).unsigned_abs() > 256)
                        {
                            loud += 1;
                        }
                        if since.elapsed() >= Duration::from_secs(5) {
                            debug!(
                                blocks,
                                with_sound = loud,
                                dropped,
                                queued,
                                ?floor,
                                "Audio blocks sent"
                            );
                            (blocks, loud, dropped, since) = (0, 0, 0, std::time::Instant::now());
                        }
                        if let Some(tx) = events.lock().unwrap().as_ref() {
                            flow.lock().unwrap().sent(ts as u16);
                            let _ =
                                tx.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(block, ts)));
                        }
                    }
                    // Fell behind: skip ahead rather than add delay.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    fn wave_confirm(&mut self, block_no: u8, timestamp: u16) {
        self.flow.lock().unwrap().confirm(block_no, timestamp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_counts_receipts_and_buffer() {
        let mut f = Flow::default();
        for i in 0..5u16 {
            f.sent(i * 20);
        }
        assert_eq!(f.queued_ms(), 100);
        // Windows and FreeRDP: receipt (echo), then played (time held added).
        f.confirm(0, 0);
        f.confirm(1, 20);
        assert_eq!(f.queued_ms(), 60);
        f.confirm(0, 45);
        assert_eq!(f.queued_ms(), 60 + 45);
        // A single confirm once played also counts as received, only once.
        f.confirm(2, 40 + 30);
        f.confirm(2, 40 + 31);
        assert_eq!(f.unreceived, 2);
        assert_eq!(f.held_ms, 31);
        // Garbage is ignored.
        f.confirm(3, 60 + 40000);
        f.confirm(200, 7);
        assert_eq!(f.unreceived, 2);
    }
}

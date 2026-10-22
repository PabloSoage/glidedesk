//! Session audio (PCM S16LE, 48 kHz, stereo) to the default output device.
//!
//! A short queue sits between the network and the device. It keeps about two device
//! periods; above [`MAX_QUEUE_MS`] the oldest samples are dropped, so delay from bursts or
//! clock drift never builds up (the same idea as the server's flow control for RDP).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tracing::{info, warn};

const RATE: u32 = 48_000;
const MAX_QUEUE_MS: usize = 80;

#[derive(Clone)]
pub struct Player {
    queue: Arc<Mutex<VecDeque<i16>>>,
}

impl Player {
    /// Queues interleaved stereo S16LE bytes.
    pub fn push(&self, pcm: &[u8]) {
        let mut q = self.queue.lock().unwrap();
        q.extend(
            pcm.as_chunks::<2>()
                .0
                .iter()
                .map(|s| i16::from_le_bytes(*s)),
        );
        let max = RATE as usize * 2 * MAX_QUEUE_MS / 1000;
        if q.len() > max {
            let extra = q.len() - max;
            q.drain(..extra);
        }
    }
}

/// Opens the default output; the returned stream must be kept alive.
pub fn start() -> Result<(Player, cpal::Stream)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no audio output device")?;
    let supported = device.default_output_config()?;
    let channels = supported.channels() as usize;
    let mut config = supported.config();
    // Ask for the stream's own rate; most devices accept 48 kHz (no resampling here).
    config.sample_rate = RATE;
    let queue: Arc<Mutex<VecDeque<i16>>> = Arc::default();
    let q = Arc::clone(&queue);
    let err = |e| warn!(%e, "Audio output error");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |out: &mut [f32], _| fill(&q, out, channels, |s| f32::from(s) / 32768.0),
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            move |out: &mut [i16], _| fill(&q, out, channels, |s| s),
            err,
            None,
        )?,
        other => anyhow::bail!("audio output format {other:?} is not handled"),
    };
    stream.play()?;
    info!(channels, "Audio output");
    Ok((Player { queue }, stream))
}

/// Fills a device buffer from the stereo queue (silence when it runs dry).
fn fill<T: Copy + Default>(
    q: &Mutex<VecDeque<i16>>,
    out: &mut [T],
    channels: usize,
    conv: impl Fn(i16) -> T,
) {
    let mut q = q.lock().unwrap();
    for frame in out.chunks_mut(channels) {
        let (l, r) = match (q.pop_front(), q.pop_front()) {
            (Some(l), Some(r)) => (l, r),
            _ => (0, 0),
        };
        for (i, s) in frame.iter_mut().enumerate() {
            *s = conv(if i % 2 == 0 { l } else { r });
        }
    }
}

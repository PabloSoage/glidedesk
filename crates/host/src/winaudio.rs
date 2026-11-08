//! Session audio on Windows: WASAPI loopback of the default output, converted by Windows to
//! 48 kHz stereo s16le and broadcast in 20 ms blocks like the other captures.
//!
//! Loopback delivers nothing while nothing plays; silence is filled in so that clients see a
//! steady stream, as from a PulseAudio monitor.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::broadcast;
use tracing::{info, warn};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient,
    IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eConsole,
    eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};

use crate::sound::{BLOCK, CHANNELS, RATE};

/// Behind the wall clock by this much with no audio: send silence.
const GAP: Duration = Duration::from_millis(100);

struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

fn open() -> Result<Loopback> {
    // SAFETY: COM calls on this thread, which stays in the multithreaded apartment.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = devices
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("no default audio output")?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * CHANNELS as u32 * 2,
            nBlockAlign: CHANNELS * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // 100 ms of buffer (in 100 ns units).
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                1_000_000,
                0,
                &format,
                None,
            )
            .context("starting the loopback capture")?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Loopback { client, capture })
    }
}

impl Loopback {
    /// Appends what the device has captured since the last call.
    fn read(&self, out: &mut Vec<u8>) -> windows::core::Result<()> {
        // SAFETY: each buffer is copied out before it is released.
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data = std::ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let len = frames as usize * CHANNELS as usize * 2;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    out.resize(out.len() + len, 0);
                } else {
                    out.extend_from_slice(std::slice::from_raw_parts(data, len));
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stopping a started client.
        let _ = unsafe { self.client.Stop() };
    }
}

pub fn start() -> Result<broadcast::Sender<Vec<u8>>> {
    let (tx, _) = broadcast::channel(32);
    let sender = tx.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("wasapi".into())
        .spawn(move || {
            let lb = match open() {
                Ok(lb) => {
                    let _ = ready_tx.send(Ok(()));
                    lb
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let block_time = Duration::from_millis(20);
            let mut pending = Vec::new();
            let mut clock = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(10));
                if let Err(e) = lb.read(&mut pending) {
                    warn!(%e, "WASAPI loopback stopped");
                    return;
                }
                while pending.len() >= BLOCK {
                    let block: Vec<u8> = pending.drain(..BLOCK).collect();
                    let _ = sender.send(block);
                    clock += block_time;
                }
                let now = Instant::now();
                if now > clock + GAP {
                    // Nothing is playing: silence up to now.
                    while clock + block_time <= now {
                        let _ = sender.send(vec![0; BLOCK]);
                        clock += block_time;
                    }
                } else if clock > now + GAP {
                    // Real audio ran ahead of the wall clock: follow it.
                    clock = now;
                }
            }
        })?;
    ready_rx.recv().context("the audio thread ended")??;
    info!("Audio from the WASAPI loopback of the default output");
    Ok(tx)
}

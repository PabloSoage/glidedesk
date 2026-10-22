//! Session audio from PulseAudio (or PipeWire's Pulse server): the default output's monitor
//! read by `parec`, broadcast in 20 ms blocks like the PipeWire capture. Used with X11
//! sessions (Termux runs PulseAudio).

use std::io::Read;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::sound::BLOCK;

pub fn start() -> Result<broadcast::Sender<Vec<u8>>> {
    let mut child = Command::new("parec")
        .args([
            "--raw",
            "--rate=48000",
            "--channels=2",
            "--format=s16le",
            "--latency-msec=20",
            "--device=@DEFAULT_MONITOR@",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting parec (PulseAudio)")?;
    let mut out = child.stdout.take().context("parec has no output")?;
    let (tx, _) = broadcast::channel(32);
    let sender = tx.clone();
    std::thread::Builder::new()
        .name("parec".into())
        .spawn(move || {
            let mut block = vec![0u8; BLOCK];
            while out.read_exact(&mut block).is_ok() {
                // No receiver: no client is playing; the block is dropped.
                let _ = sender.send(block.clone());
            }
            warn!("parec stopped");
            let _ = child.kill();
        })?;
    info!("Audio from the PulseAudio monitor");
    Ok(tx)
}

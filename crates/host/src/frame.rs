//! Frames and pointer shapes as the session backends produce them, and the one-frame
//! mailbox that hands frames to the pipeline.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::gpu::DeviceBuffer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Pixels of a frame, BGRx (B, G, R, X bytes per pixel).
pub enum FrameData {
    /// System memory (Mutter read the frame back from the GPU).
    Cpu { data: Vec<u8>, stride: u32 },
    /// Already on the encoder's GPU (DMA-BUF path, no CPU copy).
    Gpu(Arc<DeviceBuffer>),
}

/// One captured frame.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: FrameData,
    /// Regions that changed since the previous frame; `None` when Mutter did not say
    /// (treat as the whole frame).
    pub damage: Option<Vec<Rect>>,
    /// When the frame was dequeued from PipeWire.
    pub captured: Instant,
}

/// A cursor image, BGRA with rows bottom-up (the RDP pointer layout), straight alpha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub bgra_bottom_up: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorEvent {
    Shape(CursorShape),
    Hidden,
}

/// One-frame mailbox between the capture and the encoder: a new frame replaces the one
/// waiting, so the encoder always gets the newest image and never a backlog. (A bounded
/// channel with `try_send` drops the *new* frame instead, and when motion stops the final
/// image can be the one lost.)
pub struct LatestFrame {
    slot: std::sync::Mutex<(Option<Frame>, bool)>,
    ready: std::sync::Condvar,
}

pub fn latest_frame() -> (FrameSender, FrameReceiver) {
    let shared = Arc::new(LatestFrame {
        slot: std::sync::Mutex::new((None, false)),
        ready: std::sync::Condvar::new(),
    });
    (FrameSender(Arc::clone(&shared)), FrameReceiver(shared))
}

#[derive(Clone)]
pub struct FrameSender(Arc<LatestFrame>);

impl FrameSender {
    /// Hands over `frame`; returns whether it replaced one the encoder had not taken.
    pub fn send(&self, frame: Frame) -> bool {
        let mut slot = self.0.slot.lock().unwrap();
        let replaced = slot.0.replace(frame).is_some();
        self.0.ready.notify_one();
        replaced
    }
}

impl Drop for FrameSender {
    fn drop(&mut self) {
        self.0.slot.lock().unwrap().1 = true;
        self.0.ready.notify_one();
    }
}

pub struct FrameReceiver(Arc<LatestFrame>);

impl FrameReceiver {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Frame, mpsc::RecvTimeoutError> {
        let slot = self.0.slot.lock().unwrap();
        let (mut slot, _) = self
            .0
            .ready
            .wait_timeout_while(slot, timeout, |(f, closed)| f.is_none() && !*closed)
            .unwrap();
        match slot.0.take() {
            Some(f) => Ok(f),
            None if slot.1 => Err(mpsc::RecvTimeoutError::Disconnected),
            None => Err(mpsc::RecvTimeoutError::Timeout),
        }
    }
}

/// What the pipeline can ask of the session's video source.
pub trait Source: Send {
    /// Asks for a new desktop size; sources with a fixed size ignore it (frames carry
    /// their real size).
    fn resize(&self, width: u32, height: u32);

    /// Sets the refresh rate used from the next negotiation on; true when it changed.
    /// Sources that cannot change it ignore it.
    fn set_refresh(&self, _hz: u32) -> bool {
        false
    }
}

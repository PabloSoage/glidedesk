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
    /// Where the system-memory buffer goes back when the frame is dropped.
    pub recycle: Option<Recycler>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        if let (Some(r), FrameData::Cpu { data, .. }) = (&self.recycle, &mut self.data) {
            r.put(std::mem::take(data));
        }
    }
}

/// Spare system-memory frame buffers. A 4K frame is 33 MB: allocated afresh each time,
/// its page faults cost more than the copy into it (Windows, 18-24 ms against ~5).
#[derive(Clone, Default)]
pub struct Recycler(Arc<std::sync::Mutex<Vec<Vec<u8>>>>);

/// Buffers kept: the one being filled, the one waiting and the one being encoded.
const SPARE_BUFFERS: usize = 3;

impl Recycler {
    /// An empty buffer that can hold `len` bytes without allocating.
    // Only the Windows capture copies into its own buffers.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn take(&self, len: usize) -> Vec<u8> {
        let mut spare = self.0.lock().unwrap();
        // Buffers of another size (the desktop changed) are dropped.
        spare.retain(|b| b.capacity() >= len && b.capacity() <= len + len / 4);
        let mut buf = spare.pop().unwrap_or_else(|| Vec::with_capacity(len));
        buf.clear();
        buf
    }

    fn put(&self, buf: Vec<u8>) {
        let mut spare = self.0.lock().unwrap();
        if spare.len() < SPARE_BUFFERS {
            spare.push(buf);
        }
    }
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
    /// Live senders: the mailbox closes when the last one goes (a capture that replaces
    /// another holds a copy of the same sender).
    senders: std::sync::atomic::AtomicUsize,
}

pub fn latest_frame() -> (FrameSender, FrameReceiver) {
    let shared = Arc::new(LatestFrame {
        slot: std::sync::Mutex::new((None, false)),
        ready: std::sync::Condvar::new(),
        senders: std::sync::atomic::AtomicUsize::new(1),
    });
    (FrameSender(Arc::clone(&shared)), FrameReceiver(shared))
}

pub struct FrameSender(Arc<LatestFrame>);

impl Clone for FrameSender {
    fn clone(&self) -> Self {
        self.0
            .senders
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(Arc::clone(&self.0))
    }
}

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
        if self
            .0
            .senders
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            self.0.slot.lock().unwrap().1 = true;
            self.0.ready.notify_one();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Frame {
        Frame {
            width: 2,
            height: 2,
            data: FrameData::Cpu {
                data: vec![0; 16],
                stride: 8,
            },
            damage: None,
            captured: Instant::now(),
            recycle: None,
        }
    }

    #[test]
    fn dropped_frames_give_their_buffer_back() {
        let r = Recycler::default();
        let mut buf = r.take(1000);
        buf.resize(1000, 7);
        let ptr = buf.as_ptr();
        drop(Frame {
            width: 25,
            height: 10,
            data: FrameData::Cpu {
                data: buf,
                stride: 100,
            },
            damage: None,
            captured: Instant::now(),
            recycle: Some(r.clone()),
        });
        let again = r.take(1000);
        assert_eq!(again.as_ptr(), ptr);
        assert!(again.is_empty());
        // Another size gets a new buffer.
        let other = r.take(4000);
        assert_ne!(other.as_ptr(), ptr);
    }

    #[test]
    fn mailbox_stays_open_while_a_copy_of_the_sender_lives() {
        let (tx, rx) = latest_frame();
        let new_capture = tx.clone();
        // The old capture goes away after the new one started (GNOME session restart).
        drop(tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(1)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        new_capture.send(frame());
        assert!(rx.recv_timeout(Duration::from_millis(1)).is_ok());
        drop(new_capture);
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}

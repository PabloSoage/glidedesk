//! PipeWire capture of a Mutter virtual monitor.
//!
//! The PipeWire main loop runs on its own thread. Frames go out through a bounded channel
//! that keeps only the newest frame: if the encoder is behind, older frames are dropped
//! instead of queueing latency. Resizing renegotiates the stream format; Mutter then
//! resizes the virtual monitor to match.

use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use tracing::{debug, info, warn};

/// One captured frame in system memory, BGRx (B, G, R, X bytes per pixel).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
    /// When the frame was dequeued from PipeWire.
    pub captured: Instant,
    /// Frame counter since the stream started.
    pub seq: u64,
}

/// Commands to the capture thread.
enum Command {
    Resize(u32, u32),
    Stop,
}

pub struct Capture {
    commands: pw::channel::Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Capture {
    /// Connects to `node_id` asking for `width`×`height`. Frames are sent to `frames`; a
    /// `sync_channel(1)` with `try_send` gives "latest frame wins".
    pub fn start(
        node_id: u32,
        width: u32,
        height: u32,
        frames: mpsc::SyncSender<Frame>,
    ) -> Result<Self> {
        let (tx, rx) = pw::channel::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let thread = thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || {
                if let Err(e) = run(node_id, width, height, frames, rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("capture thread exited"))??;
        Ok(Self {
            commands: tx,
            thread: Some(thread),
        })
    }

    pub fn resize(&self, width: u32, height: u32) {
        let _ = self.commands.send(Command::Resize(width, height));
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct StreamState {
    format: spa::param::video::VideoInfoRaw,
    frames: mpsc::SyncSender<Frame>,
    seq: u64,
    dropped: u64,
}

fn format_pod(width: u32, height: u32) -> Vec<u8> {
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle { width, height }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 240, denom: 1 },
            spa::utils::Fraction { num: 1, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serializing a fixed pod")
    .0
    .into_inner()
}

fn run(
    node_id: u32,
    width: u32,
    height: u32,
    frames: mpsc::SyncSender<Frame>,
    commands: pw::channel::Receiver<Command>,
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamRc::new(
        core,
        "glidedesk-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let state = StreamState {
        format: Default::default(),
        frames,
        seq: 0,
        dropped: 0,
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, old, new| debug!(?old, ?new, "PipeWire stream state"))
        .param_changed(|_, st, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if st.format.parse(param).is_ok() {
                let s = st.format.size();
                info!(width = s.width, height = s.height, format = ?st.format.format(), "Negotiated capture format");
            }
        })
        .process(|stream, st| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let size = st.format.size();
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let chunk = d.chunk();
            let (offset, stride, len) = (chunk.offset() as usize, chunk.stride(), chunk.size() as usize);
            if len == 0 || stride <= 0 {
                // Mutter sends empty buffers when only the cursor moved.
                return;
            }
            let Some(bytes) = d.data() else { return };
            let Some(src) = bytes.get(offset..offset + len) else { return };
            st.seq += 1;
            let frame = Frame {
                width: size.width,
                height: size.height,
                stride: stride as u32,
                data: src.to_vec(),
                captured: Instant::now(),
                seq: st.seq,
            };
            if st.frames.try_send(frame).is_err() {
                st.dropped += 1;
                if st.dropped.is_power_of_two() {
                    debug!(dropped = st.dropped, "Encoder behind, dropped frames");
                }
            }
        })
        .register()?;

    let pod = format_pod(width, height);
    let mut params = [Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad format pod"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    let _ = ready.send(Ok(()));

    let ml = mainloop.clone();
    let st = stream.clone();
    let _cmd = commands.attach(mainloop.loop_(), move |cmd| match cmd {
        Command::Resize(w, h) => {
            info!(w, h, "Renegotiating capture size");
            let pod = format_pod(w, h);
            match Pod::from_bytes(&pod) {
                Some(p) => {
                    if let Err(e) = st.update_params(&mut [p]) {
                        warn!(%e, "update_params failed");
                    }
                }
                None => warn!("bad format pod"),
            }
        }
        Command::Stop => ml.quit(),
    });
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

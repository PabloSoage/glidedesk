//! PipeWire capture of a Mutter virtual monitor.
//!
//! The PipeWire main loop runs on its own thread. Frames go out through a one-frame mailbox
//! that keeps only the newest frame: if the encoder is behind, older frames are dropped
//! instead of queueing latency. Resizing renegotiates the stream format; Mutter then
//! resizes the virtual monitor to match.
//!
//! The cursor is not drawn into the frames: Mutter sends it as metadata (position, and the
//! image when it changes), which goes out on its own channel so the client can draw it
//! locally, as Windows RDP does.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use cudarc::driver::CudaContext;
use pipewire as pw;
use pw::spa;
use pw::spa::buffer::meta::{MetaCursor, MetaVideoDamage};
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
use tracing::{debug, info, trace, warn};

pub use crate::frame::{
    CursorEvent, CursorShape, Frame, FrameData, FrameSender, Rect, latest_frame,
};
use crate::gpu::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, DmabufImporter, PixelOrder, Pool};

/// Largest cursor image requested from Mutter (the RDP large pointer limit).
const MAX_CURSOR: usize = 384;
/// How long Mutter must send only empty pointer sprites before the pointer is hidden.
const HIDE_AFTER: Duration = Duration::from_millis(150);
/// Damage rectangles requested per frame; Mutter merges beyond this.
const MAX_DAMAGE: usize = 32;

/// Commands to the capture thread.
enum Command {
    Resize(u32, u32),
    Stop,
}

pub struct Capture {
    commands: pw::channel::Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct Sinks {
    pub frames: FrameSender,
    pub cursor: Arc<tokio::sync::watch::Sender<Option<CursorEvent>>>,
}

impl Capture {
    /// Connects to `node_id` asking for `width`×`height`. Frames go to `frames` (see
    /// [`latest_frame`]). With `gpu`, DMA-BUFs are negotiated and frames arrive as
    /// [`FrameData::Gpu`] on that CUDA context.
    pub fn start(
        node_id: u32,
        width: u32,
        height: u32,
        sinks: Sinks,
        gpu: Option<Arc<CudaContext>>,
    ) -> Result<Self> {
        let (tx, rx) = pw::channel::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let thread = thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || {
                if let Err(e) = run(node_id, width, height, sinks, gpu, rx, &ready_tx) {
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

impl crate::frame::Source for Capture {
    fn resize(&self, width: u32, height: u32) {
        Capture::resize(self, width, height);
    }

    fn set_refresh(&self, hz: u32) -> bool {
        let hz = hz.clamp(30, 240);
        if refresh_rate() == hz {
            return false;
        }
        set_refresh_rate(hz);
        true
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
    sinks: Sinks,
    dropped: u64,
    /// Since when Mutter has sent only empty pointer sprites.
    hidden_since: Option<Instant>,
    /// DMA-BUF import, when a GPU was given and the modifiers could be queried.
    gpu: Option<(DmabufImporter, Pool)>,
    /// The negotiated format carries a modifier: buffers are DMA-BUFs.
    dmabuf: bool,
    copy_errors: u64,
    /// Per-interval statistics: buffers with pixels, time spent copying them.
    stats_frames: u32,
    stats_dropped: u32,
    stats_copy_us: u128,
    stats_at: Instant,
}

fn serialize(obj: Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .expect("serializing a fixed pod")
    .0
    .into_inner()
}

/// The formats offered: with DMA-BUF modifiers first (preferred), then system memory.
fn format_pods(width: u32, height: u32, modifiers: &[u64]) -> Vec<Vec<u8>> {
    let mut pods = Vec::new();
    if let Some(&first) = modifiers.first() {
        let mut obj = format_object(width, height);
        obj.properties.push(Property {
            key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY
                | PropertyFlags::from_bits_retain(spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE),
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: first as i64,
                    alternatives: modifiers.iter().map(|&m| m as i64).collect(),
                },
            ))),
        });
        pods.push(serialize(obj));
    }
    pods.push(serialize(format_object(width, height)));
    pods
}

fn format_object(width: u32, height: u32) -> Object {
    spa::pod::object!(
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
        // A fixed value, not a range: Mutter offers 1..1000 with 60 as default, and a range
        // here would settle on its default. Mutter creates the virtual monitor at the
        // negotiated maximum frame rate, so this is the monitor's refresh rate.
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoMaxFramerate,
            Fraction,
            spa::utils::Fraction {
                num: refresh_rate(),
                denom: 1
            }
        ),
    )
}

/// Refresh rate of the virtual monitor: the default at start ([`set_refresh_rate`]), then
/// the native client's screen.
static REFRESH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(60);

pub fn set_refresh_rate(hz: u32) {
    REFRESH.store(hz.clamp(1, 1000), std::sync::atomic::Ordering::Relaxed);
}

fn refresh_rate() -> u32 {
    REFRESH.load(std::sync::atomic::Ordering::Relaxed)
}

/// `SPA_PARAM_Buffers` acknowledging the buffer type for the negotiated format.
fn buffers_pod(dmabuf: bool) -> Vec<u8> {
    let data_type = if dmabuf {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        1 << spa::sys::SPA_DATA_MemPtr
    };
    serialize(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![Property {
            key: spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Int(data_type),
        }],
    })
}

/// `SPA_PARAM_Meta` asking for one metadata type of `size` bytes.
fn meta_pod(meta_type: u32, size: usize) -> Vec<u8> {
    serialize(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_META_type,
                flags: PropertyFlags::empty(),
                value: Value::Id(spa::utils::Id(meta_type)),
            },
            Property {
                key: spa::sys::SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(size as i32),
            },
        ],
    })
}

fn meta_pods() -> Vec<Vec<u8>> {
    let cursor = std::mem::size_of::<spa::sys::spa_meta_cursor>()
        + std::mem::size_of::<spa::sys::spa_meta_bitmap>()
        + MAX_CURSOR * MAX_CURSOR * 4;
    vec![
        meta_pod(spa::sys::SPA_META_Cursor, cursor),
        meta_pod(
            spa::sys::SPA_META_VideoDamage,
            std::mem::size_of::<spa::sys::spa_meta_region>() * MAX_DAMAGE,
        ),
        meta_pod(
            spa::sys::SPA_META_Header,
            std::mem::size_of::<spa::sys::spa_meta_header>(),
        ),
    ]
}

/// Converts Mutter's cursor bitmap to the RDP layout. Mutter sends RGBA (or BGRA);
/// rows are top-down.
fn cursor_shape(cursor: &MetaCursor) -> Option<CursorEvent> {
    let bitmap = cursor.bitmap()?;
    let size = bitmap.size();
    if size.width == 0 || size.height == 0 {
        return Some(CursorEvent::Hidden);
    }
    let (w, h) = (size.width as usize, size.height as usize);
    if w > MAX_CURSOR || h > MAX_CURSOR {
        return None;
    }
    let data = bitmap.bitmap_data()?;
    let stride = bitmap.stride() as usize;
    let swap = match bitmap.format() {
        spa::param::video::VideoFormat::RGBA => true,
        spa::param::video::VideoFormat::BGRA => false,
        other => {
            debug!(?other, "Unsupported cursor format");
            return None;
        }
    };
    let mut out = Vec::with_capacity(w * h * 4);
    for row in (0..h).rev() {
        let line = data.get(row * stride..row * stride + w * 4)?;
        for px in line.as_chunks::<4>().0 {
            if swap {
                out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            } else {
                out.extend_from_slice(px);
            }
        }
    }
    let hot = cursor.hotspot();
    Some(CursorEvent::Shape(CursorShape {
        width: w as u16,
        height: h as u16,
        hot_x: hot.x.max(0) as u16,
        hot_y: hot.y.max(0) as u16,
        bgra_bottom_up: out,
    }))
}

fn run(
    node_id: u32,
    width: u32,
    height: u32,
    sinks: Sinks,
    gpu: Option<Arc<CudaContext>>,
    commands: pw::channel::Receiver<Command>,
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    // GLIDEDESK_NO_DMABUF=1 forces the system-memory path (for comparisons).
    let gpu = gpu.filter(|_| std::env::var_os("GLIDEDESK_NO_DMABUF").is_none());
    let gpu = match gpu
        .map(|ctx| DmabufImporter::new(Arc::clone(&ctx)).map(|imp| (imp, Pool::new(ctx))))
    {
        Some(Ok(g)) => Some(g),
        Some(Err(e)) => {
            warn!(%e, "DMA-BUF import unavailable, capturing through system memory");
            None
        }
        None => None,
    };
    let modifiers = gpu
        .as_ref()
        .map(|(imp, _)| {
            let mut m = imp.modifiers(DRM_FORMAT_XRGB8888);
            m.retain(|x| imp.modifiers(DRM_FORMAT_ARGB8888).contains(x));
            m
        })
        .unwrap_or_default();
    info!(count = modifiers.len(), "DMA-BUF modifiers offered");
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
        sinks,
        dropped: 0,
        hidden_since: None,
        gpu,
        dmabuf: false,
        copy_errors: 0,
        stats_frames: 0,
        stats_dropped: 0,
        stats_copy_us: 0,
        stats_at: Instant::now(),
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, old, new| debug!(?old, ?new, "PipeWire stream state"))
        .param_changed(|stream, st, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if st.format.parse(param).is_ok() {
                let s = st.format.size();
                // A modifier in the fixed format means Mutter will send DMA-BUFs.
                let has_modifier = !param.as_bytes().is_empty()
                    && spa::pod::deserialize::PodDeserializer::deserialize_from::<Value>(
                        param.as_bytes(),
                    )
                    .ok()
                    .and_then(|(_, v)| match v {
                        Value::Object(o) => Some(o),
                        _ => None,
                    })
                    .is_some_and(|o| {
                        o.properties.iter().any(|p| {
                            p.key == spa::param::format::FormatProperties::VideoModifier.as_raw()
                        })
                    });
                st.dmabuf = has_modifier && st.gpu.is_some();
                if let Some((imp, _)) = st.gpu.as_mut() {
                    imp.clear();
                }
                info!(
                    width = s.width,
                    height = s.height,
                    format = ?st.format.format(),
                    dmabuf = st.dmabuf,
                    max_fps = format!("{}/{}", st.format.max_framerate().num, st.format.max_framerate().denom),
                    modifier = format!("0x{:x}", st.format.modifier()),
                    "Negotiated capture format"
                );
                let mut metas = meta_pods();
                metas.push(buffers_pod(st.dmabuf));
                let mut pods: Vec<&Pod> = metas.iter().filter_map(|m| Pod::from_bytes(m)).collect();
                if let Err(e) = stream.update_params(&mut pods) {
                    warn!(%e, "Requesting cursor and damage metadata failed");
                }
            }
        })
        .process(|stream, st| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };

            // Mutter gives every sprite the same id and attaches a bitmap when the image
            // may have changed, so bitmaps are compared by content. It also sends an empty
            // sprite now and then for a single frame while the pointer moves: the pointer
            // is hidden only when empty sprites keep coming for `HIDE_AFTER`.
            if let Some(cursor) = buffer.find_meta::<MetaCursor>()
                && cursor.is_valid()
                && let Some(ev) = cursor_shape(cursor)
            {
                trace!(hidden = ev == CursorEvent::Hidden, "Pointer sprite");
                let show = if ev == CursorEvent::Hidden {
                    let since = *st.hidden_since.get_or_insert_with(Instant::now);
                    since.elapsed() >= HIDE_AFTER
                } else {
                    st.hidden_since = None;
                    true
                };
                if show {
                    st.sinks.cursor.send_if_modified(|cur| {
                        if cur.as_ref() == Some(&ev) {
                            return false;
                        }
                        debug!(hidden = ev == CursorEvent::Hidden, "Pointer image");
                        *cur = Some(ev);
                        true
                    });
                }
            }
            let damage = buffer.find_meta::<MetaVideoDamage>().map(|d| {
                d.iter()
                    .filter(|r| r.is_valid())
                    .map(|r| {
                        let (p, s) = (r.position(), r.size());
                        Rect {
                            x: p.x.max(0) as u32,
                            y: p.y.max(0) as u32,
                            w: s.width,
                            h: s.height,
                        }
                    })
                    .collect::<Vec<_>>()
            });

            let size = st.format.size();
            let fourcc = if st.format.format() == spa::param::video::VideoFormat::BGRA {
                DRM_FORMAT_ARGB8888
            } else {
                DRM_FORMAT_XRGB8888
            };
            let modifier = st.format.modifier();
            let dmabuf = st.dmabuf;
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let chunk = d.chunk();
            let (offset, stride, len) = (chunk.offset(), chunk.stride(), chunk.size() as usize);
            if len == 0 || stride <= 0 {
                // Mutter sends empty buffers when only the cursor moved.
                return;
            }
            let copy_start = Instant::now();
            let data = if dmabuf && d.type_() == spa::buffer::DataType::DmaBuf {
                let fd = d.as_raw().fd as i32;
                let Some((imp, pool)) = st.gpu.as_mut() else {
                    return;
                };
                let dst = match pool.get(size.width, size.height, PixelOrder::Rgba) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        st.dropped += 1;
                        return;
                    }
                    Err(e) => {
                        warn!(%e, "Allocating a GPU frame failed");
                        return;
                    }
                };
                if let Err(e) = imp.copy(
                    fd,
                    offset,
                    stride as u32,
                    fourcc,
                    modifier,
                    size.width,
                    size.height,
                    &dst,
                ) {
                    st.copy_errors += 1;
                    if st.copy_errors.is_power_of_two() {
                        warn!(%e, errors = st.copy_errors, "DMA-BUF copy failed");
                    }
                    return;
                }
                FrameData::Gpu(dst)
            } else {
                let Some(bytes) = d.data() else { return };
                let Some(src) = bytes.get(offset as usize..offset as usize + len) else {
                    return;
                };
                FrameData::Cpu {
                    data: src.to_vec(),
                    stride: stride as u32,
                }
            };
            let frame = Frame {
                width: size.width,
                height: size.height,
                data,
                damage: damage.filter(|d| !d.is_empty()),
                captured: Instant::now(),
            };
            st.stats_frames += 1;
            st.stats_copy_us += copy_start.elapsed().as_micros();
            if st.stats_at.elapsed().as_secs() >= 5 {
                let secs = st.stats_at.elapsed().as_secs_f64();
                info!(
                    fps = format!("{:.1}", f64::from(st.stats_frames) / secs),
                    dropped = st.stats_dropped,
                    copy_ms = format!(
                        "{:.2}",
                        st.stats_copy_us as f64 / f64::from(st.stats_frames.max(1)) / 1000.0
                    ),
                    "Capture"
                );
                st.stats_frames = 0;
                st.stats_dropped = 0;
                st.stats_copy_us = 0;
                st.stats_at = Instant::now();
            }
            if st.sinks.frames.send(frame) {
                st.stats_dropped += 1;
                st.dropped += 1;
                if st.dropped.is_power_of_two() {
                    debug!(dropped = st.dropped, "Encoder behind, replaced frames");
                }
            }
        })
        .register()?;

    let pods = format_pods(width, height, &modifiers);
    let mut params: Vec<&Pod> = pods.iter().filter_map(|p| Pod::from_bytes(p)).collect();
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
            let pods = format_pods(w, h, &modifiers);
            let mut params: Vec<&Pod> = pods.iter().filter_map(|p| Pod::from_bytes(p)).collect();
            if let Err(e) = st.update_params(&mut params) {
                warn!(%e, "update_params failed");
            }
        }
        Command::Stop => ml.quit(),
    });
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

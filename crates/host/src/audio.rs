//! Session audio to the RDP client (`rdpsnd`).
//!
//! The host creates a virtual sink in PipeWire (`support.null-audio-sink`) and makes it the
//! default output, so applications in the session play into it; its monitor is captured as
//! 48 kHz stereo S16 and sent to the client as PCM in 20 ms blocks. The sink disappears with
//! the host.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{
    NegotiatedFormat, RdpsndError, RdpsndServerHandler, RdpsndServerMessage,
};
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// Bytes per 20 ms block of 48 kHz stereo S16.
const BLOCK: usize = (RATE as usize / 50) * CHANNELS as usize * 2;
const SINK_NAME: &str = "glidedesk-audio";

/// Starts the PipeWire side; audio blocks are broadcast to whichever connection is playing.
pub fn start() -> Result<broadcast::Sender<Vec<u8>>> {
    let (tx, _) = broadcast::channel(32);
    let sender = tx.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    thread::Builder::new().name("audio".into()).spawn(move || {
        if let Err(e) = run(sender, &ready_tx) {
            let _ = ready_tx.send(Err(e));
        }
    })?;
    ready_rx
        .recv()
        .map_err(|_| anyhow!("audio thread exited"))??;
    Ok(tx)
}

fn run(out: broadcast::Sender<Vec<u8>>, ready: &std::sync::mpsc::Sender<Result<()>>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let _sink: pw::node::Node = core.create_object(
        "adapter",
        &pw::properties::properties! {
            *pw::keys::FACTORY_NAME => "support.null-audio-sink",
            *pw::keys::NODE_NAME => SINK_NAME,
            *pw::keys::NODE_DESCRIPTION => "Remote desktop (glidedesk)",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            "audio.position" => "FL,FR",
            "audio.rate" => "48000",
            "object.linger" => "false",
        },
    )?;

    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "glidedesk-audio-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::STREAM_CAPTURE_SINK => "true",
            "target.object" => SINK_NAME,
        },
    )?;
    let mut pending: Vec<u8> = Vec::with_capacity(BLOCK * 2);
    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed(|_, _, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if info.parse(param).is_ok() {
                info!(format = ?info.format(), rate = info.rate(), channels = info.channels(), "Audio capture format");
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(d) = buffer.datas_mut().first_mut() else {
                return;
            };
            let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
            let Some(bytes) = d.data() else { return };
            let Some(samples) = bytes.get(offset..offset + size) else {
                return;
            };
            pending.extend_from_slice(samples);
            while pending.len() >= BLOCK {
                let block: Vec<u8> = pending.drain(..BLOCK).collect();
                // No receiver: no client is playing; the block is dropped.
                let _ = out.send(block);
            }
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(u32::from(CHANNELS));
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let pod = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serializing a fixed pod")
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad audio format pod"))?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    // Make the sink the default output so applications play into it. A headless server has
    // no other output worth keeping (a dummy one at most).
    thread::spawn(|| {
        thread::sleep(Duration::from_millis(500));
        let value = format!("{{ \"name\": \"{SINK_NAME}\" }}");
        match std::process::Command::new("pw-metadata")
            .args([
                "0",
                "default.configured.audio.sink",
                &value,
                "Spa:String:JSON",
            ])
            .output()
        {
            Ok(o) if o.status.success() => {
                info!(sink = SINK_NAME, "Audio sink is the default output")
            }
            Ok(o) => warn!(stderr = %String::from_utf8_lossy(&o.stderr), "pw-metadata failed"),
            Err(e) => {
                warn!(%e, "pw-metadata not available; applications may not play into the session sink")
            }
        }
    });

    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

// ---------------------------------------------------------------------------------------
// rdpsnd

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
        })
    }
}

#[derive(Debug)]
struct Sound {
    events: Events,
    audio: broadcast::Sender<Vec<u8>>,
    task: Option<tokio::task::JoinHandle<()>>,
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
        info!("Audio playback started");
        self.task = Some(tokio::spawn(async move {
            let mut ts: u32 = 0;
            let (mut blocks, mut loud, mut since) = (0u32, 0u32, std::time::Instant::now());
            loop {
                match rx.recv().await {
                    Ok(block) => {
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
                            debug!(blocks, with_sound = loud, "Audio blocks sent");
                            (blocks, loud, since) = (0, 0, std::time::Instant::now());
                        }
                        if let Some(tx) = events.lock().unwrap().as_ref() {
                            let _ =
                                tx.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(block, ts)));
                        }
                        ts = ts.wrapping_add(20);
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
}

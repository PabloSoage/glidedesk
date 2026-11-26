//! Session audio from PipeWire (GNOME sessions).
//!
//! The host creates a virtual 7.1 sink in PipeWire (`support.null-audio-sink`) and makes it the
//! default output, so applications in the session play into it; its monitor is captured as
//! 48 kHz 7.1 S16 in 20 ms blocks. Those go as they are to front ends that take surround sound
//! (Moonlight) and mixed down to stereo to the others ([`crate::sound`] for RDP). PipeWire
//! does not spread stereo over the other channels (checked on the server, 26/11), so stereo
//! applications reach stereo clients unchanged. The sink disappears with the host.

use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::sound::{BLOCK, CHANNELS, RATE, SURROUND_CHANNELS};

const SINK_NAME: &str = "glidedesk-audio";
/// Bytes per 20 ms block of 7.1.
const SURROUND_BLOCK: usize = BLOCK / CHANNELS as usize * SURROUND_CHANNELS;

type Blocks = broadcast::Sender<Vec<u8>>;

/// Starts the PipeWire side; audio blocks are broadcast to whichever connection is playing:
/// stereo, and 7.1.
pub fn start() -> Result<(Blocks, Blocks)> {
    let (tx, _) = broadcast::channel(32);
    let (surround_tx, _) = broadcast::channel(32);
    let (sender, surround) = (tx.clone(), surround_tx.clone());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    thread::Builder::new().name("audio".into()).spawn(move || {
        if let Err(e) = run(sender, surround, &ready_tx) {
            let _ = ready_tx.send(Err(e));
        }
    })?;
    ready_rx
        .recv()
        .map_err(|_| anyhow!("audio thread exited"))??;
    Ok((tx, surround_tx))
}

fn run(
    out: broadcast::Sender<Vec<u8>>,
    surround: broadcast::Sender<Vec<u8>>,
    ready: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
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
            "audio.position" => "FL,FR,FC,LFE,RL,RR,SL,SR",
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
    let mut pending: Vec<u8> = Vec::with_capacity(SURROUND_BLOCK * 2);
    let mut samples71: Vec<i16> = Vec::with_capacity(SURROUND_BLOCK / 2);
    let mut stereo: Vec<i16> = Vec::with_capacity(BLOCK / 2);
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
            while pending.len() >= SURROUND_BLOCK {
                let block: Vec<u8> = pending.drain(..SURROUND_BLOCK).collect();
                // No receiver: no client is playing; the block is dropped.
                if out.receiver_count() > 0 {
                    samples71.clear();
                    samples71.extend(
                        block
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|b| i16::from_le_bytes(*b)),
                    );
                    stereo.clear();
                    crate::sound::downmix_71(&samples71, &mut stereo);
                    let _ = out.send(stereo.iter().flat_map(|s| s.to_le_bytes()).collect());
                }
                let _ = surround.send(block);
            }
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(SURROUND_CHANNELS as u32);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[..SURROUND_CHANNELS].copy_from_slice(&[
        spa::sys::SPA_AUDIO_CHANNEL_FL,
        spa::sys::SPA_AUDIO_CHANNEL_FR,
        spa::sys::SPA_AUDIO_CHANNEL_FC,
        spa::sys::SPA_AUDIO_CHANNEL_LFE,
        spa::sys::SPA_AUDIO_CHANNEL_RL,
        spa::sys::SPA_AUDIO_CHANNEL_RR,
        spa::sys::SPA_AUDIO_CHANNEL_SL,
        spa::sys::SPA_AUDIO_CHANNEL_SR,
    ]);
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

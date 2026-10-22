//! Client side of the RDP graphics pipeline (MS-RDPEGFX), passing H.264 straight through.
//!
//! IronRDP's own EGFX client decodes to RGBA in system memory; on phones and for low
//! latency the H.264 bitstream should instead go to the platform's hardware decoder, which
//! renders into a window surface. This processor does the protocol part (capabilities, ZGFX,
//! surfaces, frame acknowledgements) and hands each AVC420 bitstream to a [`VideoSink`].
//! glidedesk servers send one surface covering the desktop, which is what this handles.

use std::sync::{Arc, Mutex};

use ironrdp_core::{Decode as _, Encode as _, ReadCursor, decode_cursor, impl_as_any};
use ironrdp_dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp_egfx::pdu::{
    Avc420BitmapStream, CapabilitiesAdvertisePdu, CapabilitiesV81Flags, CapabilitySet, Codec1Type,
    FrameAcknowledgePdu, GfxPdu, QueueDepth,
};
use ironrdp_graphics::zgfx;
use ironrdp_pdu::{PduResult, decode_err};
use tracing::{debug, trace};

/// Where decoded-to-be video goes. Implemented by the platform (MediaCodec on Android, a
/// file in `glidedesk-dump`).
pub use glidedesk_proto::Codec as VideoCodec;

pub trait VideoSink: Send {
    /// The desktop has a new size (or codec); a new stream starting with an IDR follows.
    fn reset(&mut self, width: u32, height: u32, codec: VideoCodec);
    /// One access unit (H.264 or HEVC), Annex B.
    fn frame(&mut self, data: &[u8]);
    /// The pointer shape (native protocol; over RDP it comes with the classic updates).
    fn cursor(&mut self, _cursor: &glidedesk_proto::Cursor) {}
    /// Session audio: PCM S16LE 48 kHz stereo (native protocol).
    fn audio(&mut self, _pcm: &[u8]) {}
}

pub type SharedSink = Arc<Mutex<dyn VideoSink>>;

pub struct GfxClient {
    sink: SharedSink,
    decompressor: zgfx::Decompressor,
    buffer: Vec<u8>,
    decoded: u32,
}

impl GfxClient {
    pub fn new(sink: SharedSink) -> Self {
        Self {
            sink,
            decompressor: zgfx::Decompressor::new(),
            buffer: Vec::new(),
            decoded: 0,
        }
    }
}

impl_as_any!(GfxClient);

impl DvcProcessor for GfxClient {
    fn channel_name(&self) -> &str {
        ironrdp_egfx::CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // AVC420 only for now: the main view of AVC444 needs the auxiliary view combined in
        // a shader, which comes later.
        let caps = [CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
        }];
        let pdu = GfxPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu::from_typed(&caps));
        Ok(vec![Box::new(pdu) as DvcMessage])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        self.buffer.clear();
        self.decompressor
            .decompress(payload, &mut self.buffer)
            .map_err(|e| decode_err!(e))?;
        let mut pdus = Vec::new();
        {
            let mut cursor = ReadCursor::new(&self.buffer);
            while !cursor.is_empty() {
                let pdu: GfxPdu = decode_cursor(&mut cursor).map_err(|e| decode_err!(e))?;
                pdus.push(pdu);
            }
        }
        let mut out: Vec<DvcMessage> = Vec::new();
        for pdu in pdus {
            match pdu {
                GfxPdu::ResetGraphics(r) => {
                    debug!(width = r.width, height = r.height, "ResetGraphics");
                    self.sink
                        .lock()
                        .unwrap()
                        .reset(r.width, r.height, VideoCodec::H264);
                }
                GfxPdu::WireToSurface1(w) if w.codec_id == Codec1Type::Avc420 => {
                    let mut cursor = ReadCursor::new(&w.bitmap_data);
                    let stream =
                        Avc420BitmapStream::decode(&mut cursor).map_err(|e| decode_err!(e))?;
                    self.sink.lock().unwrap().frame(stream.data);
                }
                GfxPdu::WireToSurface1(w) => debug!(codec = ?w.codec_id, "Unsupported codec"),
                GfxPdu::EndFrame(e) => {
                    self.decoded = self.decoded.wrapping_add(1);
                    out.push(Box::new(GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
                        queue_depth: QueueDepth::Unavailable,
                        frame_id: e.frame_id,
                        total_frames_decoded: self.decoded,
                    })) as DvcMessage);
                }
                other => trace!(pdu = other.name(), "Graphics PDU"),
            }
        }
        Ok(out)
    }
}

impl DvcClientProcessor for GfxClient {}

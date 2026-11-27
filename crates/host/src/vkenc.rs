//! H.264/HEVC through Vulkan Video (`VK_KHR_video_encode_queue`): AMD (radv), Intel (anv) and
//! NVIDIA drivers on Linux, and on Windows where the driver exposes it. Experimental, behind
//! the `vulkan-video` feature (`--encoder vulkan`).
//!
//! The same low-latency shape as the other backends: no B-frames, P-frames that reference
//! only the previous picture (two DPB slots), an endless GOP with IDRs only on request, full
//! range BT.709 signalled in the VUI, a rate control of about one frame of buffer under the
//! link's ceiling. Frames come in as NV12 in system memory ([`crate::csc`]) and are copied into
//! a multi-planar image through a staging buffer on a graphics/compute queue; importing the
//! captured DMA-BUF straight into the source image (`VK_EXT_external_memory_dma_buf`, with
//! the colour conversion in a compute shader) is a later step.
//!
//! The parameter sets are built from the Video Std structures; the driver writes them
//! (`vkGetEncodedVideoSessionParametersKHR`, with whatever it overrode) and they go in front
//! of every IDR, in Annex B like the pictures.
//!
//! The call sequence follows FFmpeg's `libavcodec/vulkan_encode.c`, `vulkan_encode_h264.c` and
//! `vulkan_encode_h265.c` (commit `7fdb4c2`) and the Khronos Vulkan-Video-Samples encoder
//! (`vk_video_encoder/libs/VkVideoEncoder`, commit `9239ae2`); no code is taken from either.

use std::ffi::{CStr, c_void};
use std::ptr;

use anyhow::{Context as _, Result, bail};
use ash::vk::native as sv;
use ash::{khr, vk};
use tracing::{debug, info, warn};

use crate::color;
use crate::csc::{self, Nv12Frame};
use crate::encoder::{Backend, Codec, EncodedFrame, Request};
use crate::frame::{Frame, FrameData};

/// DPB slots: the reconstructed current picture and the one it references.
const DPB_SLOTS: u32 = 2;
/// How long a frame may take on the GPU before the encoder gives up on the session.
const FENCE_TIMEOUT_NS: u64 = 2_000_000_000;
/// `STD_VIDEO_H264_NO_REFERENCE_PICTURE` / `STD_VIDEO_H265_NO_REFERENCE_PICTURE`.
const NO_REFERENCE: u8 = 0xff;
/// `log2_max_frame_num_minus4` and `log2_max_pic_order_cnt_lsb_minus4`: 16-bit counters, so
/// they wrap rarely.
const LOG2_COUNTERS_MINUS4: u8 = 12;
/// Constant QP when the driver offers no bitrate control.
const CQP_H264: i32 = 24;
const CQP_HEVC: i32 = 26;
/// Lowest level signalled: low levels limit the bitrate more than any frame size here needs.
const MIN_LEVEL_H264: u32 = sv::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1;
const MIN_LEVEL_H265: u32 = sv::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1;

/// `disable_deblocking_filter_idc` 0: the H.264 loop filter on.
const DEBLOCKING_ON: sv::StdVideoH264DisableDeblockingFilterIdc =
    sv::StdVideoH264DisableDeblockingFilterIdc_STD_VIDEO_H264_DISABLE_DEBLOCKING_FILTER_IDC_DISABLED;

/// `VK_MAKE_VIDEO_STD_VERSION(1, 0, 0)`: the Video Std headers ash 0.38 was generated from.
const STD_VERSION_1_0_0: u32 = 1 << 22;
const STD_H264_ENCODE: &CStr = c"VK_STD_vulkan_video_codec_h264_encode";
const STD_H265_ENCODE: &CStr = c"VK_STD_vulkan_video_codec_h265_encode";

/// Plain C structures from the Video Std headers, for which all-zero is a valid value.
trait Zeroed: Copy {
    fn zeroed() -> Self {
        // SAFETY: implemented only for the `vk::native` structures below, which hold integers,
        // C enums (integers), bitfield units, arrays of those and raw pointers: all-zero is
        // every flag clear, every value 0 and every pointer null.
        unsafe { std::mem::zeroed() }
    }
}

macro_rules! zeroed {
    ($($t:ident),* $(,)?) => { $(impl Zeroed for sv::$t {})* };
}

zeroed!(
    StdVideoH264HrdParameters,
    StdVideoH264ScalingLists,
    StdVideoH264SequenceParameterSetVui,
    StdVideoH264SequenceParameterSet,
    StdVideoH264PictureParameterSet,
    StdVideoEncodeH264SliceHeader,
    StdVideoEncodeH264ReferenceListsInfo,
    StdVideoEncodeH264PictureInfo,
    StdVideoEncodeH264ReferenceInfo,
    StdVideoH265ProfileTierLevel,
    StdVideoH265DecPicBufMgr,
    StdVideoH265HrdParameters,
    StdVideoH265VideoParameterSet,
    StdVideoH265SequenceParameterSetVui,
    StdVideoH265ShortTermRefPicSet,
    StdVideoH265LongTermRefPicsSps,
    StdVideoH265SequenceParameterSet,
    StdVideoH265PictureParameterSet,
    StdVideoEncodeH265SliceSegmentHeader,
    StdVideoEncodeH265ReferenceListsInfo,
    StdVideoEncodeH265LongTermRefPics,
    StdVideoEncodeH265PictureInfo,
    StdVideoEncodeH265ReferenceInfo,
);

fn op(codec: Codec) -> vk::VideoCodecOperationFlagsKHR {
    match codec {
        Codec::H264 => vk::VideoCodecOperationFlagsKHR::ENCODE_H264,
        Codec::Hevc => vk::VideoCodecOperationFlagsKHR::ENCODE_H265,
    }
}

fn align_up(v: u64, a: u64) -> u64 {
    if a <= 1 { v } else { v.div_ceil(a) * a }
}

/// First memory type allowed by `bits` with all the flags of the first preference that any
/// type has.
fn memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    prefs: &[vk::MemoryPropertyFlags],
) -> Option<u32> {
    prefs.iter().find_map(|&want| {
        (0..props.memory_type_count.min(vk::MAX_MEMORY_TYPES as u32)).find(|&i| {
            bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want)
        })
    })
}

/// Whether a parameter set or picture starts with an Annex B start code.
fn annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

// ---------------------------------------------------------------------------------------
// Levels and HEVC block sizes

/// H.264 levels (ITU-T H.264 Table A-1): MaxMBPS and MaxFS, indexed by `StdVideoH264LevelIdc`
/// (1.0 … 6.2; level 1b has no value there).
const H264_LEVELS: [(u64, u64); 19] = [
    (1485, 99),
    (3000, 396),
    (6000, 396),
    (11880, 396),
    (11880, 396),
    (19800, 792),
    (20250, 1620),
    (40500, 1620),
    (108000, 3600),
    (216000, 5120),
    (245760, 8192),
    (245760, 8192),
    (522240, 8704),
    (589824, 22080),
    (983040, 36864),
    (2073600, 36864),
    (4177920, 139264),
    (8355840, 139264),
    (16711680, 139264),
];

/// HEVC levels (ITU-T H.265 Table A.8): MaxLumaPs and MaxLumaSr, indexed by
/// `StdVideoH265LevelIdc` (1 … 6.2).
const H265_LEVELS: [(u64, u64); 13] = [
    (36864, 552960),
    (122880, 3686400),
    (245760, 7372800),
    (552960, 16588800),
    (983040, 33177600),
    (2228224, 66846720),
    (2228224, 133693440),
    (8912896, 267386880),
    (8912896, 534773760),
    (8912896, 1069547520),
    (35651584, 1069547520),
    (35651584, 2139095040),
    (35651584, 4278190080),
];

/// The lowest H.264 level (as `StdVideoH264LevelIdc`) for the frame size and rate, at least
/// [`MIN_LEVEL_H264`].
fn h264_level(w: u32, h: u32, fps: u32) -> u32 {
    let (wm, hm) = (u64::from(w.div_ceil(16)), u64::from(h.div_ceil(16)));
    let (fs, rate) = (wm * hm, wm * hm * u64::from(fps));
    let level = H264_LEVELS
        .iter()
        .position(|&(max_rate, max_fs)| {
            fs <= max_fs && rate <= max_rate && wm * wm <= 8 * max_fs && hm * hm <= 8 * max_fs
        })
        .unwrap_or(H264_LEVELS.len() - 1) as u32;
    level.max(MIN_LEVEL_H264)
}

/// The lowest HEVC level (as `StdVideoH265LevelIdc`) for the frame size and rate, at least
/// [`MIN_LEVEL_H265`].
fn h265_level(w: u32, h: u32, fps: u32) -> u32 {
    let (w, h) = (u64::from(w), u64::from(h));
    let (ps, rate) = (w * h, w * h * u64::from(fps));
    let level = H265_LEVELS
        .iter()
        .position(|&(max_ps, max_sr)| {
            ps <= max_ps && rate <= max_sr && w * w <= 8 * max_ps && h * h <= 8 * max_ps
        })
        .unwrap_or(H265_LEVELS.len() - 1) as u32;
    level.max(MIN_LEVEL_H265)
}

/// HEVC coding and transform block sizes (log2) within what the encoder offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HevcBlocks {
    ctb: u8,
    min_cb: u8,
    min_tb: u8,
    max_tb: u8,
}

/// The largest CTB, a 16×16 minimum CU when the CTB is 32 or more (8×8 costs much more on
/// some encoders, as FFmpeg notes) and the transform sizes offered, kept within H.265 7.4.3.2:
/// MinTb < MinCb, MaxTb ≤ min(Ctb, 32).
fn hevc_blocks(
    ctb: vk::VideoEncodeH265CtbSizeFlagsKHR,
    tb: vk::VideoEncodeH265TransformBlockSizeFlagsKHR,
) -> HevcBlocks {
    use vk::VideoEncodeH265CtbSizeFlagsKHR as C;
    use vk::VideoEncodeH265TransformBlockSizeFlagsKHR as T;
    let ctb_log2 = if ctb.contains(C::TYPE_64) {
        6
    } else if ctb.contains(C::TYPE_32) {
        5
    } else {
        4
    };
    let min_cb = if ctb_log2 >= 5 { 4 } else { 3 };
    let sizes = [(T::TYPE_4, 2), (T::TYPE_8, 3), (T::TYPE_16, 4), (T::TYPE_32, 5)];
    let min_tb = sizes
        .iter()
        .find(|(f, _)| tb.contains(*f))
        .map_or(2, |s| s.1)
        .min(min_cb - 1);
    let max_tb = sizes
        .iter()
        .rev()
        .find(|(f, _)| tb.contains(*f))
        .map_or(5, |s| s.1)
        .min(ctb_log2.min(5))
        .max(min_tb);
    HevcBlocks {
        ctb: ctb_log2,
        min_cb,
        min_tb,
        max_tb,
    }
}

// ---------------------------------------------------------------------------------------
// Picture order

/// A picture in the DPB: its slot and what its reference info needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RefPic {
    slot: u8,
    frame_num: u32,
    poc: i32,
    idr: bool,
}

/// What the next picture is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    idr: bool,
    /// DPB slot the reconstructed picture goes to.
    setup_slot: u8,
    /// H.264 `frame_num` (wraps at 2^16).
    frame_num: u32,
    /// H.264: 2 per frame (POC type 2); HEVC: 1 per frame.
    poc: i32,
    idr_pic_id: u16,
    /// The previous picture, referenced by a P-frame.
    reference: Option<RefPic>,
}

/// IDR, then P-frames that each reference the previous picture; two DPB slots in turn.
#[derive(Debug)]
struct Gop {
    codec: Codec,
    reference: Option<RefPic>,
    /// Pictures since the last IDR (0 for the IDR).
    since_idr: u32,
    idrs: u32,
}

impl Gop {
    fn new(codec: Codec) -> Self {
        Self {
            codec,
            reference: None,
            since_idr: 0,
            idrs: 0,
        }
    }

    fn plan(&self, force_idr: bool) -> Plan {
        let idr = force_idr || self.reference.is_none();
        let n = if idr { 0 } else { self.since_idr + 1 };
        let reference = if idr { None } else { self.reference };
        let setup_slot = reference.map_or(0, |r| 1 - r.slot);
        let max_frame_num = 1u32 << (LOG2_COUNTERS_MINUS4 + 4);
        let poc = match self.codec {
            Codec::H264 => (n.wrapping_mul(2) & i32::MAX as u32) as i32,
            Codec::Hevc => (n & i32::MAX as u32) as i32,
        };
        Plan {
            idr,
            setup_slot,
            frame_num: n & (max_frame_num - 1),
            poc,
            idr_pic_id: self.idrs as u16,
            reference,
        }
    }

    /// The planned picture was encoded: it is the next one's reference.
    fn commit(&mut self, p: &Plan) {
        self.reference = Some(RefPic {
            slot: p.setup_slot,
            frame_num: p.frame_num,
            poc: p.poc,
            idr: p.idr,
        });
        if p.idr {
            self.since_idr = 0;
            self.idrs = self.idrs.wrapping_add(1);
        } else {
            self.since_idr += 1;
        }
    }
}

// ---------------------------------------------------------------------------------------
// Chained structures
//
// Each of these holds structures that point at each other. They are wired by `wire()` once
// they are in their final place, and not moved afterwards.

/// The video profile (codec, 4:2:0 8-bit, desktop streaming hints), for every structure that
/// names one: capabilities, session, images, buffers and query pool.
struct Profile {
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
    h264: vk::VideoEncodeH264ProfileInfoKHR<'static>,
    h265: vk::VideoEncodeH265ProfileInfoKHR<'static>,
    info: vk::VideoProfileInfoKHR<'static>,
    list: vk::VideoProfileListInfoKHR<'static>,
    codec: Codec,
}

impl Profile {
    fn new(codec: Codec, h264_profile: sv::StdVideoH264ProfileIdc) -> Self {
        Self {
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
                .video_content_hints(vk::VideoEncodeContentFlagsKHR::DESKTOP)
                .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY),
            h264: vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(h264_profile),
            h265: vk::VideoEncodeH265ProfileInfoKHR {
                std_profile_idc: sv::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
                ..Default::default()
            },
            info: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(op(codec))
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
                .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8),
            list: vk::VideoProfileListInfoKHR::default(),
            codec,
        }
    }

    fn wire(&mut self) {
        let p: *mut Self = self;
        // SAFETY: `p` points to `self`; only field addresses are taken and stored.
        unsafe {
            let usage = (&raw const (*p).usage).cast::<c_void>();
            (*p).h264.p_next = usage;
            (*p).h265.p_next = usage;
            (*p).info.p_next = match (*p).codec {
                Codec::H264 => (&raw const (*p).h264).cast(),
                Codec::Hevc => (&raw const (*p).h265).cast(),
            };
            (*p).list.profile_count = 1;
            (*p).list.p_profiles = &raw const (*p).info;
        }
    }

    fn info_ptr(&self) -> *const c_void {
        ptr::from_ref(&self.info).cast()
    }

    fn list_ptr(&self) -> *const c_void {
        ptr::from_ref(&self.list).cast()
    }
}

/// Rate control state of the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RateControl {
    mode: vk::VideoEncodeRateControlModeFlagsKHR,
    /// Ceiling, bits per second (CBR and VBR).
    max_bitrate: u64,
    fps: u32,
}

impl RateControl {
    fn layered(&self) -> bool {
        self.mode == vk::VideoEncodeRateControlModeFlagsKHR::CBR
            || self.mode == vk::VideoEncodeRateControlModeFlagsKHR::VBR
    }
}

/// `VkVideoEncodeRateControlInfoKHR` and the codec's, for `vkCmdBeginVideoCodingKHR` (the
/// state the session is in) and `vkCmdControlVideoCodingKHR` (the state it goes to).
struct RcChain {
    h264_layer: vk::VideoEncodeH264RateControlLayerInfoKHR<'static>,
    h265_layer: vk::VideoEncodeH265RateControlLayerInfoKHR<'static>,
    layer: vk::VideoEncodeRateControlLayerInfoKHR<'static>,
    h264: vk::VideoEncodeH264RateControlInfoKHR<'static>,
    h265: vk::VideoEncodeH265RateControlInfoKHR<'static>,
    info: vk::VideoEncodeRateControlInfoKHR<'static>,
    codec: Codec,
    layered: bool,
}

impl RcChain {
    fn new(codec: Codec, rc: RateControl) -> Self {
        let layered = rc.layered();
        // VBR aims below the ceiling (a still desktop needs little), CBR at it (the spec wants
        // average = max there).
        let average = if rc.mode == vk::VideoEncodeRateControlModeFlagsKHR::VBR {
            rc.max_bitrate / 4 * 3
        } else {
            rc.max_bitrate
        };
        // About one frame of buffer, as with NVENC: no picture takes longer than a frame
        // time to cross a link that carries the ceiling.
        let vbv_ms = if layered {
            (1000 / rc.fps.max(1)).max(1)
        } else {
            0
        };
        let flat = vk::VideoEncodeH264RateControlFlagsKHR::REFERENCE_PATTERN_FLAT
            | vk::VideoEncodeH264RateControlFlagsKHR::REGULAR_GOP;
        let flat265 = vk::VideoEncodeH265RateControlFlagsKHR::REFERENCE_PATTERN_FLAT
            | vk::VideoEncodeH265RateControlFlagsKHR::REGULAR_GOP;
        Self {
            h264_layer: vk::VideoEncodeH264RateControlLayerInfoKHR::default(),
            h265_layer: vk::VideoEncodeH265RateControlLayerInfoKHR::default(),
            layer: vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(average)
                .max_bitrate(rc.max_bitrate)
                .frame_rate_numerator(rc.fps.max(1))
                .frame_rate_denominator(1),
            // UINT32_MAX: an infinite GOP and IDR period.
            h264: vk::VideoEncodeH264RateControlInfoKHR::default()
                .flags(flat)
                .gop_frame_count(u32::MAX)
                .idr_period(u32::MAX)
                .consecutive_b_frame_count(0)
                .temporal_layer_count(u32::from(layered)),
            h265: vk::VideoEncodeH265RateControlInfoKHR {
                flags: flat265,
                gop_frame_count: u32::MAX,
                idr_period: u32::MAX,
                consecutive_b_frame_count: 0,
                sub_layer_count: u32::from(layered),
                ..Default::default()
            },
            info: vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(rc.mode)
                .virtual_buffer_size_in_ms(vbv_ms)
                .initial_virtual_buffer_size_in_ms(vbv_ms),
            codec,
            layered,
        }
    }

    fn wire(&mut self) {
        let p: *mut Self = self;
        // SAFETY: `p` points to `self`; only field addresses are taken and stored.
        unsafe {
            (*p).layer.p_next = match (*p).codec {
                Codec::H264 => (&raw const (*p).h264_layer).cast(),
                Codec::Hevc => (&raw const (*p).h265_layer).cast(),
            };
            if (*p).layered {
                (*p).info.layer_count = 1;
                (*p).info.p_layers = &raw const (*p).layer;
            }
            (*p).info.p_next = match (*p).codec {
                Codec::H264 => (&raw const (*p).h264).cast(),
                Codec::Hevc => (&raw const (*p).h265).cast(),
            };
        }
    }

    fn head(&self) -> *const c_void {
        ptr::from_ref(&self.info).cast()
    }
}

/// The H.264 structures of one picture.
struct PictureH264 {
    slice_header: sv::StdVideoEncodeH264SliceHeader,
    slice: vk::VideoEncodeH264NaluSliceInfoKHR<'static>,
    lists: sv::StdVideoEncodeH264ReferenceListsInfo,
    std: sv::StdVideoEncodeH264PictureInfo,
    info: vk::VideoEncodeH264PictureInfoKHR<'static>,
    setup_std: sv::StdVideoEncodeH264ReferenceInfo,
    setup: vk::VideoEncodeH264DpbSlotInfoKHR<'static>,
    ref_std: sv::StdVideoEncodeH264ReferenceInfo,
    reference: vk::VideoEncodeH264DpbSlotInfoKHR<'static>,
}

/// The HEVC structures of one picture.
struct PictureH265 {
    slice_header: sv::StdVideoEncodeH265SliceSegmentHeader,
    slice: vk::VideoEncodeH265NaluSliceSegmentInfoKHR<'static>,
    lists: sv::StdVideoEncodeH265ReferenceListsInfo,
    rps: sv::StdVideoH265ShortTermRefPicSet,
    long_term: sv::StdVideoEncodeH265LongTermRefPics,
    std: sv::StdVideoEncodeH265PictureInfo,
    info: vk::VideoEncodeH265PictureInfoKHR<'static>,
    setup_std: sv::StdVideoEncodeH265ReferenceInfo,
    setup: vk::VideoEncodeH265DpbSlotInfoKHR<'static>,
    ref_std: sv::StdVideoEncodeH265ReferenceInfo,
    reference: vk::VideoEncodeH265DpbSlotInfoKHR<'static>,
}

// One per frame, on the stack and wired in place: not boxed.
#[allow(clippy::large_enum_variant)]
enum Picture {
    H264(PictureH264),
    H265(PictureH265),
}

impl Picture {
    /// `qp`: the constant QP when the rate control is off (else 0, as the spec requires).
    fn new(codec: Codec, p: &Plan, qp: Option<i32>, sao: bool) -> Self {
        let qp_delta = qp.map_or(0, |q| (q - 26).clamp(-26, 25) as i8);
        match codec {
            Codec::H264 => {
                let mut slice_header = sv::StdVideoEncodeH264SliceHeader::zeroed();
                slice_header.flags.set_direct_spatial_mv_pred_flag(1);
                slice_header.first_mb_in_slice = 0;
                slice_header.slice_type = if p.idr {
                    sv::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I
                } else {
                    sv::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P
                };
                slice_header.cabac_init_idc =
                    sv::StdVideoH264CabacInitIdc_STD_VIDEO_H264_CABAC_INIT_IDC_0;
                // disable_deblocking_filter_idc = 0 ("DISABLED"): the loop filter is on.
                slice_header.disable_deblocking_filter_idc = DEBLOCKING_ON;
                slice_header.slice_qp_delta = qp_delta;

                let mut lists = sv::StdVideoEncodeH264ReferenceListsInfo::zeroed();
                lists.RefPicList0 = [NO_REFERENCE; 32];
                lists.RefPicList1 = [NO_REFERENCE; 32];
                if let Some(r) = p.reference {
                    lists.RefPicList0[0] = r.slot;
                }

                let pic_type = if p.idr {
                    sv::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
                } else {
                    sv::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
                };
                let mut std = sv::StdVideoEncodeH264PictureInfo::zeroed();
                std.flags.set_IdrPicFlag(u32::from(p.idr));
                std.flags.set_is_reference(1);
                std.idr_pic_id = p.idr_pic_id;
                std.primary_pic_type = pic_type;
                std.frame_num = p.frame_num;
                std.PicOrderCnt = p.poc;

                let mut setup_std = sv::StdVideoEncodeH264ReferenceInfo::zeroed();
                setup_std.primary_pic_type = pic_type;
                setup_std.FrameNum = p.frame_num;
                setup_std.PicOrderCnt = p.poc;

                let mut ref_std = sv::StdVideoEncodeH264ReferenceInfo::zeroed();
                if let Some(r) = p.reference {
                    ref_std.primary_pic_type = if r.idr {
                        sv::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
                    } else {
                        sv::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
                    };
                    ref_std.FrameNum = r.frame_num;
                    ref_std.PicOrderCnt = r.poc;
                }
                Self::H264(PictureH264 {
                    slice_header,
                    slice: vk::VideoEncodeH264NaluSliceInfoKHR::default()
                        .constant_qp(qp.unwrap_or(0)),
                    lists,
                    std,
                    info: vk::VideoEncodeH264PictureInfoKHR::default(),
                    setup_std,
                    setup: vk::VideoEncodeH264DpbSlotInfoKHR::default(),
                    ref_std,
                    reference: vk::VideoEncodeH264DpbSlotInfoKHR::default(),
                })
            }
            Codec::Hevc => {
                let mut slice_header = sv::StdVideoEncodeH265SliceSegmentHeader::zeroed();
                slice_header.flags.set_first_slice_segment_in_pic_flag(1);
                slice_header.flags.set_slice_sao_luma_flag(u32::from(sao));
                slice_header.flags.set_slice_sao_chroma_flag(u32::from(sao));
                slice_header.flags.set_collocated_from_l0_flag(1);
                slice_header.slice_type = if p.idr {
                    sv::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_I
                } else {
                    sv::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_P
                };
                slice_header.MaxNumMergeCand = 5;
                slice_header.slice_qp_delta = qp_delta;

                let mut lists = sv::StdVideoEncodeH265ReferenceListsInfo::zeroed();
                lists.RefPicList0 = [NO_REFERENCE; 15];
                lists.RefPicList1 = [NO_REFERENCE; 15];
                // The short-term set: the previous picture, used by this one.
                let mut rps = sv::StdVideoH265ShortTermRefPicSet::zeroed();
                if let Some(r) = p.reference {
                    lists.RefPicList0[0] = r.slot;
                    rps.num_negative_pics = 1;
                    rps.delta_poc_s0_minus1[0] = (p.poc - r.poc - 1).max(0) as u16;
                    rps.used_by_curr_pic_s0_flag = 1;
                }

                let pic_type = if p.idr {
                    sv::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
                } else {
                    sv::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
                };
                let mut std = sv::StdVideoEncodeH265PictureInfo::zeroed();
                std.flags.set_is_reference(1);
                std.flags.set_IrapPicFlag(u32::from(p.idr));
                std.flags.set_pic_output_flag(1);
                std.pic_type = pic_type;
                std.PicOrderCntVal = p.poc;

                let mut setup_std = sv::StdVideoEncodeH265ReferenceInfo::zeroed();
                setup_std.pic_type = pic_type;
                setup_std.PicOrderCntVal = p.poc;

                let mut ref_std = sv::StdVideoEncodeH265ReferenceInfo::zeroed();
                if let Some(r) = p.reference {
                    ref_std.pic_type = if r.idr {
                        sv::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
                    } else {
                        sv::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
                    };
                    ref_std.PicOrderCntVal = r.poc;
                }
                Self::H265(PictureH265 {
                    slice_header,
                    slice: vk::VideoEncodeH265NaluSliceSegmentInfoKHR {
                        constant_qp: qp.unwrap_or(0),
                        ..Default::default()
                    },
                    lists,
                    rps,
                    long_term: sv::StdVideoEncodeH265LongTermRefPics::zeroed(),
                    std,
                    info: vk::VideoEncodeH265PictureInfoKHR::default(),
                    setup_std,
                    setup: vk::VideoEncodeH265DpbSlotInfoKHR::default(),
                    ref_std,
                    reference: vk::VideoEncodeH265DpbSlotInfoKHR::default(),
                })
            }
        }
    }

    fn wire(&mut self) {
        match self {
            Self::H264(b) => {
                let p: *mut PictureH264 = b;
                // SAFETY: `p` points to the structure in place; only field addresses are taken
                // and stored.
                unsafe {
                    (*p).slice.p_std_slice_header = &raw const (*p).slice_header;
                    (*p).std.pRefLists = &raw const (*p).lists;
                    (*p).info.nalu_slice_entry_count = 1;
                    (*p).info.p_nalu_slice_entries = &raw const (*p).slice;
                    (*p).info.p_std_picture_info = &raw const (*p).std;
                    (*p).setup.p_std_reference_info = &raw const (*p).setup_std;
                    (*p).reference.p_std_reference_info = &raw const (*p).ref_std;
                }
            }
            Self::H265(b) => {
                let p: *mut PictureH265 = b;
                // SAFETY: as above.
                unsafe {
                    (*p).slice.p_std_slice_segment_header = &raw const (*p).slice_header;
                    (*p).std.pRefLists = &raw const (*p).lists;
                    (*p).std.pShortTermRefPicSet = &raw const (*p).rps;
                    (*p).std.pLongTermRefPics = &raw const (*p).long_term;
                    (*p).info.nalu_slice_segment_entry_count = 1;
                    (*p).info.p_nalu_slice_segment_entries = &raw const (*p).slice;
                    (*p).info.p_std_picture_info = &raw const (*p).std;
                    (*p).setup.p_std_reference_info = &raw const (*p).setup_std;
                    (*p).reference.p_std_reference_info = &raw const (*p).ref_std;
                }
            }
        }
    }

    /// The codec picture info, for `VkVideoEncodeInfoKHR::pNext`.
    fn info(&self) -> *const c_void {
        match self {
            Self::H264(b) => ptr::from_ref(&b.info).cast(),
            Self::H265(b) => ptr::from_ref(&b.info).cast(),
        }
    }

    /// The DPB slot info of the reconstructed picture.
    fn setup(&self) -> *const c_void {
        match self {
            Self::H264(b) => ptr::from_ref(&b.setup).cast(),
            Self::H265(b) => ptr::from_ref(&b.setup).cast(),
        }
    }

    /// The DPB slot info of the reference.
    fn reference(&self) -> *const c_void {
        match self {
            Self::H264(b) => ptr::from_ref(&b.reference).cast(),
            Self::H265(b) => ptr::from_ref(&b.reference).cast(),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Capabilities

/// What the driver reports for a profile.
#[derive(Clone, Copy, Debug)]
struct Caps {
    size_align: u64,
    /// The larger of the picture access and the encode input granularity.
    granularity: vk::Extent2D,
    min_extent: vk::Extent2D,
    max_extent: vk::Extent2D,
    max_dpb_slots: u32,
    max_active_refs: u32,
    std_version: u32,
    rc_modes: vk::VideoEncodeRateControlModeFlagsKHR,
    max_bitrate: u64,
    feedback: vk::VideoEncodeFeedbackFlagsKHR,
    max_l0_refs: u32,
    min_qp: i32,
    max_qp: i32,
    /// `StdVideoH264LevelIdc` or `StdVideoH265LevelIdc`.
    max_level: u32,
    requires_gop_remaining: bool,
    h264_std: vk::VideoEncodeH264StdFlagsKHR,
    h265_std: vk::VideoEncodeH265StdFlagsKHR,
    ctb_sizes: vk::VideoEncodeH265CtbSizeFlagsKHR,
    tb_sizes: vk::VideoEncodeH265TransformBlockSizeFlagsKHR,
}

/// `vkGetPhysicalDeviceVideoCapabilitiesKHR` with the encode and codec structures.
fn query_caps(
    vq: &khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    profile: &Profile,
) -> Result<Caps, vk::Result> {
    let mut h264 = vk::VideoEncodeH264CapabilitiesKHR::default();
    let mut h265 = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut enc = vk::VideoEncodeCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default();
    enc.p_next = match profile.codec {
        Codec::H264 => (&raw mut h264).cast(),
        Codec::Hevc => (&raw mut h265).cast(),
    };
    caps.p_next = (&raw mut enc).cast();
    // SAFETY: the profile chain is wired and the output chain points to live locals.
    unsafe {
        (vq.fp().get_physical_device_video_capabilities_khr)(pd, &profile.info, &mut caps)
    }
    .result()?;
    let (max_l0_refs, min_qp, max_qp, max_level, requires_gop_remaining) = match profile.codec {
        Codec::H264 => (
            h264.max_p_picture_l0_reference_count,
            h264.min_qp,
            h264.max_qp,
            h264.max_level_idc,
            h264.requires_gop_remaining_frames == vk::TRUE,
        ),
        Codec::Hevc => (
            h265.max_p_picture_l0_reference_count,
            h265.min_qp,
            h265.max_qp,
            h265.max_level_idc,
            h265.requires_gop_remaining_frames == vk::TRUE,
        ),
    };
    Ok(Caps {
        size_align: caps.min_bitstream_buffer_size_alignment,
        granularity: vk::Extent2D {
            width: caps
                .picture_access_granularity
                .width
                .max(enc.encode_input_picture_granularity.width)
                .max(1),
            height: caps
                .picture_access_granularity
                .height
                .max(enc.encode_input_picture_granularity.height)
                .max(1),
        },
        min_extent: caps.min_coded_extent,
        max_extent: caps.max_coded_extent,
        max_dpb_slots: caps.max_dpb_slots,
        max_active_refs: caps.max_active_reference_pictures,
        std_version: caps.std_header_version.spec_version,
        rc_modes: enc.rate_control_modes,
        max_bitrate: enc.max_bitrate,
        feedback: enc.supported_encode_feedback_flags,
        max_l0_refs,
        min_qp,
        max_qp,
        max_level,
        requires_gop_remaining,
        h264_std: h264.std_syntax_flags,
        h265_std: h265.std_syntax_flags,
        ctb_sizes: h265.ctb_sizes,
        tb_sizes: h265.transform_block_sizes,
    })
}

/// `vkGetPhysicalDeviceVideoFormatPropertiesKHR` for `usage` with the profile.
fn video_formats(
    vq: &khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    profile: &Profile,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<vk::VideoFormatPropertiesKHR<'static>>> {
    let mut info = vk::PhysicalDeviceVideoFormatInfoKHR::default().image_usage(usage);
    info.p_next = profile.list_ptr();
    let f = vq.fp().get_physical_device_video_format_properties_khr;
    let mut n = 0u32;
    // SAFETY: the profile list is wired; the first call only counts.
    unsafe { f(pd, &info, &mut n, ptr::null_mut()) }
        .result()
        .context("vkGetPhysicalDeviceVideoFormatPropertiesKHR")?;
    let mut out = vec![vk::VideoFormatPropertiesKHR::default(); n as usize];
    // SAFETY: `out` holds `n` initialised structures.
    unsafe { f(pd, &info, &mut n, out.as_mut_ptr()) }
        .result()
        .context("vkGetPhysicalDeviceVideoFormatPropertiesKHR")?;
    out.truncate(n as usize);
    Ok(out)
}

/// H.264 profiles tried in order (4:2:0 8-bit): High, Main, Constrained Baseline.
const H264_PROFILES: [sv::StdVideoH264ProfileIdc; 3] = [
    sv::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
    sv::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN,
    sv::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE,
];

// ---------------------------------------------------------------------------------------
// Device

/// The Vulkan instance and device with an encode queue and a queue for the uploads.
struct Gpu {
    // Kept for the loader library.
    _entry: ash::Entry,
    instance: ash::Instance,
    pd: vk::PhysicalDevice,
    device: ash::Device,
    vq_instance: khr::video_queue::Instance,
    vq: khr::video_queue::Device,
    veq: khr::video_encode_queue::Device,
    memory: vk::PhysicalDeviceMemoryProperties,
    enc_family: u32,
    xfer_family: u32,
    enc_queue: vk::Queue,
    xfer_queue: vk::Queue,
    /// The encode queue reports query result status (`VK_QUERY_RESULT_WITH_STATUS_BIT_KHR`).
    status_queries: bool,
    h264_profile: sv::StdVideoH264ProfileIdc,
    hevc: bool,
    name: String,
}

/// A device that can encode H.264 through Vulkan Video.
struct Candidate {
    pd: vk::PhysicalDevice,
    name: String,
    enc_family: u32,
    xfer_family: u32,
    status_queries: bool,
    h264_profile: sv::StdVideoH264ProfileIdc,
    /// Has the H.265 extension and the encode queue does HEVC (capabilities checked later).
    hevc_ext: bool,
}

impl Gpu {
    /// Opens the `index`-th device (in the order the loader lists them) that can encode H.264.
    fn open(index: usize) -> Result<Self> {
        // SAFETY: loads the system Vulkan loader; its initialisers have no preconditions.
        let entry = unsafe { ash::Entry::load() }
            .context("loading the Vulkan loader (libvulkan.so.1 / vulkan-1.dll)")?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"glidedesk")
            .engine_name(c"glidedesk")
            .api_version(vk::API_VERSION_1_3);
        let create = vk::InstanceCreateInfo::default().application_info(&app);
        // SAFETY: valid create info; the instance is destroyed in Drop (or below on failure).
        let instance =
            unsafe { entry.create_instance(&create, None) }.context("vkCreateInstance")?;
        let vq_instance = khr::video_queue::Instance::new(&entry, &instance);
        match Self::choose(&instance, &vq_instance, index) {
            Ok(c) => Self::create(entry, instance, vq_instance, c),
            Err(e) => {
                // SAFETY: nothing was created from the instance.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn choose(
        instance: &ash::Instance,
        vq: &khr::video_queue::Instance,
        index: usize,
    ) -> Result<Candidate> {
        // SAFETY: plain queries on a live instance.
        let devices = unsafe { instance.enumerate_physical_devices() }
            .context("vkEnumeratePhysicalDevices")?;
        let mut found = Vec::new();
        for pd in devices {
            match Self::candidate(instance, vq, pd) {
                Ok(c) => {
                    info!(
                        device = %c.name,
                        hevc_extension = c.hevc_ext,
                        "Vulkan Video encoder found"
                    );
                    found.push(c);
                }
                Err(e) => debug!(error = format!("{e:#}"), "Vulkan device skipped"),
            }
        }
        let n = found.len();
        found
            .into_iter()
            .nth(index)
            .with_context(|| {
                format!("no Vulkan device number {index} with H.264 encode ({n} found)")
            })
    }

    fn candidate(
        instance: &ash::Instance,
        vq: &khr::video_queue::Instance,
        pd: vk::PhysicalDevice,
    ) -> Result<Candidate> {
        // SAFETY: plain queries on a physical device of this instance.
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let name = props
            .device_name_as_c_str()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if props.api_version < vk::API_VERSION_1_3 {
            bail!(
                "{name}: Vulkan {}.{} (1.3 needed)",
                vk::api_version_major(props.api_version),
                vk::api_version_minor(props.api_version)
            );
        }
        // SAFETY: as above.
        let exts = unsafe { instance.enumerate_device_extension_properties(pd) }?;
        let has = |want: &CStr| {
            exts.iter()
                .any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == want))
        };
        for want in [
            khr::video_queue::NAME,
            khr::video_encode_queue::NAME,
            khr::video_encode_h264::NAME,
        ] {
            if !has(want) {
                bail!("{name}: no {}", want.to_string_lossy());
            }
        }

        // SAFETY: as above; every element's chain points into `video` and `status`, which
        // are allocated before the pointers are taken and outlive the call.
        let n = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
        let mut status = vec![vk::QueueFamilyQueryResultStatusPropertiesKHR::default(); n];
        let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
        let mut families = vec![vk::QueueFamilyProperties2::default(); n];
        for ((f, v), s) in families.iter_mut().zip(video.iter_mut()).zip(status.iter_mut()) {
            v.p_next = ptr::from_mut(s).cast();
            f.p_next = ptr::from_mut(v).cast();
        }
        // SAFETY: as above.
        unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut families) };
        let flags = |i: usize| families[i].queue_family_properties.queue_flags;
        let enc = (0..n)
            .find(|&i| {
                flags(i).contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
                    && video[i]
                        .video_codec_operations
                        .contains(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            })
            .with_context(|| format!("{name}: no queue that encodes H.264"))?;
        // Copies on a graphics or compute queue: no transfer granularity to respect.
        let xfer = (0..n)
            .find(|&i| flags(i).intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE))
            .with_context(|| format!("{name}: no graphics or compute queue"))?;
        let hevc_ext = has(khr::video_encode_h265::NAME)
            && video[enc]
                .video_codec_operations
                .contains(vk::VideoCodecOperationFlagsKHR::ENCODE_H265);

        let mut profile_err = vk::Result::ERROR_VIDEO_PROFILE_CODEC_NOT_SUPPORTED_KHR;
        for h264_profile in H264_PROFILES {
            let mut p = Profile::new(Codec::H264, h264_profile);
            p.wire();
            match query_caps(vq, pd, &p) {
                Ok(_) => {
                    return Ok(Candidate {
                        pd,
                        name,
                        enc_family: enc as u32,
                        xfer_family: xfer as u32,
                        status_queries: status[enc].query_result_status_support == vk::TRUE,
                        h264_profile,
                        hevc_ext,
                    });
                }
                Err(e) => profile_err = e,
            }
        }
        bail!("{name}: no H.264 4:2:0 8-bit profile ({profile_err:?})")
    }

    fn create(
        entry: ash::Entry,
        instance: ash::Instance,
        vq_instance: khr::video_queue::Instance,
        c: Candidate,
    ) -> Result<Self> {
        let priorities = [1.0f32];
        let mut queues = vec![
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(c.enc_family)
                .queue_priorities(&priorities),
        ];
        if c.xfer_family != c.enc_family {
            queues.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(c.xfer_family)
                    .queue_priorities(&priorities),
            );
        }
        let mut extensions = vec![
            khr::video_queue::NAME.as_ptr(),
            khr::video_encode_queue::NAME.as_ptr(),
            khr::video_encode_h264::NAME.as_ptr(),
        ];
        if c.hevc_ext {
            extensions.push(khr::video_encode_h265::NAME.as_ptr());
        }
        // The video pipeline stages and access flags exist only in synchronization2 (core,
        // and required, in Vulkan 1.3).
        let mut vk13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
        let create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&extensions)
            .push_next(&mut vk13);
        // SAFETY: valid create info for a physical device of this instance.
        let device = match unsafe { instance.create_device(c.pd, &create, None) } {
            Ok(d) => d,
            Err(e) => {
                // SAFETY: nothing else was created from the instance.
                unsafe { instance.destroy_instance(None) };
                bail!("vkCreateDevice on {}: {e:?}", c.name);
            }
        };
        // SAFETY: the queues were requested above; plain queries.
        let (enc_queue, xfer_queue, memory) = unsafe {
            (
                device.get_device_queue(c.enc_family, 0),
                device.get_device_queue(c.xfer_family, 0),
                instance.get_physical_device_memory_properties(c.pd),
            )
        };
        let mut gpu = Self {
            vq: khr::video_queue::Device::new(&instance, &device),
            veq: khr::video_encode_queue::Device::new(&instance, &device),
            _entry: entry,
            vq_instance,
            pd: c.pd,
            memory,
            enc_family: c.enc_family,
            xfer_family: c.xfer_family,
            enc_queue,
            xfer_queue,
            status_queries: c.status_queries,
            h264_profile: c.h264_profile,
            hevc: false,
            name: c.name,
            device,
            instance,
        };
        if c.hevc_ext {
            let mut p = Profile::new(Codec::Hevc, gpu.h264_profile);
            p.wire();
            match query_caps(&gpu.vq_instance, gpu.pd, &p) {
                Ok(_) => gpu.hevc = true,
                Err(e) => info!(error = ?e, "No HEVC Main profile from Vulkan Video"),
            }
        }
        Ok(gpu)
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: every session was destroyed before (see `Vulkan`'s field order); the device
        // goes before the instance.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Session

#[derive(Default)]
struct Image {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

struct Buffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
    /// Persistently mapped.
    ptr: *mut u8,
    coherent: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Self {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            size: 0,
            ptr: ptr::null_mut(),
            coherent: true,
        }
    }
}

/// A video session for one codec and size, with everything a frame needs. Objects are created
/// one by one into a session whose other handles are null, so Drop cleans up after a failure
/// half-way (destroying a null handle is a no-op).
struct Session {
    device: ash::Device,
    vq: khr::video_queue::Device,
    veq: khr::video_encode_queue::Device,
    codec: Codec,
    size: (u32, u32),
    enc_queue: vk::Queue,
    xfer_queue: vk::Queue,
    status_queries: bool,
    session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    params: vk::VideoSessionParametersKHR,
    /// The parameter sets in Annex B, in front of every IDR.
    header: Vec<u8>,
    src: Image,
    dpb: Image,
    staging: Buffer,
    bitstream: Buffer,
    query_pool: vk::QueryPool,
    enc_pool: vk::CommandPool,
    enc_cmd: vk::CommandBuffer,
    xfer_pool: vk::CommandPool,
    xfer_cmd: vk::CommandBuffer,
    enc_fence: vk::Fence,
    xfer_fence: vk::Fence,
    uploaded: vk::Semaphore,
    /// The DPB image is in `VIDEO_ENCODE_DPB_KHR`.
    dpb_ready: bool,
    /// The rate control wanted and the one the session is in (`None` before the reset).
    rc: RateControl,
    rc_applied: Option<RateControl>,
    max_bitrate: u64,
    /// Constant QP, with the rate control off.
    qp: i32,
    /// HEVC: SAO is on in the SPS.
    sao: bool,
    h264_profile: sv::StdVideoH264ProfileIdc,
    gop: Gop,
}

// The raw handles and mapped pointers are only used from the thread that owns the encoder.
unsafe impl Send for Session {}

fn check(r: vk::Result, what: &str) -> Result<()> {
    r.result().map_err(|e| anyhow::anyhow!("{what}: {e:?}"))
}

/// Fails when the profile cannot do what the session needs.
fn check_caps(caps: &Caps) -> Result<()> {
    let feedback = vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
        | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN;
    if !caps.feedback.contains(feedback) {
        bail!("no encode feedback for the bitstream offset and size");
    }
    if caps.max_dpb_slots < DPB_SLOTS || caps.max_active_refs < 1 || caps.max_l0_refs < 1 {
        bail!(
            "P-frames need {DPB_SLOTS} DPB slots and one reference ({} slots, {} references)",
            caps.max_dpb_slots,
            caps.max_active_refs.min(caps.max_l0_refs)
        );
    }
    if caps.std_version < STD_VERSION_1_0_0 {
        bail!("Video Std header version {:#x} is older than 1.0.0", caps.std_version);
    }
    Ok(())
}

impl Session {
    fn new(gpu: &Gpu, codec: Codec, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let mut s = Self {
            device: gpu.device.clone(),
            vq: gpu.vq.clone(),
            veq: gpu.veq.clone(),
            codec,
            size: (w, h),
            enc_queue: gpu.enc_queue,
            xfer_queue: gpu.xfer_queue,
            status_queries: gpu.status_queries,
            session: vk::VideoSessionKHR::null(),
            session_memory: Vec::new(),
            params: vk::VideoSessionParametersKHR::null(),
            header: Vec::new(),
            src: Image::default(),
            dpb: Image::default(),
            staging: Buffer::default(),
            bitstream: Buffer::default(),
            query_pool: vk::QueryPool::null(),
            enc_pool: vk::CommandPool::null(),
            enc_cmd: vk::CommandBuffer::null(),
            xfer_pool: vk::CommandPool::null(),
            xfer_cmd: vk::CommandBuffer::null(),
            enc_fence: vk::Fence::null(),
            xfer_fence: vk::Fence::null(),
            uploaded: vk::Semaphore::null(),
            dpb_ready: false,
            rc: RateControl {
                mode: vk::VideoEncodeRateControlModeFlagsKHR::DEFAULT,
                max_bitrate: u64::from(bitrate),
                fps,
            },
            rc_applied: None,
            max_bitrate: u64::MAX,
            qp: 0,
            sao: false,
            h264_profile: gpu.h264_profile,
            gop: Gop::new(codec),
        };
        let mut profile = Profile::new(codec, gpu.h264_profile);
        profile.wire();
        let caps = query_caps(&gpu.vq_instance, gpu.pd, &profile)
            .map_err(|e| anyhow::anyhow!("video capabilities: {e:?}"))?;
        debug!(?codec, ?caps, "Vulkan Video capabilities");
        check_caps(&caps)?;
        s.max_bitrate = if caps.max_bitrate > 0 {
            caps.max_bitrate
        } else {
            u64::MAX
        };
        s.rc.mode = if caps.rc_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::VBR) {
            vk::VideoEncodeRateControlModeFlagsKHR::VBR
        } else if caps.rc_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR) {
            vk::VideoEncodeRateControlModeFlagsKHR::CBR
        } else if caps.rc_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::DISABLED) {
            vk::VideoEncodeRateControlModeFlagsKHR::DISABLED
        } else {
            vk::VideoEncodeRateControlModeFlagsKHR::DEFAULT
        };
        s.rc.max_bitrate = s.rc.max_bitrate.min(s.max_bitrate);
        let cqp = match codec {
            Codec::H264 => CQP_H264,
            Codec::Hevc => CQP_HEVC,
        };
        s.qp = cqp.clamp(caps.min_qp, caps.max_qp.max(caps.min_qp));
        if caps.requires_gop_remaining {
            warn!("The driver wants GOP remaining-frame counts, which are not sent");
        }

        // Image extent: the coded size, padded to the granularity the encoder reads in.
        let extent = vk::Extent2D {
            width: w.next_multiple_of(caps.granularity.width),
            height: h.next_multiple_of(caps.granularity.height),
        };
        if w < caps.min_extent.width
            || h < caps.min_extent.height
            || extent.width > caps.max_extent.width
            || extent.height > caps.max_extent.height
        {
            bail!(
                "{w}x{h} is outside what the encoder takes ({}x{} to {}x{})",
                caps.min_extent.width,
                caps.min_extent.height,
                caps.max_extent.width,
                caps.max_extent.height
            );
        }

        let src_formats = video_formats(
            &gpu.vq_instance,
            gpu.pd,
            &profile,
            vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
        )?;
        let src_format = src_formats
            .iter()
            .find(|f| f.format == vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .with_context(|| {
                let formats: Vec<_> = src_formats.iter().map(|f| f.format).collect();
                format!("no NV12 encode input (formats offered: {formats:?})")
            })?;
        if !src_format
            .image_usage_flags
            .contains(vk::ImageUsageFlags::TRANSFER_DST)
        {
            warn!("The driver does not list copies into its NV12 input images; trying anyway");
        }
        let dpb_formats = video_formats(
            &gpu.vq_instance,
            gpu.pd,
            &profile,
            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
        )?;
        let dpb_format = dpb_formats
            .iter()
            .find(|f| f.format == vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .or(dpb_formats.first())
            .context("no DPB format")?;

        s.create_session(gpu, &profile, extent, src_format.format, dpb_format.format)?;
        let families = [gpu.enc_family, gpu.xfer_family];
        s.src = s.create_image(
            gpu,
            &profile,
            src_format,
            extent,
            1,
            vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST,
            vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
            if gpu.enc_family != gpu.xfer_family {
                &families[..]
            } else {
                &families[..1]
            },
        )?;
        s.dpb = s.create_image(
            gpu,
            &profile,
            dpb_format,
            extent,
            DPB_SLOTS,
            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
            &families[..1],
        )?;

        let nv12_bytes = u64::from(w) * u64::from(h) * 3 / 2;
        s.staging = s.create_buffer(
            gpu,
            None,
            nv12_bytes,
            vk::BufferUsageFlags::TRANSFER_SRC,
            &[
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                vk::MemoryPropertyFlags::HOST_VISIBLE,
            ],
        )?;
        // Room for an intra picture at a high quality: the size of the raw frame and some.
        let bitstream_bytes = align_up(nv12_bytes + (1 << 20), caps.size_align);
        s.bitstream = s.create_buffer(
            gpu,
            Some(&profile),
            bitstream_bytes,
            vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR,
            &[
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                vk::MemoryPropertyFlags::HOST_VISIBLE,
            ],
        )?;
        s.create_sync(gpu, &profile)?;
        s.create_parameters(&caps, fps)?;
        info!(
            device = %gpu.name,
            ?codec,
            w,
            h,
            rate_control = ?s.rc.mode,
            header_bytes = s.header.len(),
            "Vulkan Video session ready"
        );
        Ok(s)
    }

    fn create_session(
        &mut self,
        gpu: &Gpu,
        profile: &Profile,
        extent: vk::Extent2D,
        picture_format: vk::Format,
        reference_format: vk::Format,
    ) -> Result<()> {
        let std_name = match self.codec {
            Codec::H264 => STD_H264_ENCODE,
            Codec::Hevc => STD_H265_ENCODE,
        };
        let std_header = vk::ExtensionProperties::default()
            .extension_name(std_name)
            .context("Video Std extension name")?
            .spec_version(STD_VERSION_1_0_0);
        let info = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(gpu.enc_family)
            // The driver may change parameter set fields; the sets come back from it anyway.
            .flags(vk::VideoSessionCreateFlagsKHR::ALLOW_ENCODE_PARAMETER_OPTIMIZATIONS)
            .video_profile(&profile.info)
            .picture_format(picture_format)
            .max_coded_extent(extent)
            .reference_picture_format(reference_format)
            .max_dpb_slots(DPB_SLOTS)
            .max_active_reference_pictures(1)
            .std_header_version(&std_header);
        let d = self.device.handle();
        let fp = self.vq.fp();
        // SAFETY: valid create info; the profile chain is wired.
        check(
            unsafe { (fp.create_video_session_khr)(d, &info, ptr::null(), &mut self.session) },
            "vkCreateVideoSessionKHR",
        )?;

        let mut n = 0u32;
        // SAFETY: the first call counts the bindings, the second fills `reqs`.
        check(
            unsafe {
                (fp.get_video_session_memory_requirements_khr)(
                    d,
                    self.session,
                    &mut n,
                    ptr::null_mut(),
                )
            },
            "vkGetVideoSessionMemoryRequirementsKHR",
        )?;
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); n as usize];
        // SAFETY: as above.
        check(
            unsafe {
                (fp.get_video_session_memory_requirements_khr)(
                    d,
                    self.session,
                    &mut n,
                    reqs.as_mut_ptr(),
                )
            },
            "vkGetVideoSessionMemoryRequirementsKHR",
        )?;
        reqs.truncate(n as usize);
        let mut binds = Vec::with_capacity(reqs.len());
        for r in &reqs {
            let m = r.memory_requirements;
            let ty = memory_type(
                &gpu.memory,
                m.memory_type_bits,
                &[vk::MemoryPropertyFlags::DEVICE_LOCAL, vk::MemoryPropertyFlags::empty()],
            )
            .context("no memory type for the video session")?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(m.size)
                .memory_type_index(ty);
            // SAFETY: valid allocate info; freed in Drop.
            let mem = unsafe { self.device.allocate_memory(&alloc, None) }
                .context("allocating video session memory")?;
            self.session_memory.push(mem);
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(r.memory_bind_index)
                    .memory(mem)
                    .memory_offset(0)
                    .memory_size(m.size),
            );
        }
        // SAFETY: one binding per requirement, each with its own allocation.
        check(
            unsafe {
                (fp.bind_video_session_memory_khr)(
                    d,
                    self.session,
                    binds.len() as u32,
                    binds.as_ptr(),
                )
            },
            "vkBindVideoSessionMemoryKHR",
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_image(
        &self,
        gpu: &Gpu,
        profile: &Profile,
        format: &vk::VideoFormatPropertiesKHR,
        extent: vk::Extent2D,
        layers: u32,
        usage: vk::ImageUsageFlags,
        view_usage: vk::ImageUsageFlags,
        families: &[u32],
    ) -> Result<Image> {
        let mut info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format.format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(layers)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(format.image_tiling)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        info = if families.len() > 1 {
            info.sharing_mode(vk::SharingMode::CONCURRENT)
                .queue_family_indices(families)
        } else {
            info.sharing_mode(vk::SharingMode::EXCLUSIVE)
        };
        info.p_next = profile.list_ptr();
        let mut img = Image::default();
        match fill_image(&self.device, gpu, &info, format.format, layers, view_usage, &mut img) {
            Ok(()) => Ok(img),
            Err(e) => {
                destroy_image(&self.device, &img);
                bail!("creating a {:?} image: {e:?}", format.format)
            }
        }
    }

    fn create_buffer(
        &self,
        gpu: &Gpu,
        profile: Option<&Profile>,
        size: u64,
        usage: vk::BufferUsageFlags,
        prefs: &[vk::MemoryPropertyFlags],
    ) -> Result<Buffer> {
        let mut info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        if let Some(p) = profile {
            info.p_next = p.list_ptr();
        }
        let mut b = Buffer {
            size,
            ..Default::default()
        };
        match fill_buffer(&self.device, gpu, &info, prefs, &mut b) {
            Ok(()) => Ok(b),
            Err(e) => {
                destroy_buffer(&self.device, &b);
                bail!("creating a {size}-byte buffer: {e:?}")
            }
        }
    }

    fn create_sync(&mut self, gpu: &Gpu, profile: &Profile) -> Result<()> {
        let d = &self.device;
        // The feedback query: where the picture starts in the bitstream buffer and its size.
        let mut feedback = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
            .encode_feedback_flags(
                vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
                    | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
            );
        feedback.p_next = profile.info_ptr();
        let mut query = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
            .query_count(1);
        query.p_next = ptr::from_ref(&feedback).cast();
        // SAFETY: valid create infos; every object is destroyed in Drop.
        unsafe {
            self.query_pool = d
                .create_query_pool(&query, None)
                .context("encode feedback query pool")?;
            let pool = |family| {
                d.create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(family),
                    None,
                )
            };
            self.enc_pool = pool(gpu.enc_family)?;
            self.xfer_pool = pool(gpu.xfer_family)?;
            let one = |pool| {
                d.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map(|v| v[0])
            };
            self.enc_cmd = one(self.enc_pool)?;
            self.xfer_cmd = one(self.xfer_pool)?;
            self.enc_fence = d.create_fence(&vk::FenceCreateInfo::default(), None)?;
            self.xfer_fence = d.create_fence(&vk::FenceCreateInfo::default(), None)?;
            self.uploaded = d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
        }
        Ok(())
    }

    /// Creates the session parameters (VPS/SPS/PPS) and fetches them as written by the driver.
    fn create_parameters(&mut self, caps: &Caps, fps: u32) -> Result<()> {
        let (w, h) = self.size;
        let rc_on = self.rc.mode != vk::VideoEncodeRateControlModeFlagsKHR::DISABLED;
        let d = self.device.handle();
        let fp = self.vq.fp();
        match self.codec {
            Codec::H264 => {
                use vk::VideoEncodeH264StdFlagsKHR as H264Std;
                let profile = self.h264_profile;
                let high = profile == sv::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH;
                let level = h264_level(w, h, fps).min(caps.max_level);

                let hrd = sv::StdVideoH264HrdParameters::zeroed();
                let mut vui = sv::StdVideoH264SequenceParameterSetVui::zeroed();
                // Full-range BT.709, as the colour conversion writes it.
                vui.flags.set_video_signal_type_present_flag(1);
                vui.flags.set_video_full_range_flag(1);
                vui.flags.set_color_description_present_flag(1);
                vui.video_format = 5;
                vui.colour_primaries = 1;
                vui.transfer_characteristics = 1;
                vui.matrix_coefficients = 1;
                // No reordering: decoders can output each frame as soon as it is decoded.
                vui.flags.set_bitstream_restriction_flag(1);
                vui.max_num_reorder_frames = 0;
                vui.max_dec_frame_buffering = 1;
                vui.pHrdParameters = &hrd;

                let scaling = sv::StdVideoH264ScalingLists::zeroed();
                let mut sps = sv::StdVideoH264SequenceParameterSet::zeroed();
                sps.flags.set_frame_mbs_only_flag(1);
                sps.flags.set_direct_8x8_inference_flag(1);
                sps.flags.set_vui_parameters_present_flag(1);
                if profile == sv::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE {
                    // Constrained Baseline.
                    sps.flags.set_constraint_set0_flag(1);
                    sps.flags.set_constraint_set1_flag(1);
                }
                sps.profile_idc = profile;
                sps.level_idc = level;
                sps.chroma_format_idc =
                    sv::StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420;
                sps.log2_max_frame_num_minus4 = LOG2_COUNTERS_MINUS4;
                // POC from frame_num: no B-frames, nothing to reorder.
                sps.pic_order_cnt_type = sv::StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_2;
                sps.log2_max_pic_order_cnt_lsb_minus4 = LOG2_COUNTERS_MINUS4;
                sps.max_num_ref_frames = 1;
                sps.pic_width_in_mbs_minus1 = w / 16 - 1;
                sps.pic_height_in_map_units_minus1 = h / 16 - 1;
                sps.pScalingLists = &scaling;
                sps.pSequenceParameterSetVui = &vui;

                let pps_scaling = sv::StdVideoH264ScalingLists::zeroed();
                let mut pps = sv::StdVideoH264PictureParameterSet::zeroed();
                let syntax = caps.h264_std;
                // CABAC where the profile has it, unless the driver only takes CAVLC.
                let baseline =
                    profile == sv::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE;
                let cabac = !baseline
                    && (syntax.contains(H264Std::ENTROPY_CODING_MODE_FLAG_SET)
                        || !syntax.contains(H264Std::ENTROPY_CODING_MODE_FLAG_UNSET));
                let t8x8 = high && syntax.contains(H264Std::TRANSFORM_8X8_MODE_FLAG_SET);
                pps.flags.set_entropy_coding_mode_flag(u32::from(cabac));
                pps.flags.set_transform_8x8_mode_flag(u32::from(t8x8));
                pps.flags.set_deblocking_filter_control_present_flag(1);
                pps.weighted_bipred_idc =
                    sv::StdVideoH264WeightedBipredIdc_STD_VIDEO_H264_WEIGHTED_BIPRED_IDC_DEFAULT;
                pps.pScalingLists = &pps_scaling;

                let add = vk::VideoEncodeH264SessionParametersAddInfoKHR::default()
                    .std_sp_ss(std::slice::from_ref(&sps))
                    .std_pp_ss(std::slice::from_ref(&pps));
                let mut codec = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default()
                    .max_std_sps_count(1)
                    .max_std_pps_count(1)
                    .parameters_add_info(&add);
                let info = vk::VideoSessionParametersCreateInfoKHR::default()
                    .video_session(self.session)
                    .push_next(&mut codec);
                // SAFETY: the parameter sets and everything they point to are live locals.
                check(
                    unsafe {
                        (fp.create_video_session_parameters_khr)(
                            d,
                            &info,
                            ptr::null(),
                            &mut self.params,
                        )
                    },
                    "vkCreateVideoSessionParametersKHR (H.264)",
                )?;
                let mut get = vk::VideoEncodeH264SessionParametersGetInfoKHR::default()
                    .write_std_sps(true)
                    .write_std_pps(true)
                    .std_sps_id(0)
                    .std_pps_id(0);
                let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
                    .video_session_parameters(self.params)
                    .push_next(&mut get);
                self.header = self.encoded_parameters(&info)?;
                debug!(level, cabac, "H.264 parameter sets");
            }
            Codec::Hevc => {
                let level = h265_level(w, h, fps).min(caps.max_level);
                let blocks = hevc_blocks(caps.ctb_sizes, caps.tb_sizes);
                self.sao = caps.h265_std.contains(
                    vk::VideoEncodeH265StdFlagsKHR::SAMPLE_ADAPTIVE_OFFSET_ENABLED_FLAG_SET,
                );

                let mut ptl = sv::StdVideoH265ProfileTierLevel::zeroed();
                ptl.flags.set_general_progressive_source_flag(1);
                ptl.flags.set_general_frame_only_constraint_flag(1);
                ptl.general_profile_idc =
                    sv::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN;
                ptl.general_level_idc = level;
                // Two pictures: the current one and its reference; nothing reordered.
                let mut dpbm = sv::StdVideoH265DecPicBufMgr::zeroed();
                dpbm.max_dec_pic_buffering_minus1[0] = 1;
                dpbm.max_num_reorder_pics[0] = 0;
                dpbm.max_latency_increase_plus1[0] = 0;

                let vps_hrd = sv::StdVideoH265HrdParameters::zeroed();
                let mut vps = sv::StdVideoH265VideoParameterSet::zeroed();
                vps.flags.set_vps_temporal_id_nesting_flag(1);
                vps.flags.set_vps_sub_layer_ordering_info_present_flag(1);
                vps.pDecPicBufMgr = &dpbm;
                vps.pHrdParameters = &vps_hrd;
                vps.pProfileTierLevel = &ptl;

                let sps_hrd = sv::StdVideoH265HrdParameters::zeroed();
                let mut vui = sv::StdVideoH265SequenceParameterSetVui::zeroed();
                vui.flags.set_video_signal_type_present_flag(1);
                vui.flags.set_video_full_range_flag(1);
                vui.flags.set_colour_description_present_flag(1);
                vui.video_format = 5;
                vui.colour_primaries = 1;
                vui.transfer_characteristics = 1;
                vui.matrix_coeffs = 1;
                vui.max_bytes_per_pic_denom = 2;
                vui.max_bits_per_min_cu_denom = 1;
                vui.pHrdParameters = &sps_hrd;

                let st_rps = sv::StdVideoH265ShortTermRefPicSet::zeroed();
                let lt_rps = sv::StdVideoH265LongTermRefPicsSps::zeroed();
                let mut sps = sv::StdVideoH265SequenceParameterSet::zeroed();
                sps.flags.set_sps_temporal_id_nesting_flag(1);
                sps.flags.set_sps_sub_layer_ordering_info_present_flag(1);
                sps.flags.set_vui_parameters_present_flag(1);
                sps.flags.set_sample_adaptive_offset_enabled_flag(u32::from(self.sao));
                sps.chroma_format_idc =
                    sv::StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_420;
                sps.pic_width_in_luma_samples = w;
                sps.pic_height_in_luma_samples = h;
                sps.log2_max_pic_order_cnt_lsb_minus4 = LOG2_COUNTERS_MINUS4;
                sps.log2_min_luma_coding_block_size_minus3 = blocks.min_cb - 3;
                sps.log2_diff_max_min_luma_coding_block_size = blocks.ctb - blocks.min_cb;
                sps.log2_min_luma_transform_block_size_minus2 = blocks.min_tb - 2;
                sps.log2_diff_max_min_luma_transform_block_size = blocks.max_tb - blocks.min_tb;
                sps.max_transform_hierarchy_depth_inter = blocks.ctb - blocks.min_tb;
                sps.max_transform_hierarchy_depth_intra = blocks.ctb - blocks.min_tb;
                sps.pProfileTierLevel = &ptl;
                sps.pDecPicBufMgr = &dpbm;
                sps.pShortTermRefPicSet = &st_rps;
                sps.pLongTermRefPicsSps = &lt_rps;
                sps.pSequenceParameterSetVui = &vui;

                let mut pps = sv::StdVideoH265PictureParameterSet::zeroed();
                // Per-CU QP lets the rate control adapt within a picture (as FFmpeg sets it).
                pps.flags.set_cu_qp_delta_enabled_flag(u32::from(rc_on));

                let add = vk::VideoEncodeH265SessionParametersAddInfoKHR {
                    std_vps_count: 1,
                    p_std_vp_ss: &vps,
                    std_sps_count: 1,
                    p_std_sp_ss: &sps,
                    std_pps_count: 1,
                    p_std_pp_ss: &pps,
                    ..Default::default()
                };
                let mut codec = vk::VideoEncodeH265SessionParametersCreateInfoKHR {
                    max_std_vps_count: 1,
                    max_std_sps_count: 1,
                    max_std_pps_count: 1,
                    p_parameters_add_info: &add,
                    ..Default::default()
                };
                let info = vk::VideoSessionParametersCreateInfoKHR::default()
                    .video_session(self.session)
                    .push_next(&mut codec);
                // SAFETY: the parameter sets and everything they point to are live locals.
                check(
                    unsafe {
                        (fp.create_video_session_parameters_khr)(
                            d,
                            &info,
                            ptr::null(),
                            &mut self.params,
                        )
                    },
                    "vkCreateVideoSessionParametersKHR (HEVC)",
                )?;
                let mut get = vk::VideoEncodeH265SessionParametersGetInfoKHR {
                    write_std_vps: vk::TRUE,
                    write_std_sps: vk::TRUE,
                    write_std_pps: vk::TRUE,
                    ..Default::default()
                };
                let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
                    .video_session_parameters(self.params)
                    .push_next(&mut get);
                self.header = self.encoded_parameters(&info)?;
                debug!(level, ?blocks, sao = self.sao, "HEVC parameter sets");
            }
        }
        if !annex_b(&self.header) {
            warn!(
                head = ?&self.header[..self.header.len().min(8)],
                "The driver's parameter sets do not start with a start code"
            );
        }
        Ok(())
    }

    /// `vkGetEncodedVideoSessionParametersKHR`: size, then the bytes.
    fn encoded_parameters(
        &self,
        info: &vk::VideoEncodeSessionParametersGetInfoKHR,
    ) -> Result<Vec<u8>> {
        let d = self.device.handle();
        let f = self.veq.fp().get_encoded_video_session_parameters_khr;
        let mut feedback = vk::VideoEncodeSessionParametersFeedbackInfoKHR::default();
        let mut size = 0usize;
        // SAFETY: the first call only reports the size.
        check(
            unsafe { f(d, info, &mut feedback, &mut size, ptr::null_mut()) },
            "vkGetEncodedVideoSessionParametersKHR",
        )?;
        let mut data = vec![0u8; size];
        // SAFETY: `data` holds `size` bytes.
        check(
            unsafe { f(d, info, &mut feedback, &mut size, data.as_mut_ptr().cast()) },
            "vkGetEncodedVideoSessionParametersKHR",
        )?;
        data.truncate(size);
        if feedback.has_overrides == vk::TRUE {
            debug!("The driver changed some parameter set fields");
        }
        Ok(data)
    }

    fn set_max_bitrate(&mut self, bps: u32) {
        // Applied by a control command before the next picture, without an IDR.
        self.rc.max_bitrate = u64::from(bps).min(self.max_bitrate).max(1);
    }

    /// Encodes one NV12 frame of the session's size.
    fn encode(&mut self, frame: &Nv12Frame, force_idr: bool) -> Result<EncodedFrame> {
        let (w, h) = self.size;
        if (frame.width, frame.height) != (w, h) {
            bail!("frame is {}x{}, session {w}x{h}", frame.width, frame.height);
        }
        let plan = self.gop.plan(force_idr);
        self.record_upload(frame)?;
        self.record_encode(&plan)?;
        self.submit()?;
        let mut data = Vec::new();
        if plan.idr {
            data.extend_from_slice(&self.header);
        }
        self.read_bitstream(&mut data)?;
        self.gop.commit(&plan);
        Ok(EncodedFrame {
            data,
            keyframe: plan.idr,
            ..Default::default()
        })
    }

    /// Copies the frame into the staging buffer and records its copy into the source image.
    fn record_upload(&self, frame: &Nv12Frame) -> Result<()> {
        let (w, h) = self.size;
        let len = frame.pitch * h as usize * 3 / 2;
        if len as u64 > self.staging.size || frame.data.len() < len {
            bail!("NV12 frame of {len} bytes does not fit the staging buffer");
        }
        let d = &self.device;
        // SAFETY: the mapping covers `staging.size` bytes and the frame `len`; nothing on the
        // GPU uses the staging buffer (the previous frame was waited for).
        unsafe {
            ptr::copy_nonoverlapping(frame.data.as_ptr(), self.staging.ptr, len);
            if !self.staging.coherent {
                d.flush_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                    .memory(self.staging.memory)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)])?;
            }
        }
        let cb = self.xfer_cmd;
        let pitch = frame.pitch as u32;
        let to_dst = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            // The previous contents go: every coded pixel is written.
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.src.image)
            .subresource_range(color_range(1))];
        let plane = |aspect, offset: u64, row: u32, width: u32, height: u32| {
            vk::BufferImageCopy::default()
                .buffer_offset(offset)
                .buffer_row_length(row)
                .buffer_image_height(height)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(aspect)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D::default())
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
        };
        // The chroma plane is R8G8: row length and extent in two-byte texels.
        let regions = [
            plane(vk::ImageAspectFlags::PLANE_0, 0, pitch, w, h),
            plane(
                vk::ImageAspectFlags::PLANE_1,
                u64::from(pitch) * u64::from(h),
                pitch / 2,
                w / 2,
                h / 2,
            ),
        ];
        // SAFETY: the command buffer is not in use (its last submission was waited for).
        unsafe {
            d.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            d.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&to_dst),
            );
            d.cmd_copy_buffer_to_image(
                cb,
                self.staging.buffer,
                self.src.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
            d.end_command_buffer(cb)?;
        }
        Ok(())
    }

    fn record_encode(&mut self, plan: &Plan) -> Result<()> {
        let (w, h) = self.size;
        let extent = vk::Extent2D {
            width: w,
            height: h,
        };
        let d = &self.device;
        let cb = self.enc_cmd;

        let mut images = vec![
            vk::ImageMemoryBarrier2::default()
                // After the semaphore wait (all commands): the copy's writes are available.
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.src.image)
                .subresource_range(color_range(1)),
        ];
        if !self.dpb_ready {
            images.push(
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                    .dst_access_mask(
                        vk::AccessFlags2::VIDEO_ENCODE_READ_KHR
                            | vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR,
                    )
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(self.dpb.image)
                    .subresource_range(color_range(vk::REMAINING_ARRAY_LAYERS)),
            );
        }
        // The previous picture's reconstruction (last submission) before this one reads it.
        let memory = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .src_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .dst_access_mask(
                vk::AccessFlags2::VIDEO_ENCODE_READ_KHR | vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR,
            )];
        let to_host = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .src_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
            .dst_stage_mask(vk::PipelineStageFlags2::HOST)
            .dst_access_mask(vk::AccessFlags2::HOST_READ)];

        let rc_off = self.rc.mode == vk::VideoEncodeRateControlModeFlagsKHR::DISABLED;
        let mut picture = Picture::new(self.codec, plan, rc_off.then_some(self.qp), self.sao);
        picture.wire();

        let src = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D::default())
            .coded_extent(extent)
            .base_array_layer(0)
            .image_view_binding(self.src.view);
        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D::default())
            .coded_extent(extent)
            .base_array_layer(u32::from(plan.setup_slot))
            .image_view_binding(self.dpb.view);
        let mut setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(i32::from(plan.setup_slot))
            .picture_resource(&setup_res);
        setup.p_next = picture.setup();
        let ref_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D::default())
            .coded_extent(extent)
            .base_array_layer(plan.reference.map_or(0, |r| u32::from(r.slot)))
            .image_view_binding(self.dpb.view);
        let mut reference = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(plan.reference.map_or(-1, |r| i32::from(r.slot)))
            .picture_resource(&ref_res);
        reference.p_next = picture.reference();
        let refs = if plan.reference.is_some() {
            std::slice::from_ref(&reference)
        } else {
            &[]
        };
        // The coding scope names the reference and, inactive (-1), the slot being written.
        let mut setup_begin = setup;
        setup_begin.slot_index = -1;
        let begin_slots: Vec<_> = refs.iter().copied().chain([setup_begin]).collect();

        let mut begin_rc = self.rc_applied.map(|rc| RcChain::new(self.codec, rc));
        if let Some(c) = begin_rc.as_mut() {
            c.wire();
        }
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(&begin_slots);
        if let Some(c) = &begin_rc {
            begin.p_next = c.head();
        }
        let mut control_rc =
            (self.rc_applied != Some(self.rc)).then(|| RcChain::new(self.codec, self.rc));
        if let Some(c) = control_rc.as_mut() {
            c.wire();
        }
        let mut control_flags = vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL;
        if self.rc_applied.is_none() {
            control_flags |= vk::VideoCodingControlFlagsKHR::RESET;
        }
        let mut control = vk::VideoCodingControlInfoKHR::default().flags(control_flags);
        if let Some(c) = &control_rc {
            control.p_next = c.head();
        }

        let mut encode = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(self.bitstream.buffer)
            .dst_buffer_offset(0)
            .dst_buffer_range(self.bitstream.size)
            .src_picture_resource(src)
            .setup_reference_slot(&setup)
            .reference_slots(refs);
        encode.p_next = picture.info();

        let vq = self.vq.fp();
        let veq = self.veq.fp();
        // SAFETY: the command buffer is not in use; every structure the commands read lives
        // until the end of this function (commands consume their parameters when recorded).
        unsafe {
            d.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            d.cmd_reset_query_pool(cb, self.query_pool, 0, 1);
            d.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default()
                    .memory_barriers(&memory)
                    .image_memory_barriers(&images),
            );
            (vq.cmd_begin_video_coding_khr)(cb, &begin);
            if control_rc.is_some() {
                (vq.cmd_control_video_coding_khr)(cb, &control);
            }
            d.cmd_begin_query(cb, self.query_pool, 0, vk::QueryControlFlags::empty());
            (veq.cmd_encode_video_khr)(cb, &encode);
            d.cmd_end_query(cb, self.query_pool, 0);
            (vq.cmd_end_video_coding_khr)(cb, &vk::VideoEndCodingInfoKHR::default());
            d.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().memory_barriers(&to_host));
            d.end_command_buffer(cb)?;
        }
        // From here the session is in the new state (if the submission fails, the session is
        // dropped and a new one starts from a reset).
        self.rc_applied = Some(self.rc);
        self.dpb_ready = true;
        Ok(())
    }

    /// Submits the upload and the encode and waits for both.
    fn submit(&self) -> Result<()> {
        let d = &self.device;
        let signal = [vk::SemaphoreSubmitInfo::default()
            .semaphore(self.uploaded)
            .stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)];
        let xfer_cmd = [vk::CommandBufferSubmitInfo::default().command_buffer(self.xfer_cmd)];
        let wait = [vk::SemaphoreSubmitInfo::default()
            .semaphore(self.uploaded)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let enc_cmd = [vk::CommandBufferSubmitInfo::default().command_buffer(self.enc_cmd)];
        // SAFETY: both command buffers are recorded; the fences are unsignalled; the semaphore
        // is signalled once and waited once.
        unsafe {
            d.queue_submit2(
                self.xfer_queue,
                &[vk::SubmitInfo2::default()
                    .command_buffer_infos(&xfer_cmd)
                    .signal_semaphore_infos(&signal)],
                self.xfer_fence,
            )
            .context("submitting the upload")?;
            d.queue_submit2(
                self.enc_queue,
                &[vk::SubmitInfo2::default()
                    .wait_semaphore_infos(&wait)
                    .command_buffer_infos(&enc_cmd)],
                self.enc_fence,
            )
            .context("submitting the encode")?;
            let fences = [self.xfer_fence, self.enc_fence];
            d.wait_for_fences(&fences, true, FENCE_TIMEOUT_NS)
                .context("waiting for the encode")?;
            d.reset_fences(&fences)?;
        }
        Ok(())
    }

    /// Appends the picture the last encode wrote.
    fn read_bitstream(&self, out: &mut Vec<u8>) -> Result<()> {
        let d = &self.device;
        let flags = vk::QueryResultFlags::WAIT;
        // SAFETY: the query was ended in the submission just waited for; each result is two
        // (or with status three) 32-bit values.
        let (offset, bytes) = unsafe {
            if self.status_queries {
                let mut r = [[0u32; 3]; 1];
                d.get_query_pool_results(
                    self.query_pool,
                    0,
                    &mut r,
                    flags | vk::QueryResultFlags::WITH_STATUS_KHR,
                )
                .context("encode feedback")?;
                let status = r[0][2] as i32;
                if status != vk::QueryResultStatusKHR::COMPLETE.as_raw() {
                    bail!("the encode failed (query status {status})");
                }
                (r[0][0], r[0][1])
            } else {
                let mut r = [[0u32; 2]; 1];
                d.get_query_pool_results(self.query_pool, 0, &mut r, flags)
                    .context("encode feedback")?;
                (r[0][0], r[0][1])
            }
        };
        let (offset, bytes) = (offset as usize, bytes as usize);
        if bytes == 0 || (offset + bytes) as u64 > self.bitstream.size {
            bail!("encode feedback out of range: {bytes} bytes at {offset}");
        }
        // SAFETY: the mapping covers the buffer; the range was checked above; the GPU is done
        // with it (fence and host barrier).
        unsafe {
            if !self.bitstream.coherent {
                d.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                    .memory(self.bitstream.memory)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)])?;
            }
            out.extend_from_slice(std::slice::from_raw_parts(
                self.bitstream.ptr.add(offset),
                bytes,
            ));
        }
        Ok(())
    }
}

/// Creates the image of `info`, its memory and a view of every layer into `img` (what was
/// created stays there on failure, for [`destroy_image`]).
fn fill_image(
    d: &ash::Device,
    gpu: &Gpu,
    info: &vk::ImageCreateInfo,
    format: vk::Format,
    layers: u32,
    view_usage: vk::ImageUsageFlags,
    img: &mut Image,
) -> ash::prelude::VkResult<()> {
    // SAFETY: `info` is a valid create info (with the profile list wired); every handle is
    // created on `d` and handed to the caller.
    unsafe {
        img.image = d.create_image(info, None)?;
        let req = d.get_image_memory_requirements(img.image);
        let ty = memory_type(
            &gpu.memory,
            req.memory_type_bits,
            &[vk::MemoryPropertyFlags::DEVICE_LOCAL, vk::MemoryPropertyFlags::empty()],
        )
        .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
        img.memory = d.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty),
            None,
        )?;
        d.bind_image_memory(img.image, img.memory, 0)?;
        let mut usage = vk::ImageViewUsageCreateInfo::default().usage(view_usage);
        let view = vk::ImageViewCreateInfo::default()
            .image(img.image)
            .view_type(if layers > 1 {
                vk::ImageViewType::TYPE_2D_ARRAY
            } else {
                vk::ImageViewType::TYPE_2D
            })
            .format(format)
            .subresource_range(color_range(layers))
            .push_next(&mut usage);
        img.view = d.create_image_view(&view, None)?;
    }
    Ok(())
}

/// Creates the buffer of `info` with host-visible memory, mapped, into `b` (what was created
/// stays there on failure, for [`destroy_buffer`]).
fn fill_buffer(
    d: &ash::Device,
    gpu: &Gpu,
    info: &vk::BufferCreateInfo,
    prefs: &[vk::MemoryPropertyFlags],
    b: &mut Buffer,
) -> ash::prelude::VkResult<()> {
    // SAFETY: `info` is a valid create info; every handle is created on `d` and handed to the
    // caller; the whole allocation is mapped once.
    unsafe {
        b.buffer = d.create_buffer(info, None)?;
        let req = d.get_buffer_memory_requirements(b.buffer);
        let ty = memory_type(&gpu.memory, req.memory_type_bits, prefs)
            .ok_or(vk::Result::ERROR_OUT_OF_HOST_MEMORY)?;
        b.coherent = gpu.memory.memory_types[ty as usize]
            .property_flags
            .contains(vk::MemoryPropertyFlags::HOST_COHERENT);
        b.memory = d.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty),
            None,
        )?;
        d.bind_buffer_memory(b.buffer, b.memory, 0)?;
        b.ptr = d
            .map_memory(b.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?
            .cast();
    }
    Ok(())
}

fn color_range(layers: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(layers)
}

fn destroy_image(d: &ash::Device, img: &Image) {
    // SAFETY: the handles were created on `d` (or are null) and are not in use.
    unsafe {
        d.destroy_image_view(img.view, None);
        d.destroy_image(img.image, None);
        d.free_memory(img.memory, None);
    }
}

fn destroy_buffer(d: &ash::Device, b: &Buffer) {
    // SAFETY: as above; the memory is unmapped before it is freed.
    unsafe {
        if !b.ptr.is_null() {
            d.unmap_memory(b.memory);
        }
        d.destroy_buffer(b.buffer, None);
        d.free_memory(b.memory, None);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let d = &self.device;
        // SAFETY: after the device is idle nothing is in use; every handle was created on
        // this device or is null, and each is destroyed once.
        unsafe {
            let _ = d.device_wait_idle();
            let vq = self.vq.fp();
            (vq.destroy_video_session_parameters_khr)(d.handle(), self.params, ptr::null());
            (vq.destroy_video_session_khr)(d.handle(), self.session, ptr::null());
            for m in self.session_memory.drain(..) {
                d.free_memory(m, None);
            }
            d.destroy_semaphore(self.uploaded, None);
            d.destroy_fence(self.enc_fence, None);
            d.destroy_fence(self.xfer_fence, None);
            // Destroying the pools frees their command buffers.
            d.destroy_command_pool(self.enc_pool, None);
            d.destroy_command_pool(self.xfer_pool, None);
            d.destroy_query_pool(self.query_pool, None);
        }
        destroy_buffer(d, &self.bitstream);
        destroy_buffer(d, &self.staging);
        destroy_image(d, &self.dpb);
        destroy_image(d, &self.src);
    }
}

// ---------------------------------------------------------------------------------------
// Backend

/// The Vulkan Video backend: one device, and a session per codec and size.
pub struct Vulkan {
    // Field order matters: the session's objects belong to `gpu`'s device, so it drops
    // first.
    session: Option<Session>,
    nv12: Option<Nv12Frame>,
    gpu: Gpu,
    fps: u32,
    bitrate: u32,
    force_idr: bool,
}

// The Vulkan handles are only used from the thread that owns the encoder.
unsafe impl Send for Vulkan {}

impl Vulkan {
    /// Opens the `gpu`-th device that can encode H.264 through Vulkan Video and encodes a
    /// few small frames, so a driver that lists the extensions but cannot encode fails here.
    pub fn new(gpu: usize, fps: u32, bitrate: u32) -> Result<Self> {
        let gpu = Gpu::open(gpu)?;
        let mut this = Self {
            session: None,
            nv12: None,
            gpu,
            fps,
            bitrate,
            force_idr: true,
        };
        this.trial(Codec::H264)
            .context("Vulkan Video H.264 test frames")?;
        if this.gpu.hevc
            && let Err(e) = this.trial(Codec::Hevc)
        {
            info!(error = format!("{e:#}"), "No HEVC from Vulkan Video");
            this.gpu.hevc = false;
        }
        info!(
            device = %this.gpu.name,
            hevc = this.gpu.hevc,
            "Vulkan Video encoder ready"
        );
        Ok(this)
    }

    fn trial(&self, codec: Codec) -> Result<()> {
        let (w, h) = (256, 256);
        let frame = Nv12Frame::new(w, h);
        let mut s = Session::new(&self.gpu, codec, w, h, self.fps, self.bitrate)?;
        for i in 0..3 {
            let out = s.encode(&frame, i == 0)?;
            if out.data.is_empty() {
                bail!("empty picture {i}");
            }
        }
        Ok(())
    }
}

impl Backend for Vulkan {
    fn name(&self) -> &'static str {
        "vulkan"
    }

    fn supports_avc444(&self) -> bool {
        false
    }

    fn supports_hevc(&self) -> bool {
        self.gpu.hevc
    }

    fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    fn set_max_bitrate(&mut self, bps: u32) -> Result<()> {
        self.bitrate = bps;
        if let Some(s) = self.session.as_mut() {
            s.set_max_bitrate(bps);
        }
        Ok(())
    }

    /// Takes frames in system memory only: it asks for no VA surfaces or D3D11 textures
    /// (`vaapi_import`, `d3d11_input`) and no CUDA context, so the capture sends `Cpu`; any
    /// other input is an error. `yuv444` and `region_quality` are not offered (4:2:0, one QP
    /// for the picture), and `const_qp` is not honoured (see the trait).
    fn encode(&mut self, frame: &Frame, req: &Request) -> Result<(EncodedFrame, EncodedFrame)> {
        let FrameData::Cpu { data, stride } = &frame.data else {
            bail!("the Vulkan backend takes frames in system memory");
        };
        if req.codec == Codec::Hevc && !self.gpu.hevc {
            bail!("this Vulkan device does not encode HEVC");
        }
        let (cw, ch) = color::coded_size(frame.width, frame.height);
        if self
            .nv12
            .as_ref()
            .is_none_or(|m| (m.width, m.height) != (cw, ch))
        {
            self.nv12 = Some(Nv12Frame::new(cw, ch));
        }
        if self
            .session
            .as_ref()
            .is_none_or(|s| s.codec != req.codec || s.size != (cw, ch))
        {
            // A new size or codec: a new session (and an IDR).
            self.session = None;
            self.session = Some(Session::new(
                &self.gpu,
                req.codec,
                cw,
                ch,
                self.fps,
                self.bitrate,
            )?);
            self.force_idr = true;
        }
        if !req.main {
            return Ok(Default::default());
        }
        let nv12 = self.nv12.as_mut().expect("allocated above");
        let t = std::time::Instant::now();
        csc::convert(data, *stride as usize, frame.width, frame.height, nv12, None);
        let csc_us = t.elapsed().as_micros();
        let session = self.session.as_mut().expect("created above");
        match session.encode(nv12, self.force_idr) {
            Ok(out) => {
                self.force_idr = false;
                tracing::debug!(
                    csc_us,
                    encode_us = t.elapsed().as_micros() - csc_us,
                    bytes = out.data.len(),
                    "Vulkan frame"
                );
                Ok((out, EncodedFrame::default()))
            }
            Err(e) => {
                // The session may be in any state: start over with the next frame.
                self.session = None;
                self.force_idr = true;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NAL unit types of an Annex B stream.
    fn nal_types(data: &[u8], codec: Codec) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let b = data[i + 3];
                out.push(match codec {
                    Codec::H264 => b & 0x1f,
                    Codec::Hevc => (b >> 1) & 0x3f,
                });
                i += 3;
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn nal_types_of_annex_b() {
        let h264 = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9];
        assert_eq!(nal_types(&h264, Codec::H264), [7, 8, 5]);
        let hevc = [0, 0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 1, 0, 0, 1, 0x44, 1, 0, 0, 1, 0x26, 1];
        assert_eq!(nal_types(&hevc, Codec::Hevc), [32, 33, 34, 19]);
        assert!(annex_b(&h264));
        assert!(!annex_b(&[0x67, 0, 0, 1]));
    }

    #[test]
    fn gop_alternates_slots_and_restarts_on_idr() {
        let mut g = Gop::new(Codec::H264);
        let p0 = g.plan(false);
        assert!(p0.idr && p0.reference.is_none());
        assert_eq!((p0.setup_slot, p0.frame_num, p0.poc, p0.idr_pic_id), (0, 0, 0, 0));
        g.commit(&p0);
        let p1 = g.plan(false);
        assert!(!p1.idr);
        assert_eq!(p1.reference.map(|r| (r.slot, r.idr)), Some((0, true)));
        assert_eq!((p1.setup_slot, p1.frame_num, p1.poc), (1, 1, 2));
        g.commit(&p1);
        let p2 = g.plan(false);
        assert_eq!((p2.setup_slot, p2.frame_num, p2.poc), (0, 2, 4));
        assert_eq!(p2.reference.map(|r| r.slot), Some(1));
        g.commit(&p2);
        // An IDR on request: no reference, counters back to 0, a new idr_pic_id.
        let p3 = g.plan(true);
        assert!(p3.idr && p3.reference.is_none());
        assert_eq!((p3.frame_num, p3.poc, p3.idr_pic_id), (0, 0, 1));
        g.commit(&p3);
        assert_eq!(g.plan(false).frame_num, 1);
    }

    #[test]
    fn frame_num_wraps_and_hevc_poc_counts_by_one() {
        let mut g = Gop::new(Codec::Hevc);
        g.commit(&g.plan(false));
        assert_eq!(g.plan(false).poc, 1);
        g.since_idr = (1 << 16) - 1;
        let p = g.plan(false);
        assert_eq!(p.frame_num, 0);
        assert_eq!(p.poc, 1 << 16);
    }

    #[test]
    fn levels() {
        // 1080p60 H.264: 8160 macroblocks at 489600/s, level 4.2.
        assert_eq!(h264_level(1920, 1088, 60), 12);
        // 1080p30 would be 4.0; never below 4.1.
        assert_eq!(h264_level(1920, 1088, 30), MIN_LEVEL_H264);
        assert_eq!(h264_level(3840, 2160, 60), 15);
        assert_eq!(h265_level(1920, 1088, 60), 6);
        assert_eq!(h265_level(3840, 2160, 60), 8);
        assert_eq!(h265_level(640, 480, 30), MIN_LEVEL_H265);
        // Beyond every level: the highest.
        assert_eq!(h264_level(16384, 16384, 240), 18);
    }

    #[test]
    fn hevc_block_sizes_follow_the_encoder() {
        use vk::VideoEncodeH265CtbSizeFlagsKHR as C;
        use vk::VideoEncodeH265TransformBlockSizeFlagsKHR as T;
        let all_tb = T::TYPE_4 | T::TYPE_8 | T::TYPE_16 | T::TYPE_32;
        assert_eq!(
            hevc_blocks(C::TYPE_32 | C::TYPE_64, all_tb),
            HevcBlocks {
                ctb: 6,
                min_cb: 4,
                min_tb: 2,
                max_tb: 5
            }
        );
        assert_eq!(
            hevc_blocks(C::TYPE_16, T::TYPE_4 | T::TYPE_8 | T::TYPE_16),
            HevcBlocks {
                ctb: 4,
                min_cb: 3,
                min_tb: 2,
                max_tb: 4
            }
        );
        // Nothing reported: 32×32 CTBs are not assumed, transforms 4 to 16 within a 16 CTB.
        let b = hevc_blocks(C::empty(), T::empty());
        assert_eq!((b.ctb, b.min_cb, b.min_tb, b.max_tb), (4, 3, 2, 4));
        // A minimum transform of 16 must still be below the minimum CU (16): 8.
        assert_eq!(hevc_blocks(C::TYPE_32, T::TYPE_16 | T::TYPE_32).min_tb, 3);
    }

    #[test]
    fn memory_type_preferences() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        props.memory_types[0].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[1].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[2].property_flags = vk::MemoryPropertyFlags::HOST_VISIBLE
            | vk::MemoryPropertyFlags::HOST_COHERENT
            | vk::MemoryPropertyFlags::HOST_CACHED;
        let cached = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED;
        let visible = vk::MemoryPropertyFlags::HOST_VISIBLE;
        assert_eq!(memory_type(&props, 0b111, &[cached, visible]), Some(2));
        assert_eq!(memory_type(&props, 0b011, &[cached, visible]), Some(1));
        assert_eq!(memory_type(&props, 0b001, &[cached, visible]), None);
        assert_eq!(
            memory_type(&props, 0b001, &[vk::MemoryPropertyFlags::empty()]),
            Some(0)
        );
        assert_eq!(align_up(1000, 256), 1024);
        assert_eq!(align_up(1000, 0), 1000);
    }

    /// Encodes synthetic frames with the first Vulkan Video device:
    /// `cargo test -p glidedesk-host --features vulkan-video vulkan_encodes_frames -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn vulkan_encodes_frames() {
        let mut backend = Vulkan::new(0, 60, 20_000_000).expect("opening the Vulkan encoder");
        for codec in [Codec::H264, Codec::Hevc] {
            if codec == Codec::Hevc && !backend.supports_hevc() {
                println!("{codec:?}: not offered");
                continue;
            }
            // The second size checks that a resize opens a new session with an IDR.
            for (w, h) in [(1280u32, 720u32), (1920, 1080)] {
                backend.request_keyframe();
                let start = std::time::Instant::now();
                let mut bytes = 0;
                for i in 0..30u32 {
                    if i == 15 {
                        backend.set_max_bitrate(8_000_000).expect("bitrate");
                    }
                    // A gradient that moves: every frame differs.
                    let mut data = vec![0u8; (w * h * 4) as usize];
                    for (p, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let (x, y) = (p as u32 % w, p as u32 / w);
                        *px = [(x + i * 8) as u8, (y + i * 4) as u8, (x ^ y) as u8, 0];
                    }
                    let frame = Frame {
                        width: w,
                        height: h,
                        data: FrameData::Cpu {
                            data,
                            stride: w * 4,
                        },
                        damage: None,
                        captured: std::time::Instant::now(),
                        recycle: None,
                    };
                    let req = Request {
                        codec,
                        avc444: false,
                        main: true,
                        aux: false,
                        const_qp: None,
                        yuv444: false,
                        region_quality: false,
                    };
                    let (main, aux) = backend.encode(&frame, &req).expect("encoding");
                    assert!(aux.data.is_empty());
                    assert!(annex_b(&main.data), "{codec:?}: picture {i} not Annex B");
                    let types = nal_types(&main.data, codec);
                    let (params, idr): (&[u8], &[u8]) = match codec {
                        Codec::H264 => (&[7, 8], &[5]),
                        Codec::Hevc => (&[32, 33, 34], &[19, 20]),
                    };
                    let has_params = params.iter().all(|t| types.contains(t));
                    let has_idr = types.iter().any(|t| idr.contains(t));
                    if i == 0 {
                        assert!(main.keyframe, "{codec:?} {w}x{h}: first picture not a keyframe");
                        assert!(has_params, "{codec:?}: IDR without parameter sets: {types:?}");
                        assert!(has_idr, "{codec:?}: no IDR slice: {types:?}");
                    } else {
                        assert!(!main.keyframe, "{codec:?}: picture {i} is a keyframe");
                        assert!(!has_params && !has_idr, "{codec:?}: picture {i}: {types:?}");
                    }
                    bytes += main.data.len();
                }
                println!(
                    "{codec:?} {w}x{h}: 30 frames in {:.0} ms (with the CPU colour conversion), {} kB",
                    start.elapsed().as_secs_f64() * 1000.0,
                    bytes / 1000
                );
            }
        }
    }
}

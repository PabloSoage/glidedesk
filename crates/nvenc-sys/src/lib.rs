//! Raw NVENC bindings plus the GUIDs and struct versions that bindgen cannot expand
//! (they are `static const` initialisers and function-like macros in the header).
#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    }
}

pub const NV_ENC_CODEC_H264_GUID: GUID = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
pub const NV_ENC_CODEC_HEVC_GUID: GUID = guid(
    0x790cdc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
pub const NV_ENC_CODEC_AV1_GUID: GUID = guid(
    0x0a352289,
    0x0aa7,
    0x4759,
    [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);
pub const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = guid(
    0xe7cbc309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
pub const NV_ENC_H264_PROFILE_HIGH_444_GUID: GUID = guid(
    0x7ac663cb,
    0xa598,
    0x4960,
    [0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x52],
);
pub const NV_ENC_HEVC_PROFILE_MAIN_GUID: GUID = guid(
    0xb514c39a,
    0xb55b,
    0x40fa,
    [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
pub const NV_ENC_HEVC_PROFILE_FREXT_GUID: GUID = guid(
    0x51ec32b5,
    0x1b4c,
    0x453c,
    [0x9c, 0xbd, 0xb6, 0x16, 0xbd, 0x62, 0x13, 0x41],
);
pub const NV_ENC_PRESET_P1_GUID: GUID = guid(
    0xfc0a8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);
pub const NV_ENC_PRESET_P2_GUID: GUID = guid(
    0xf581cfb8,
    0x88d6,
    0x4381,
    [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab],
);
pub const NV_ENC_PRESET_P3_GUID: GUID = guid(
    0x36850110,
    0x3a07,
    0x441f,
    [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
);
pub const NV_ENC_PRESET_P4_GUID: GUID = guid(
    0x90a7b826,
    0xdf06,
    0x4862,
    [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
);
pub const NV_ENC_PRESET_P5_GUID: GUID = guid(
    0x21c6e6b4,
    0x297a,
    0x4cba,
    [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3],
);
pub const NV_ENC_PRESET_P6_GUID: GUID = guid(
    0x8e75c279,
    0x6299,
    0x4ab6,
    [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5],
);
pub const NV_ENC_PRESET_P7_GUID: GUID = guid(
    0x84848c12,
    0x6f71,
    0x4c13,
    [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74],
);

pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}
pub const NV_ENC_CAPS_PARAM_VER: u32 = struct_version(1);
pub const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = struct_version(2);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_CONFIG_VER: u32 = struct_version(9) | (1 << 31);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(2) | (1 << 31);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(5) | (1 << 31);
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2) | (1 << 31);
pub const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(5);
pub const NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER: u32 = struct_version(1);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);

/// Signature of `NvEncodeAPICreateInstance`, resolved from `libnvidia-encode.so.1`.
pub type PFN_NvEncodeAPICreateInstance =
    unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

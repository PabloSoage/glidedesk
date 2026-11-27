//! A virtual Sony DualSense for a Moonlight controller with motion sensors, made through Linux
//! UHID (`/dev/uhid`), so the client's gyroscope and accelerometer reach games.
//!
//! The kernel's `hid-playstation` driver binds to the device as to a USB DualSense and makes
//! three evdev devices (gamepad, motion sensors, touchpad) whose `uniq` is the MAC address the
//! pad reports in its pairing feature report: SDL and Steam pair a pad with its sensors by that
//! shared `uniq`, which uinput devices cannot set. The device's hidraw node also serves SDL's
//! HIDAPI driver, which reads the same reports.
//!
//! The pad answers the feature reports the driver (and SDL) read while probing: calibration
//! 0x05, pairing info 0x09 (with a MAC per pad) and firmware info 0x20. The client's state and
//! motion go into USB input report 0x01; the motor bytes of output report 0x02 go back to the
//! client as rumble. The lightbar, player LEDs and trigger effects are ignored; the touchpad
//! reports no touches.
//!
//! Adapted from [inputtino](https://github.com/games-on-whales/inputtino) (MIT, commit
//! `d28ec79`): `src/uhid/include/uhid/ps5.hpp` (the USB report descriptor, which inputtino took
//! from nondebug/dualsense, and the calibration, pairing and firmware feature reports, copied
//! byte for byte; the report layouts), `src/uhid/joypad_ps5.cpp` (buttons, sticks, triggers,
//! motion units, sequence number, timestamp, battery, rumble) and
//! `src/uhid/include/uhid/uhid.hpp` (the UHID event loop). Report layouts and the calibration
//! math also follow Linux `drivers/hid/hid-playstation.c`; event layouts follow Linux
//! `include/uapi/linux/uhid.h`. Button bits and motion units: moonlight-common-c
//! `src/Limelight.h` (`b4ee1ce`).

// Parts of this file come from inputtino:
//
// Copyright (c) 2021-2024 Games on Whales https://github.com/games-on-whales/
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::debug;

use super::gamepad::{MOTION_ACCEL, MOTION_GYRO, Pad, PadState, Rumble, button};

/// `include/uapi/linux/uhid.h`: event types and byte offsets in `struct uhid_event`, which is
/// packed and native endian. Offsets count from the start of the event, whose first 4 bytes
/// are the `__u32 type`.
mod uhid {
    pub const DESTROY: u32 = 1;
    pub const OUTPUT: u32 = 6;
    pub const GET_REPORT: u32 = 9;
    pub const GET_REPORT_REPLY: u32 = 10;
    pub const CREATE2: u32 = 11;
    pub const INPUT2: u32 = 12;
    pub const SET_REPORT: u32 = 13;
    pub const SET_REPORT_REPLY: u32 = 14;

    /// `UHID_DATA_MAX`, also `HID_MAX_DESCRIPTOR_SIZE` (the size of `rd_data`).
    pub const DATA_MAX: usize = 4096;
    /// `sizeof(struct uhid_event)`: the type and the largest member, `uhid_create2_req`
    /// (name 128, phys 64, uniq 64, rd_size 2, bus 2, vendor, product, version and country 4
    /// each, rd_data 4096). The kernel returns at most this much per read.
    pub const EVENT_SIZE: usize = 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4 + DATA_MAX;

    // uhid_create2_req { u8 name[128]; u8 phys[64]; u8 uniq[64]; u16 rd_size; u16 bus;
    // u32 vendor; u32 product; u32 version; u32 country; u8 rd_data[4096]; }
    pub const CREATE2_NAME: usize = 4;
    pub const CREATE2_PHYS: usize = 132;
    pub const CREATE2_UNIQ: usize = 196;
    /// Room for each of name, phys and uniq, NUL included.
    pub const CREATE2_NAME_LEN: usize = 128;
    pub const CREATE2_PHYS_LEN: usize = 64;
    pub const CREATE2_UNIQ_LEN: usize = 64;
    pub const CREATE2_RD_SIZE: usize = 260;
    pub const CREATE2_BUS: usize = 262;
    pub const CREATE2_VENDOR: usize = 264;
    pub const CREATE2_PRODUCT: usize = 268;
    pub const CREATE2_VERSION: usize = 272;
    pub const CREATE2_COUNTRY: usize = 276;
    pub const CREATE2_RD_DATA: usize = 280;

    // uhid_input2_req { u16 size; u8 data[4096]; }
    pub const INPUT2_SIZE: usize = 4;
    pub const INPUT2_DATA: usize = 6;

    // uhid_output_req { u8 data[4096]; u16 size; u8 rtype; }
    pub const OUTPUT_DATA: usize = 4;
    pub const OUTPUT_SIZE: usize = 4 + DATA_MAX;

    // uhid_get_report_req { u32 id; u8 rnum; u8 rtype; }
    pub const GET_REPORT_ID: usize = 4;
    pub const GET_REPORT_RNUM: usize = 8;

    // uhid_get_report_reply_req { u32 id; u16 err; u16 size; u8 data[4096]; }
    pub const GET_REPORT_REPLY_ID: usize = 4;
    pub const GET_REPORT_REPLY_ERR: usize = 8;
    pub const GET_REPORT_REPLY_SIZE: usize = 10;
    pub const GET_REPORT_REPLY_DATA: usize = 12;

    // uhid_set_report_req { u32 id; u8 rnum; u8 rtype; u16 size; u8 data[4096]; }
    pub const SET_REPORT_ID: usize = 4;

    // uhid_set_report_reply_req { u32 id; u16 err; }
    pub const SET_REPORT_REPLY_ID: usize = 4;
    pub const SET_REPORT_REPLY_ERR: usize = 8;
    pub const SET_REPORT_REPLY_LEN: usize = 10;
}

/// `BUS_USB` of `linux/input.h`.
const BUS_USB: u16 = 0x03;
/// Sony's ids of the DualSense (`USB_VENDOR_ID_SONY`, `USB_DEVICE_ID_SONY_PS5_CONTROLLER`),
/// which `hid-playstation` binds to; the version is the pad's `bcdDevice`.
const VENDOR: u32 = 0x054c;
const PRODUCT: u32 = 0x0ce6;
const VERSION: u32 = 0x0100;

/// How long the event thread waits for the kernel before it checks the stop flag.
const POLL_MS: i32 = 50;

/// The DualSense USB report descriptor (inputtino `ps5.hpp` `ps5_rdesc`, from
/// nondebug/dualsense `report-descriptor-usb.txt`).
#[rustfmt::skip]
const RDESC: [u8; 273] = [
    0x05, 0x01,       // Usage Page (Generic Desktop Ctrls)
    0x09, 0x05,       // Usage (Game Pad)
    0xA1, 0x01,       // Collection (Application)
    0x85, 0x01,       //   Report ID (1)
    0x09, 0x30,       //   Usage (X)
    0x09, 0x31,       //   Usage (Y)
    0x09, 0x32,       //   Usage (Z)
    0x09, 0x35,       //   Usage (Rz)
    0x09, 0x33,       //   Usage (Rx)
    0x09, 0x34,       //   Usage (Ry)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xFF, 0x00, //   Logical Maximum (255)
    0x75, 0x08,       //   Report Size (8)
    0x95, 0x06,       //   Report Count (6)
    0x81, 0x02,       //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x06, 0x00, 0xFF, //   Usage Page (Vendor Defined 0xFF00)
    0x09, 0x20,       //   Usage (0x20)
    0x95, 0x01,       //   Report Count (1)
    0x81, 0x02,       //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,       //   Usage Page (Generic Desktop Ctrls)
    0x09, 0x39,       //   Usage (Hat switch)
    0x15, 0x00,       //   Logical Minimum (0)
    0x25, 0x07,       //   Logical Maximum (7)
    0x35, 0x00,       //   Physical Minimum (0)
    0x46, 0x3B, 0x01, //   Physical Maximum (315)
    0x65, 0x14,       //   Unit (System: English Rotation, Length: Centimeter)
    0x75, 0x04,       //   Report Size (4)
    0x95, 0x01,       //   Report Count (1)
    0x81, 0x42,       //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,Null State)
    0x65, 0x00,       //   Unit (None)
    0x05, 0x09,       //   Usage Page (Button)
    0x19, 0x01,       //   Usage Minimum (0x01)
    0x29, 0x0F,       //   Usage Maximum (0x0F)
    0x15, 0x00,       //   Logical Minimum (0)
    0x25, 0x01,       //   Logical Maximum (1)
    0x75, 0x01,       //   Report Size (1)
    0x95, 0x0F,       //   Report Count (15)
    0x81, 0x02,       //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x06, 0x00, 0xFF, //   Usage Page (Vendor Defined 0xFF00)
    0x09, 0x21,       //   Usage (0x21)
    0x95, 0x0D,       //   Report Count (13)
    0x81, 0x02,       //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x06, 0x00, 0xFF, //   Usage Page (Vendor Defined 0xFF00)
    0x09, 0x22,       //   Usage (0x22)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xFF, 0x00, //   Logical Maximum (255)
    0x75, 0x08,       //   Report Size (8)
    0x95, 0x34,       //   Report Count (52)
    0x81, 0x02,       //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x85, 0x02,       //   Report ID (2)
    0x09, 0x23,       //   Usage (0x23)
    0x95, 0x2F,       //   Report Count (47)
    0x91, 0x02,       //   Output (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x05,       //   Report ID (5)
    0x09, 0x33,       //   Usage (0x33)
    0x95, 0x28,       //   Report Count (40)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x08,       //   Report ID (8)
    0x09, 0x34,       //   Usage (0x34)
    0x95, 0x2F,       //   Report Count (47)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x09,       //   Report ID (9)
    0x09, 0x24,       //   Usage (0x24)
    0x95, 0x13,       //   Report Count (19)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x0A,       //   Report ID (10)
    0x09, 0x25,       //   Usage (0x25)
    0x95, 0x1A,       //   Report Count (26)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x20,       //   Report ID (32)
    0x09, 0x26,       //   Usage (0x26)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x21,       //   Report ID (33)
    0x09, 0x27,       //   Usage (0x27)
    0x95, 0x04,       //   Report Count (4)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x22,       //   Report ID (34)
    0x09, 0x40,       //   Usage (0x40)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x80,       //   Report ID (-128)
    0x09, 0x28,       //   Usage (0x28)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x81,       //   Report ID (-127)
    0x09, 0x29,       //   Usage (0x29)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x82,       //   Report ID (-126)
    0x09, 0x2A,       //   Usage (0x2A)
    0x95, 0x09,       //   Report Count (9)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x83,       //   Report ID (-125)
    0x09, 0x2B,       //   Usage (0x2B)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x84,       //   Report ID (-124)
    0x09, 0x2C,       //   Usage (0x2C)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0x85,       //   Report ID (-123)
    0x09, 0x2D,       //   Usage (0x2D)
    0x95, 0x02,       //   Report Count (2)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0xA0,       //   Report ID (-96)
    0x09, 0x2E,       //   Usage (0x2E)
    0x95, 0x01,       //   Report Count (1)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0xE0,       //   Report ID (-32)
    0x09, 0x2F,       //   Usage (0x2F)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0xF0,       //   Report ID (-16)
    0x09, 0x30,       //   Usage (0x30)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0xF1,       //   Report ID (-15)
    0x09, 0x31,       //   Usage (0x31)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0xF2,       //   Report ID (-14)
    0x09, 0x32,       //   Usage (0x32)
    0x95, 0x0F,       //   Report Count (15)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0xF4,       //   Report ID (-12)
    0x09, 0x35,       //   Usage (0x35)
    0x95, 0x3F,       //   Report Count (63)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x85, 0xF5,       //   Report ID (-11)
    0x09, 0x36,       //   Usage (0x36)
    0x95, 0x03,       //   Report Count (3)
    0xB1, 0x02,       //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0xC0,             // End Collection
];

const _: () = assert!(RDESC.len() <= uhid::DATA_MAX);

/// Feature report ids `hid-playstation` reads while probing (`DS_FEATURE_REPORT_*`).
const FEATURE_CALIBRATION: u8 = 0x05;
const FEATURE_PAIRING_INFO: u8 = 0x09;
const FEATURE_FIRMWARE_INFO: u8 = 0x20;

/// Calibration (inputtino `ps5_calibration_info`; `DS_FEATURE_REPORT_CALIBRATION_SIZE` 41).
/// Biases 0, gyro ±10000 at speed 500 and accelerometer ±10000: the kernel and SDL then read
/// 20 raw units as 1 °/s and 10000 as 1 g (see [`GYRO_RAW_PER_DEG_S`], [`ACCEL_RAW_PER_MS2`]).
#[rustfmt::skip]
const CALIBRATION: [u8; 41] = [
    0x05,
    0x00, // gyro_pitch_bias
    0x00,
    0x00, // gyro_yaw_bias
    0x00,
    0x00, // gyro_roll_bias
    0x00,
    0x10, // gyro_pitch_plus
    0x27,
    0xF0, // gyro_pitch_minus
    0xD8,
    0x10, // gyro_yaw_plus
    0x27,
    0xF0, // gyro_yaw_minus
    0xD8,
    0x10, // gyro_roll_plus
    0x27,
    0xF0, // gyro_roll_minus
    0xD8,
    0xF4, // gyro_speed_plus
    0x01,
    0xF4, // gyro_speed_minus
    0x01,
    0x10, // acc_x_plus
    0x27,
    0xF0, // acc_x_minus
    0xD8,
    0x10, // acc_y_plus
    0x27,
    0xF0, // acc_y_minus
    0xD8,
    0x10, // acc_z_plus
    0x27,
    0xF0, // acc_z_minus
    0xD8, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Firmware info (inputtino `ps5_firmware_info`; `DS_FEATURE_REPORT_FIRMWARE_INFO_SIZE` 64).
/// The kernel reads the hardware and firmware versions at 24 and 28 and the update version
/// at 44 (0x0154, below 2.21, so it rumbles through `valid_flag0`).
#[rustfmt::skip]
const FIRMWARE_INFO: [u8; 64] = [
    0x20, 0x4A, 0x75, 0x6E, 0x20, 0x31, 0x39, 0x20, 0x32, 0x30, 0x32, 0x33, 0x31, 0x34, 0x3A, 0x34,
    0x37, 0x3A, 0x33, 0x34, 0x03, 0x00, 0x44, 0x00, 0x08, 0x02, 0x00, 0x01, 0x36, 0x00, 0x00, 0x01,
    0xC1, 0xC8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x54, 0x01, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x0B, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Pairing info (inputtino `ps5_pairing_info`; `DS_FEATURE_REPORT_PAIRING_INFO_SIZE` 20).
/// Bytes 1-6 are the pad's MAC, last octet first; [`feature_report`] puts the pad's own there.
#[rustfmt::skip]
const PAIRING_INFO: [u8; 20] = [
    0x09, 0x74, 0xE7, 0xD6, 0x3A, 0x53, 0x35, 0x08, 0x25, 0x00,
    0x1E, 0x00, 0xEE, 0x74, 0xD0, 0xBC, 0x00, 0x00, 0x00, 0x00,
];
/// Where the MAC goes in [`PAIRING_INFO`].
const PAIRING_MAC: usize = 1;

/// Raw gyroscope units per °/s with [`CALIBRATION`]: the kernel scales a raw value by
/// `(speed_plus + speed_minus) * 1024 / (|pitch_plus| + |pitch_minus|)` = 1000 * 1024 / 20000
/// to 1024 units per °/s, so 1 °/s is 20 raw. inputtino multiplies rad/s by 1145 (19.98 per
/// °/s); Moonlight sends °/s.
const GYRO_RAW_PER_DEG_S: f32 = 20.0;
/// Raw accelerometer units per m/s² with [`CALIBRATION`]: the kernel and SDL scale a raw value
/// by `2 * 8192 / (acc_plus - acc_minus)` = 16384 / 20000 to 8192 units per g, so 1 g is 10000
/// raw. inputtino multiplies m/s² by 9.80665 * 100 (980.7, 3.8 % less); this keeps gravity at
/// 1 g. Moonlight sends m/s², gravity included.
const ACCEL_RAW_PER_MS2: f32 = 10_000.0 / 9.806_65;

/// USB input report 0x01 (`DS_INPUT_REPORT_USB`, 64 bytes): the report id, then the kernel's
/// `struct dualsense_input_report` (inputtino's of the same name). Offsets in the report.
const INPUT_REPORT_USB: u8 = 0x01;
const INPUT_REPORT_SIZE: usize = 64;
const IN_LX: usize = 1;
const IN_LY: usize = 2;
const IN_RX: usize = 3;
const IN_RY: usize = 4;
const IN_L2: usize = 5;
const IN_R2: usize = 6;
const IN_SEQ: usize = 7;
/// `buttons[4]`, then `reserved[4]`.
const IN_BUTTONS: usize = 8;
/// `__le16 gyro[3]`, `__le16 accel[3]`, `__le32 sensor_timestamp`, `reserved2`.
const IN_GYRO: usize = 16;
const IN_ACCEL: usize = 22;
const IN_TIMESTAMP: usize = 28;
/// `struct dualsense_touch_point points[2]`, 4 bytes each: contact (bit 7 set: no touch, the
/// rest the touch id), x low 8 bits, x high 4 and y low 4, y high 8.
const IN_TOUCH: [usize; 2] = [33, 37];
/// Battery: level (0-10, tens of percent) in the low nibble, charging state in the high one.
const IN_STATUS: usize = 53;
/// What inputtino puts in the byte after the status (kernel `reserved4[0]`).
const IN_STATUS2: usize = 54;
const STATUS2: u8 = 0x0c;

/// A touch point with no finger (`DS_TOUCH_POINT_INACTIVE`).
const TOUCH_INACTIVE: u8 = 0x80;
/// Full battery (inputtino `BATTERY_FULL`, kernel charging status 0x2) at level 10, so the
/// desktop does not warn about a low battery the pad does not have.
const STATUS: u8 = (0x2 << 4) | 10;

/// `buttons[0]`: the hat in the low nibble (`DS_BUTTONS0_*`).
const HAT_NEUTRAL: u8 = 0x08;
const SQUARE: u8 = 0x10;
const CROSS: u8 = 0x20;
const CIRCLE: u8 = 0x40;
const TRIANGLE: u8 = 0x80;
/// `buttons[1]` (`DS_BUTTONS1_*`).
const L1: u8 = 0x01;
const R1: u8 = 0x02;
const L2: u8 = 0x04;
const R2: u8 = 0x08;
const CREATE: u8 = 0x10;
const OPTIONS: u8 = 0x20;
const L3: u8 = 0x40;
const R3: u8 = 0x80;
/// `buttons[2]` (`DS_BUTTONS2_*`).
const PS_HOME: u8 = 0x01;
const TOUCHPAD: u8 = 0x02;
const MIC_MUTE: u8 = 0x04;

/// Moonlight's `TOUCHPAD_FLAG` and `MISC_FLAG` (`Limelight.h`): bits of the second button
/// word, which [`PadState::buttons`] keeps above the first 16.
const MOONLIGHT_TOUCHPAD: u32 = 0x0010_0000;
const MOONLIGHT_MISC: u32 = 0x0020_0000;

/// Moonlight buttons and where they go: byte of `buttons[]` and bit (inputtino
/// `set_pressed_buttons`). The paddles have no place on a DualSense.
const BUTTONS: [(u32, usize, u8); 13] = [
    (button::X, 0, SQUARE),
    (button::A, 0, CROSS),
    (button::B, 0, CIRCLE),
    (button::Y, 0, TRIANGLE),
    (button::LB, 1, L1),
    (button::RB, 1, R1),
    (button::BACK, 1, CREATE),
    (button::START, 1, OPTIONS),
    (button::LEFT_STICK, 1, L3),
    (button::RIGHT_STICK, 1, R3),
    (button::GUIDE, 2, PS_HOME),
    (MOONLIGHT_TOUCHPAD, 2, TOUCHPAD),
    (MOONLIGHT_MISC, 2, MIC_MUTE),
];

/// USB output report 0x02 (`DS_OUTPUT_REPORT_USB`): the report id, then the kernel's
/// `struct dualsense_output_report_common`. Offsets in the report.
const OUTPUT_REPORT_USB: u8 = 0x02;
/// `valid_flag2`: after flags 0 and 1, the motors, `reserved[4]`, `mute_button_led`,
/// `power_save_control` and `reserved2[28]` (the trigger effects).
const OUT_VALID_FLAG2: usize = 39;
/// `DS_OUTPUT_VALID_FLAG0_COMPATIBLE_VIBRATION` (inputtino `MOTOR_OR_COMPATIBLE_VIBRATION`).
const FLAG0_COMPATIBLE_VIBRATION: u8 = 0x01;
/// `DS_OUTPUT_VALID_FLAG2_COMPATIBLE_VIBRATION2`, used with update versions from 2.21 on.
const FLAG2_COMPATIBLE_VIBRATION2: u8 = 0x04;

/// MACs of the DualSenses alive in this process. `hid-playstation` refuses a second device
/// with a MAC it already has ("Duplicate device found for MAC address"), and two clients can
/// each have a pad 0.
static MACS: Mutex<Vec<[u8; 6]>> = Mutex::new(Vec::new());

/// A pad's MAC: locally administered and unicast (0x02 in the first octet), "gld", an
/// instance number that only changes when another pad with the same number is alive, and the
/// pad number. One client's pad N keeps the same `uniq` across sessions.
fn mac_for(number: usize, instance: u8) -> [u8; 6] {
    [0x02, b'g', b'l', b'd', instance, number as u8]
}

/// A MAC address as the kernel prints `uniq` (`%pMR` of the reversed bytes).
fn mac_string(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// A MAC in [`MACS`] for as long as this lives.
struct MacLease([u8; 6]);

impl MacLease {
    fn take(number: usize) -> Option<Self> {
        let mut used = MACS.lock().unwrap_or_else(PoisonError::into_inner);
        let mac = (0..=u8::MAX)
            .map(|instance| mac_for(number, instance))
            .find(|m| !used.contains(m))?;
        used.push(mac);
        Some(Self(mac))
    }
}

impl Drop for MacLease {
    fn drop(&mut self) {
        MACS.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|m| *m != self.0);
    }
}

/// The last motion reports, in raw units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Motion {
    gyro: [i16; 3],
    accel: [i16; 3],
}

struct DualSense {
    file: Arc<File>,
    stop: Arc<AtomicBool>,
    state: PadState,
    motion: Motion,
    seq: u8,
    /// Origin of the sensor timestamp.
    start: Instant,
    /// Dropped after `Drop::drop` destroyed the device, so the MAC is free in the kernel too.
    _mac: MacLease,
}

/// Creates the DualSense for Moonlight controller `number`; its rumble goes to `rumble`.
pub(super) fn create(number: usize, rumble: Rumble) -> Result<Box<dyn Pad>> {
    // Taken before the device exists and released after it is gone (locals drop in reverse).
    let lease = MacLease::take(number).context("no free MAC address for a virtual DualSense")?;
    let mac = lease.0;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/dev/uhid")
        .context("opening /dev/uhid")?;
    let file = Arc::new(file);
    let name = format!("glidedesk DualSense (pad {number})");
    let phys = format!("glidedesk/pad{number}");
    send(&file, &create2_event(&name, &phys, &mac_string(&mac), &RDESC))
        .context("creating the virtual DualSense")?;
    // The kernel probes the device from a worker and waits (up to 5 s) for the feature
    // reports this thread answers. If the thread cannot start, dropping `file` closes
    // /dev/uhid, which destroys the device.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (file, stop) = (Arc::clone(&file), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("gs-dualsense".into())
            .spawn(move || events(&file, &stop, number as u16, mac, &rumble))
            .context("starting the DualSense thread")?;
    }
    debug!(number, mac = mac_string(&mac), "Virtual DualSense created");
    Ok(Box::new(DualSense {
        file,
        stop,
        state: PadState::default(),
        motion: Motion::default(),
        seq: 0,
        start: Instant::now(),
        _mac: lease,
    }))
}

impl Drop for DualSense {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Closing /dev/uhid would destroy the device too, but the event thread keeps the file
        // open until its next poll timeout.
        if let Err(e) = send(&self.file, &uhid::DESTROY.to_ne_bytes()) {
            debug!(%e, "Destroying the virtual DualSense failed");
        }
    }
}

impl Pad for DualSense {
    fn set(&mut self, s: &PadState) -> Result<()> {
        self.state = *s;
        self.send_input()
    }

    fn motion(&mut self, kind: u8, xyz: [f32; 3]) -> Result<()> {
        match kind {
            MOTION_ACCEL => self.motion.accel = xyz.map(|v| to_raw(v, ACCEL_RAW_PER_MS2)),
            MOTION_GYRO => self.motion.gyro = xyz.map(|v| to_raw(v, GYRO_RAW_PER_DEG_S)),
            _ => return Ok(()),
        }
        self.send_input()
    }

    fn has_motion(&self) -> bool {
        true
    }
}

impl DualSense {
    /// Sends the current state, motion and inactive touch points as an input report.
    fn send_input(&mut self) -> Result<()> {
        // inputtino counts 0-254 and wraps to 0.
        self.seq = (self.seq + 1) % 255;
        // The sensor clock counts thirds of a microsecond and wraps at 32 bits.
        let timestamp = (self.start.elapsed().as_nanos() * 3 / 1000) as u32;
        let report = input_report(&self.state, &self.motion, self.seq, timestamp);
        send(&self.file, &input2_event(&report)).context("writing a DualSense input report")
    }
}

/// A value in `per_unit` raw units, saturated to the 16 bits of the report (inputtino
/// `to_le_signed`). NaN becomes 0.
fn to_raw(v: f32, per_unit: f32) -> i16 {
    (v * per_unit)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// A stick axis (-32768-32767, or 32768 for a flipped -32768) to 0-255 with 0 at 0x80
/// (inputtino `scale_value`: `round((v + 32768) / 257)`, halves rounded up).
fn axis(v: i32) -> u8 {
    let n = (v + 32768).clamp(0, 65536);
    ((2 * n + 257) / 514).min(255) as u8
}

/// The d-pad as the hat value: 0 up, clockwise to 7 up-left, 8 centred. Opposite directions
/// cancel out.
fn hat(buttons: u32) -> u8 {
    let pressed = |bit: u32| i8::from(buttons & bit != 0);
    let dx = pressed(button::RIGHT) - pressed(button::LEFT);
    let dy = pressed(button::DOWN) - pressed(button::UP);
    match (dx, dy) {
        (0, -1) => 0,
        (1, -1) => 1,
        (1, 0) => 2,
        (1, 1) => 3,
        (0, 1) => 4,
        (-1, 1) => 5,
        (-1, 0) => 6,
        (-1, -1) => 7,
        _ => HAT_NEUTRAL,
    }
}

/// USB input report 0x01 for a state, the last motion, a sequence number and a timestamp.
fn input_report(
    s: &PadState,
    motion: &Motion,
    seq: u8,
    timestamp: u32,
) -> [u8; INPUT_REPORT_SIZE] {
    let mut r = [0u8; INPUT_REPORT_SIZE];
    r[0] = INPUT_REPORT_USB;
    // The report's y grows downwards; Moonlight's up is positive.
    r[IN_LX] = axis(i32::from(s.left.0));
    r[IN_LY] = axis(-i32::from(s.left.1));
    r[IN_RX] = axis(i32::from(s.right.0));
    r[IN_RY] = axis(-i32::from(s.right.1));
    r[IN_L2] = s.left_trigger;
    r[IN_R2] = s.right_trigger;
    r[IN_SEQ] = seq;
    let mut buttons = [hat(s.buttons), 0, 0, 0];
    for (bit, byte, mask) in BUTTONS {
        if s.buttons & bit != 0 {
            buttons[byte] |= mask;
        }
    }
    // The digital L2 and R2 bits follow the analog triggers (inputtino `set_triggers`).
    if s.left_trigger != 0 {
        buttons[1] |= L2;
    }
    if s.right_trigger != 0 {
        buttons[1] |= R2;
    }
    r[IN_BUTTONS..IN_BUTTONS + 4].copy_from_slice(&buttons);
    for (i, (gyro, accel)) in motion.gyro.iter().zip(&motion.accel).enumerate() {
        r[IN_GYRO + 2 * i..IN_GYRO + 2 * i + 2].copy_from_slice(&gyro.to_le_bytes());
        r[IN_ACCEL + 2 * i..IN_ACCEL + 2 * i + 2].copy_from_slice(&accel.to_le_bytes());
    }
    r[IN_TIMESTAMP..IN_TIMESTAMP + 4].copy_from_slice(&timestamp.to_le_bytes());
    for at in IN_TOUCH {
        r[at] = TOUCH_INACTIVE;
    }
    r[IN_STATUS] = STATUS;
    r[IN_STATUS2] = STATUS2;
    r
}

/// The feature report `rnum`, or `None` for one the pad does not have.
fn feature_report(rnum: u8, mac: &[u8; 6]) -> Option<Vec<u8>> {
    match rnum {
        FEATURE_CALIBRATION => Some(CALIBRATION.to_vec()),
        FEATURE_PAIRING_INFO => {
            let mut report = PAIRING_INFO;
            let mut reversed = *mac;
            reversed.reverse();
            report[PAIRING_MAC..PAIRING_MAC + 6].copy_from_slice(&reversed);
            Some(report.to_vec())
        }
        FEATURE_FIRMWARE_INFO => Some(FIRMWARE_INFO.to_vec()),
        _ => None,
    }
}

/// Rumble in a USB output report: low and high frequency motors, 0-65535 (inputtino
/// `on_uhid_event`). The kernel puts the strong (low frequency) motor in `motor_left` and the
/// weak one in `motor_right`, 0-255 each. A report with no valid flags stops the motors;
/// `None` for reports that leave them alone (LEDs only, Bluetooth reports).
fn output_rumble(report: &[u8]) -> Option<(u16, u16)> {
    let &[OUTPUT_REPORT_USB, flag0, flag1, motor_right, motor_left, ..] = report else {
        return None;
    };
    let flag2 = *report.get(OUT_VALID_FLAG2)?;
    if flag0 & FLAG0_COMPATIBLE_VIBRATION != 0 || flag2 & FLAG2_COMPATIBLE_VIBRATION2 != 0 {
        Some((u16::from(motor_left) * 257, u16::from(motor_right) * 257))
    } else if flag0 == 0 && flag1 == 0 && flag2 == 0 {
        Some((0, 0))
    } else {
        None
    }
}

/// What the pad does about one event from the kernel.
#[derive(Debug, PartialEq, Eq)]
enum Reaction {
    /// Write this event back.
    Reply(Vec<u8>),
    /// Tell the client: low and high frequency motors.
    Rumble(u16, u16),
    Nothing,
}

fn react(ev: &[u8], mac: &[u8; 6]) -> Reaction {
    match ne_u32(ev, 0) {
        Some(uhid::GET_REPORT) => {
            let (Some(id), Some(&rnum)) = (
                ne_u32(ev, uhid::GET_REPORT_ID),
                ev.get(uhid::GET_REPORT_RNUM),
            ) else {
                return Reaction::Nothing;
            };
            Reaction::Reply(match feature_report(rnum, mac) {
                Some(data) => get_report_reply(id, 0, &data),
                None => get_report_reply(id, libc::EINVAL as u16, &[]),
            })
        }
        // Nothing the pad needs is set through feature reports; acknowledge them so the
        // caller does not wait for the kernel's timeout.
        Some(uhid::SET_REPORT) => ne_u32(ev, uhid::SET_REPORT_ID)
            .map_or(Reaction::Nothing, |id| Reaction::Reply(set_report_reply(id, 0))),
        Some(uhid::OUTPUT) => {
            let size = ne_u16(ev, uhid::OUTPUT_SIZE)
                .map_or(0, usize::from)
                .min(uhid::DATA_MAX);
            ev.get(uhid::OUTPUT_DATA..uhid::OUTPUT_DATA + size)
                .and_then(output_rumble)
                .map_or(Reaction::Nothing, |(low, high)| Reaction::Rumble(low, high))
        }
        // UHID_START, UHID_STOP, UHID_OPEN, UHID_CLOSE: nothing to do.
        _ => Reaction::Nothing,
    }
}

/// Answers the kernel's requests and passes rumble on until `stop` is set.
fn events(file: &File, stop: &AtomicBool, number: u16, mac: [u8; 6], rumble: &Rumble) {
    let mut buf = vec![0u8; uhid::EVENT_SIZE];
    let mut rumbling = false;
    while !stop.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call; `file` keeps the fd open.
        let ready = unsafe { libc::poll(&mut pfd, 1, POLL_MS) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
        if pfd.revents & libc::POLLIN == 0 {
            continue;
        }
        let mut reader = file;
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(e) => {
                debug!(%e, "Reading /dev/uhid failed");
                break;
            }
        };
        match react(&buf[..n], &mac) {
            Reaction::Reply(ev) => {
                if let Err(e) = send(file, &ev) {
                    debug!(%e, "Answering the kernel's DualSense request failed");
                }
            }
            Reaction::Rumble(low, high) => {
                rumbling = low != 0 || high != 0;
                rumble(number, low, high);
            }
            Reaction::Nothing => {}
        }
    }
    if rumbling {
        rumble(number, 0, 0);
    }
}

/// Writes one event: /dev/uhid takes each in a single write.
fn send(file: &File, ev: &[u8]) -> io::Result<()> {
    let mut writer = file;
    let n = writer.write(ev)?;
    if n != ev.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short write to /dev/uhid",
        ));
    }
    Ok(())
}

fn ne_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn ne_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

/// An event of `len` bytes (the kernel zero-fills the rest of a short write).
fn event(kind: u32, len: usize) -> Vec<u8> {
    let mut ev = vec![0u8; len];
    ev[..4].copy_from_slice(&kind.to_ne_bytes());
    ev
}

fn put(ev: &mut [u8], at: usize, bytes: &[u8]) {
    ev[at..at + bytes.len()].copy_from_slice(bytes);
}

/// A string in a NUL-terminated field of `room` bytes, cut if longer.
fn put_str(ev: &mut [u8], at: usize, room: usize, s: &str) {
    let bytes = s.as_bytes();
    put(ev, at, &bytes[..bytes.len().min(room - 1)]);
}

fn create2_event(name: &str, phys: &str, uniq: &str, rdesc: &[u8]) -> Vec<u8> {
    let mut ev = event(uhid::CREATE2, uhid::CREATE2_RD_DATA + rdesc.len());
    put_str(&mut ev, uhid::CREATE2_NAME, uhid::CREATE2_NAME_LEN, name);
    put_str(&mut ev, uhid::CREATE2_PHYS, uhid::CREATE2_PHYS_LEN, phys);
    put_str(&mut ev, uhid::CREATE2_UNIQ, uhid::CREATE2_UNIQ_LEN, uniq);
    put(&mut ev, uhid::CREATE2_RD_SIZE, &(rdesc.len() as u16).to_ne_bytes());
    put(&mut ev, uhid::CREATE2_BUS, &BUS_USB.to_ne_bytes());
    put(&mut ev, uhid::CREATE2_VENDOR, &VENDOR.to_ne_bytes());
    put(&mut ev, uhid::CREATE2_PRODUCT, &PRODUCT.to_ne_bytes());
    put(&mut ev, uhid::CREATE2_VERSION, &VERSION.to_ne_bytes());
    put(&mut ev, uhid::CREATE2_COUNTRY, &0u32.to_ne_bytes());
    put(&mut ev, uhid::CREATE2_RD_DATA, rdesc);
    ev
}

fn input2_event(report: &[u8]) -> Vec<u8> {
    let mut ev = event(uhid::INPUT2, uhid::INPUT2_DATA + report.len());
    put(&mut ev, uhid::INPUT2_SIZE, &(report.len() as u16).to_ne_bytes());
    put(&mut ev, uhid::INPUT2_DATA, report);
    ev
}

fn get_report_reply(id: u32, err: u16, data: &[u8]) -> Vec<u8> {
    let mut ev = event(
        uhid::GET_REPORT_REPLY,
        uhid::GET_REPORT_REPLY_DATA + data.len(),
    );
    put(&mut ev, uhid::GET_REPORT_REPLY_ID, &id.to_ne_bytes());
    put(&mut ev, uhid::GET_REPORT_REPLY_ERR, &err.to_ne_bytes());
    put(
        &mut ev,
        uhid::GET_REPORT_REPLY_SIZE,
        &(data.len() as u16).to_ne_bytes(),
    );
    put(&mut ev, uhid::GET_REPORT_REPLY_DATA, data);
    ev
}

fn set_report_reply(id: u32, err: u16) -> Vec<u8> {
    let mut ev = event(uhid::SET_REPORT_REPLY, uhid::SET_REPORT_REPLY_LEN);
    put(&mut ev, uhid::SET_REPORT_REPLY_ID, &id.to_ne_bytes());
    put(&mut ev, uhid::SET_REPORT_REPLY_ERR, &err.to_ne_bytes());
    ev
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uhid_layout() {
        assert_eq!(uhid::EVENT_SIZE, 4376);
        assert_eq!(uhid::CREATE2_RD_DATA + uhid::DATA_MAX, uhid::EVENT_SIZE);
        assert_eq!(RDESC.len(), 273);
        assert_eq!(RDESC[RDESC.len() - 1], 0xC0);
    }

    #[test]
    fn create2_event_bytes() {
        let ev = create2_event(
            "glidedesk DualSense (pad 0)",
            "glidedesk/pad0",
            "02:67:6c:64:00:00",
            &RDESC,
        );
        assert_eq!(ev.len(), 280 + 273);
        assert_eq!(ne_u32(&ev, 0), Some(uhid::CREATE2));
        assert_eq!(&ev[4..32], b"glidedesk DualSense (pad 0)\0");
        assert_eq!(&ev[132..147], b"glidedesk/pad0\0");
        assert_eq!(&ev[196..214], b"02:67:6c:64:00:00\0");
        assert_eq!(ne_u16(&ev, 260), Some(273));
        assert_eq!(ne_u16(&ev, 262), Some(0x03));
        assert_eq!(ne_u32(&ev, 264), Some(0x054c));
        assert_eq!(ne_u32(&ev, 268), Some(0x0ce6));
        assert_eq!(ne_u32(&ev, 272), Some(0x0100));
        assert_eq!(ne_u32(&ev, 276), Some(0));
        assert_eq!(&ev[280..], &RDESC);
        // Names longer than the field keep their NUL.
        let long = create2_event(&"x".repeat(300), "", "", &RDESC);
        assert_eq!(long[4 + 126], b'x');
        assert_eq!(long[4 + 127], 0);
    }

    #[test]
    fn input_report_maps_the_state() {
        let s = PadState {
            buttons: button::A
                | button::Y
                | button::LB
                | button::START
                | button::GUIDE
                | button::UP
                | button::RIGHT
                | MOONLIGHT_TOUCHPAD
                | MOONLIGHT_MISC,
            left_trigger: 200,
            right_trigger: 0,
            left: (-32768, 32767),
            right: (0, -32768),
        };
        let motion = Motion {
            gyro: [1, -2, 3],
            accel: [-1, 0, i16::MAX],
        };
        let r = input_report(&s, &motion, 5, 0x1234_5678);
        assert_eq!(r.len(), 64);
        assert_eq!(r[0], 0x01);
        // Sticks: left full left and up, right centred and down.
        assert_eq!(&r[1..5], &[0, 0, 0x80, 0xff]);
        assert_eq!(&r[5..8], &[200, 0, 5]);
        // Hat up-right (1), cross and triangle; L1, options and the digital L2; PS,
        // touchpad click and mute.
        assert_eq!(&r[8..12], &[0x01 | 0x20 | 0x80, 0x01 | 0x20 | 0x04, 0x07, 0]);
        assert_eq!(&r[12..16], &[0; 4]);
        assert_eq!(&r[16..22], &[1, 0, 0xfe, 0xff, 3, 0]);
        assert_eq!(&r[22..28], &[0xff, 0xff, 0, 0, 0xff, 0x7f]);
        assert_eq!(&r[28..32], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(r[32], 0);
        assert_eq!(&r[33..41], &[0x80, 0, 0, 0, 0x80, 0, 0, 0]);
        assert_eq!(&r[41..53], &[0; 12]);
        assert_eq!(&r[53..55], &[0x2a, 0x0c]);
        assert_eq!(&r[55..], &[0; 9]);

        let s = PadState {
            buttons: button::B
                | button::X
                | button::RB
                | button::BACK
                | button::LEFT_STICK
                | button::RIGHT_STICK
                | button::DOWN
                | button::LEFT
                // Paddles have no place.
                | 0x0001_0000,
            left_trigger: 0,
            right_trigger: 1,
            left: (32767, -32768),
            right: (0, 0),
        };
        let r = input_report(&s, &Motion::default(), 0, 0);
        assert_eq!(&r[1..5], &[0xff, 0xff, 0x80, 0x80]);
        // Hat down-left (5), circle and square; R1, create, L3, R3 and the digital R2.
        assert_eq!(
            &r[8..12],
            &[0x05 | 0x40 | 0x10, 0x02 | 0x10 | 0x40 | 0x80 | 0x08, 0, 0]
        );

        let r = input_report(&PadState::default(), &Motion::default(), 0, 0);
        assert_eq!(&r[1..7], &[0x80, 0x80, 0x80, 0x80, 0, 0]);
        assert_eq!(r[8], 0x08);
    }

    #[test]
    fn hat_directions() {
        let hat_of = |bits: &[u32]| hat(bits.iter().fold(0, |a, b| a | b));
        assert_eq!(hat_of(&[]), 8);
        assert_eq!(hat_of(&[button::UP]), 0);
        assert_eq!(hat_of(&[button::RIGHT]), 2);
        assert_eq!(hat_of(&[button::DOWN, button::RIGHT]), 3);
        assert_eq!(hat_of(&[button::DOWN]), 4);
        assert_eq!(hat_of(&[button::LEFT]), 6);
        assert_eq!(hat_of(&[button::UP, button::LEFT]), 7);
        assert_eq!(hat_of(&[button::UP, button::DOWN]), 8);
    }

    #[test]
    fn axes_and_motion_units() {
        assert_eq!(axis(-32768), 0);
        assert_eq!(axis(0), 0x80);
        assert_eq!(axis(32767), 0xff);
        assert_eq!(axis(32768), 0xff);
        assert_eq!(axis(-257), 0x7f);
        // 1 g reads 10000 raw, 1 °/s 20 raw (the calibration's units).
        assert!((to_raw(9.806_65, ACCEL_RAW_PER_MS2) - 10_000).abs() <= 1);
        assert_eq!(to_raw(-1.0, GYRO_RAW_PER_DEG_S), -20);
        assert_eq!(to_raw(1e9, GYRO_RAW_PER_DEG_S), i16::MAX);
        assert_eq!(to_raw(-1e9, ACCEL_RAW_PER_MS2), i16::MIN);
        assert_eq!(to_raw(f32::NAN, GYRO_RAW_PER_DEG_S), 0);
    }

    fn get_report(id: u32, rnum: u8) -> Vec<u8> {
        let mut ev = event(uhid::GET_REPORT, 10);
        put(&mut ev, 4, &id.to_ne_bytes());
        ev[8] = rnum;
        ev
    }

    #[test]
    fn pairing_info_carries_the_mac() {
        let mac = mac_for(1, 0);
        assert_eq!(mac, [0x02, 0x67, 0x6c, 0x64, 0x00, 0x01]);
        let Reaction::Reply(reply) = react(&get_report(7, 0x09), &mac) else {
            panic!("no reply");
        };
        assert_eq!(reply.len(), 12 + 20);
        assert_eq!(ne_u32(&reply, 0), Some(uhid::GET_REPORT_REPLY));
        assert_eq!(ne_u32(&reply, 4), Some(7));
        assert_eq!(ne_u16(&reply, 8), Some(0));
        assert_eq!(ne_u16(&reply, 10), Some(20));
        assert_eq!(reply[12], 0x09);
        assert_eq!(&reply[13..19], &[0x01, 0x00, 0x64, 0x6c, 0x67, 0x02]);
        assert_eq!(&reply[19..], &PAIRING_INFO[7..]);
        // The kernel's uniq: `%pMR` of bytes 1-6.
        let mut printed = [0u8; 6];
        printed.copy_from_slice(&reply[13..19]);
        printed.reverse();
        assert_eq!(mac_string(&printed), "02:67:6c:64:00:01");
        assert_eq!(mac_string(&mac), "02:67:6c:64:00:01");
    }

    #[test]
    fn other_feature_reports() {
        let mac = mac_for(0, 0);
        for (rnum, data) in [(0x05, &CALIBRATION[..]), (0x20, &FIRMWARE_INFO[..])] {
            let Reaction::Reply(reply) = react(&get_report(3, rnum), &mac) else {
                panic!("no reply for {rnum:#x}");
            };
            assert_eq!(ne_u16(&reply, 8), Some(0));
            assert_eq!(ne_u16(&reply, 10), Some(data.len() as u16));
            assert_eq!(&reply[12..], data);
        }
        assert_eq!(CALIBRATION.len(), 41);
        assert_eq!(FIRMWARE_INFO.len(), 64);
        // Update version 0x0154 at 44.
        assert_eq!(&FIRMWARE_INFO[44..46], &[0x54, 0x01]);
        let Reaction::Reply(reply) = react(&get_report(4, 0x08), &mac) else {
            panic!("no reply");
        };
        assert_ne!(ne_u16(&reply, 8), Some(0));
        assert_eq!(ne_u16(&reply, 10), Some(0));
        // Too short to be a request.
        assert_eq!(react(&get_report(4, 0x09)[..8], &mac), Reaction::Nothing);
    }

    #[test]
    fn set_reports_are_acknowledged() {
        let mut ev = event(uhid::SET_REPORT, 12);
        put(&mut ev, 4, &9u32.to_ne_bytes());
        let mut expected = Vec::new();
        expected.extend_from_slice(&14u32.to_ne_bytes());
        expected.extend_from_slice(&9u32.to_ne_bytes());
        expected.extend_from_slice(&[0, 0]);
        assert_eq!(react(&ev, &mac_for(0, 0)), Reaction::Reply(expected));
    }

    /// USB output report 0x02 as `hid-playstation` sends it (63 bytes).
    fn output_report(flag0: u8, flag1: u8, flag2: u8, right: u8, left: u8) -> Vec<u8> {
        let mut r = vec![0u8; 63];
        r[0] = 0x02;
        r[1] = flag0;
        r[2] = flag1;
        r[3] = right;
        r[4] = left;
        r[39] = flag2;
        r
    }

    #[test]
    fn rumble_from_output_reports() {
        // Compatible vibration and haptics select, as the kernel sets them.
        assert_eq!(
            output_rumble(&output_report(0x03, 0, 0, 0x40, 0x80)),
            Some((0x8080, 0x4040))
        );
        // Newer firmware: the flag in valid_flag2.
        assert_eq!(
            output_rumble(&output_report(0x02, 0, 0x04, 0xff, 0)),
            Some((0, 0xffff))
        );
        // Lightbar only.
        assert_eq!(output_rumble(&output_report(0, 0x04, 0, 0x40, 0x80)), None);
        assert_eq!(output_rumble(&output_report(0, 0, 0x02, 0, 0)), None);
        // No flags: stop.
        assert_eq!(output_rumble(&output_report(0, 0, 0, 0x40, 0x80)), Some((0, 0)));
        // Not a USB output report, or cut short.
        let mut bt = output_report(0x03, 0, 0, 0x40, 0x80);
        bt[0] = 0x31;
        assert_eq!(output_rumble(&bt), None);
        assert_eq!(output_rumble(&output_report(0x03, 0, 0, 0x40, 0x80)[..20]), None);

        // Through a UHID_OUTPUT event.
        let report = output_report(0x03, 0, 0, 0x10, 0x20);
        let mut ev = event(uhid::OUTPUT, uhid::EVENT_SIZE);
        put(&mut ev, 4, &report);
        put(&mut ev, 4100, &(report.len() as u16).to_ne_bytes());
        ev[4102] = 1; // UHID_OUTPUT_REPORT
        assert_eq!(react(&ev, &mac_for(0, 0)), Reaction::Rumble(0x2020, 0x1010));
    }

    #[test]
    fn macs_are_unique_while_alive() {
        let a = MacLease::take(15).unwrap();
        let b = MacLease::take(15).unwrap();
        assert_eq!(a.0, mac_for(15, 0));
        assert_eq!(b.0, mac_for(15, 1));
        // Locally administered, unicast.
        assert_eq!(a.0[0] & 0x03, 0x02);
        drop(a);
        let c = MacLease::take(15).unwrap();
        assert_eq!(c.0, mac_for(15, 0));
        drop((b, c));
    }

    /// Creates a DualSense through UHID and reads what `hid-playstation` makes of it. Needs
    /// write access to /dev/uhid and read access to /dev/input: `cargo test -- --ignored
    /// dualsense` as root or with udev rules.
    #[test]
    #[ignore]
    fn uhid_dualsense_reports_the_state() {
        use std::time::Duration;
        let rumbles = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&rumbles);
        let mut pad = create(
            3,
            Arc::new(move |n, lo, hi| seen.lock().unwrap().push((n, lo, hi))),
        )
        .expect("no /dev/uhid access");
        assert!(pad.has_motion());
        std::thread::sleep(Duration::from_millis(500));
        let open = |suffix: &str| {
            let want = format!("glidedesk DualSense (pad 3){suffix}");
            std::fs::read_dir("/dev/input")
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter_map(|p| evdev::Device::open(p).ok())
                .find(|d| d.name() == Some(want.as_str()))
                .unwrap_or_else(|| panic!("{want} in /dev/input"))
        };
        let mut gamepad = open("");
        let sensors = open(" Motion Sensors");
        let touchpad = open(" Touchpad");
        let uniq = mac_string(&mac_for(3, 0));
        for dev in [&gamepad, &sensors, &touchpad] {
            assert_eq!(dev.unique_name(), Some(uniq.as_str()));
        }
        let id = gamepad.input_id();
        assert_eq!((id.vendor(), id.product()), (0x054c, 0x0ce6));

        pad.set(&PadState {
            buttons: button::A | button::UP,
            left_trigger: 200,
            right_trigger: 0,
            left: (0, 32767),
            right: (0, 0),
        })
        .unwrap();
        pad.motion(MOTION_ACCEL, [0.0, 9.806_65, 0.0]).unwrap();
        pad.motion(MOTION_GYRO, [1.0, 0.0, -2.0]).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let keys = gamepad.get_key_state().unwrap();
        assert!(keys.contains(evdev::KeyCode::BTN_SOUTH));
        assert!(!keys.contains(evdev::KeyCode::BTN_EAST));
        let abs = gamepad.get_abs_state().unwrap();
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_X.0 as usize].value, 0x80);
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_Y.0 as usize].value, 0);
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_Z.0 as usize].value, 200);
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_HAT0Y.0 as usize].value, -1);
        // The kernel's units: 8192 per g, 1024 per °/s.
        let abs = sensors.get_abs_state().unwrap();
        let accel_y = abs[evdev::AbsoluteAxisCode::ABS_Y.0 as usize].value;
        assert!((accel_y - 8192).abs() <= 1, "{accel_y}");
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_RX.0 as usize].value, 1024);
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_RZ.0 as usize].value, -2048);

        // A game's rumble: the kernel turns it into an output report.
        let effect = evdev::FFEffectData {
            direction: 0,
            trigger: evdev::FFTrigger::default(),
            replay: evdev::FFReplay {
                length: 300,
                delay: 0,
            },
            kind: evdev::FFEffectKind::Rumble {
                strong_magnitude: 0x8000,
                weak_magnitude: 0x1000,
            },
        };
        let mut ff = gamepad.upload_ff_effect(effect).unwrap();
        ff.play(1).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        let got = rumbles.lock().unwrap().clone();
        assert!(got.contains(&(3, 0x8080, 0x1010)), "{got:?}");
        // The effect's 300 ms ran out: the motors stop.
        assert_eq!(got.last(), Some(&(3, 0, 0)), "{got:?}");
        drop(ff);
        drop(pad);
    }
}

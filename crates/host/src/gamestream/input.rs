//! Moonlight's keyboard and mouse (control stream message 0x0206; moonlight-common-c `Input.h`
//! and `InputStream.c`, Sunshine `src/input.cpp`, commits in THIRD_PARTY.md), passed on through
//! the same translation as RDP input.
//!
//! Each message starts with its size (BE u32) and type (LE u32). Keys come as Windows
//! virtual-key codes for the US layout position (Moonlight maps the client's scan codes), which
//! go back to set-1 scan codes here. Positions come relative to the client's view, wheel steps
//! as multiples of 120 (positive up or right), mostly big-endian.

use ironrdp_server::{KeyboardEvent, MouseButton, MouseEvent, RdpServerInputHandler};
use tracing::debug;

use crate::input::InputForwarder;

const KEY_DOWN: u32 = 0x03;
const KEY_UP: u32 = 0x04;
const MOUSE_ABS: u32 = 0x05;
const MOUSE_REL: u32 = 0x07;
const BUTTON_DOWN: u32 = 0x08;
const BUTTON_UP: u32 = 0x09;
const SCROLL: u32 = 0x0a;
const UTF8_TEXT: u32 = 0x17;
const HSCROLL: u32 = 0x5500_0001;

pub struct MoonlightInput {
    input: InputForwarder,
    /// Pointer position in desktop pixels (relative motion moves it).
    pos: (f64, f64),
}

impl MoonlightInput {
    pub fn new(input: InputForwarder) -> Self {
        Self {
            input,
            pos: (0.0, 0.0),
        }
    }

    /// One input message; `desktop` is the size of the desktop being streamed.
    pub fn handle(&mut self, msg: &[u8], desktop: (u32, u32)) {
        let Some(kind) = msg
            .get(4..8)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        else {
            return;
        };
        let body = &msg[8..];
        let be16 = |at: usize| -> Option<i16> {
            body.get(at..at + 2)
                .map(|b| i16::from_be_bytes([b[0], b[1]]))
        };
        let (dw, dh) = (f64::from(desktop.0.max(1)), f64::from(desktop.1.max(1)));
        match kind {
            KEY_DOWN | KEY_UP => {
                let Some(code) = body.get(1..3).map(|b| u16::from_le_bytes([b[0], b[1]])) else {
                    return;
                };
                let vk = (code & 0xff) as u8;
                let Some((code, extended)) = vk_to_scancode(vk) else {
                    debug!(vk = format!("{vk:#04x}"), "Unmapped Moonlight key");
                    return;
                };
                self.input.keyboard(if kind == KEY_DOWN {
                    KeyboardEvent::Pressed { code, extended }
                } else {
                    KeyboardEvent::Released { code, extended }
                });
            }
            MOUSE_ABS => {
                let (Some(x), Some(y), Some(w), Some(h)) = (be16(0), be16(2), be16(6), be16(8))
                else {
                    return;
                };
                // The client sends its view's size less one as the reference.
                let (w, h) = (f64::from(w.max(1)), f64::from(h.max(1)));
                self.pos = (
                    (f64::from(x) / w * (dw - 1.0)).clamp(0.0, dw - 1.0),
                    (f64::from(y) / h * (dh - 1.0)).clamp(0.0, dh - 1.0),
                );
                self.moved();
            }
            MOUSE_REL => {
                let (Some(dx), Some(dy)) = (be16(0), be16(2)) else {
                    return;
                };
                self.pos = (
                    (self.pos.0 + f64::from(dx)).clamp(0.0, dw - 1.0),
                    (self.pos.1 + f64::from(dy)).clamp(0.0, dh - 1.0),
                );
                self.moved();
            }
            BUTTON_DOWN | BUTTON_UP => {
                let button = match body.first() {
                    Some(1) => MouseButton::Left,
                    Some(2) => MouseButton::Middle,
                    Some(3) => MouseButton::Right,
                    Some(4) => MouseButton::X1,
                    Some(5) => MouseButton::X2,
                    _ => return,
                };
                let (x, y) = self.pixel();
                self.input.mouse(MouseEvent::Button {
                    x,
                    y,
                    button,
                    pressed: kind == BUTTON_DOWN,
                });
            }
            SCROLL => {
                if let Some(value) = be16(0) {
                    self.input.mouse(MouseEvent::VerticalScroll { value });
                }
            }
            HSCROLL => {
                if let Some(value) = be16(0) {
                    self.input.mouse(MouseEvent::HorizontalScroll { value });
                }
            }
            UTF8_TEXT => {
                for ch in String::from_utf8_lossy(body).chars() {
                    let mut units = [0u16; 2];
                    for u in ch.encode_utf16(&mut units) {
                        self.input.keyboard(KeyboardEvent::UnicodePressed(*u));
                        self.input.keyboard(KeyboardEvent::UnicodeReleased(*u));
                    }
                }
            }
            other => debug!(
                kind = format!("{other:#x}"),
                "Moonlight input not handled yet"
            ),
        }
    }

    /// The client went away: let go of every key.
    pub fn release_all(&mut self) {
        self.input.keyboard(KeyboardEvent::Synchronize(
            ironrdp_pdu::input::fast_path::SynchronizeFlags::empty(),
        ));
    }

    fn pixel(&self) -> (u16, u16) {
        (self.pos.0 as u16, self.pos.1 as u16)
    }

    fn moved(&mut self) {
        let (x, y) = self.pixel();
        self.input.mouse(MouseEvent::Move { x, y });
    }
}

/// Windows virtual-key code (US positions) to set-1 scan code and whether it has the E0
/// prefix.
pub fn vk_to_scancode(vk: u8) -> Option<(u8, bool)> {
    const LETTERS: [u8; 26] = [
        0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
    ];
    const NUMPAD: [u8; 10] = [0x52, 0x4f, 0x50, 0x51, 0x4b, 0x4c, 0x4d, 0x47, 0x48, 0x49];
    let plain = |c| Some((c, false));
    let ext = |c| Some((c, true));
    match vk {
        b'A'..=b'Z' => plain(LETTERS[usize::from(vk - b'A')]),
        b'1'..=b'9' => plain(vk - b'1' + 0x02),
        b'0' => plain(0x0b),
        0x60..=0x69 => plain(NUMPAD[usize::from(vk - 0x60)]),
        0x70..=0x79 => plain(vk - 0x70 + 0x3b), // F1-F10
        0x7a => plain(0x57),                    // F11
        0x7b => plain(0x58),                    // F12
        0x08 => plain(0x0e),                    // Backspace
        0x09 => plain(0x0f),                    // Tab
        0x0d => plain(0x1c),                    // Enter
        0x10 | 0xa0 => plain(0x2a),             // Shift, left Shift
        0xa1 => plain(0x36),                    // right Shift
        0x11 | 0xa2 => plain(0x1d),             // Ctrl, left Ctrl
        0xa3 => ext(0x1d),                      // right Ctrl
        0x12 | 0xa4 => plain(0x38),             // Alt, left Alt
        0xa5 => ext(0x38),                      // right Alt (AltGr)
        0x13 => ext(0x46),                      // Pause
        0x14 => plain(0x3a),                    // Caps Lock
        0x1b => plain(0x01),                    // Esc
        0x20 => plain(0x39),                    // Space
        0x21 => ext(0x49),                      // Page Up
        0x22 => ext(0x51),                      // Page Down
        0x23 => ext(0x4f),                      // End
        0x24 => ext(0x47),                      // Home
        0x25 => ext(0x4b),                      // Left
        0x26 => ext(0x48),                      // Up
        0x27 => ext(0x4d),                      // Right
        0x28 => ext(0x50),                      // Down
        0x2c => ext(0x37),                      // Print Screen
        0x2d => ext(0x52),                      // Insert
        0x2e => ext(0x53),                      // Delete
        0x5b => ext(0x5b),                      // left Windows
        0x5c => ext(0x5c),                      // right Windows
        0x5d => ext(0x5d),                      // Menu
        0x6a => plain(0x37),                    // keypad *
        0x6b => plain(0x4e),                    // keypad +
        0x6d => plain(0x4a),                    // keypad -
        0x6e => plain(0x53),                    // keypad .
        0x6f => ext(0x35),                      // keypad /
        0x90 => plain(0x45),                    // Num Lock
        0x91 => plain(0x46),                    // Scroll Lock
        0xba => plain(0x27),                    // ;
        0xbb => plain(0x0d),                    // =
        0xbc => plain(0x33),                    // ,
        0xbd => plain(0x0c),                    // -
        0xbe => plain(0x34),                    // .
        0xbf => plain(0x35),                    // /
        0xc0 => plain(0x29),                    // `
        0xdb => plain(0x1a),                    // [
        0xdc => plain(0x2b),                    // \
        0xdd => plain(0x1b),                    // ]
        0xde => plain(0x28),                    // '
        0xe2 => plain(0x56),                    // the extra ISO key (< > on Spanish keyboards)
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::scancode_to_evdev;

    #[test]
    fn keys_reach_evdev_codes() {
        let evdev = |vk| vk_to_scancode(vk).and_then(|(c, e)| scancode_to_evdev(c, e));
        assert_eq!(evdev(b'A'), Some(30)); // KEY_A
        assert_eq!(evdev(b'Q'), Some(16)); // KEY_Q
        assert_eq!(evdev(b'0'), Some(11)); // KEY_0
        assert_eq!(evdev(0x26), Some(103)); // KEY_UP
        assert_eq!(evdev(0xa5), Some(100)); // KEY_RIGHTALT
        assert_eq!(evdev(0x7b), Some(88)); // KEY_F12
        assert_eq!(evdev(0x60), Some(82)); // KEY_KP0
        assert_eq!(evdev(0xe2), Some(86)); // KEY_102ND
        // Every mapped key reaches a key code.
        for vk in 0..=255u8 {
            if let Some((c, e)) = vk_to_scancode(vk) {
                assert!(scancode_to_evdev(c, e).is_some(), "vk {vk:#x}");
            }
        }
    }

    /// Keeps what reaches the session.
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    #[async_trait::async_trait]
    impl crate::input::InputBackend for Recorder {
        async fn apply(&self, ev: &crate::input::Input) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("{ev:?}"));
            Ok(())
        }
    }

    /// A message as moonlight-common-c builds it: size (BE, without itself), type (LE), body.
    fn msg(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut m = ((4 + body.len()) as u32).to_be_bytes().to_vec();
        m.extend_from_slice(&kind.to_le_bytes());
        m.extend_from_slice(body);
        m
    }

    #[tokio::test]
    async fn moonlight_messages_reach_the_session() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let forwarder = InputForwarder::spawn(Box::new(Recorder(std::sync::Arc::clone(&seen))));
        let mut input = MoonlightInput::new(forwarder);
        let desktop = (1920, 1080);
        // Queued pointer moves are merged (only the newest counts), so wait between them.
        let wait_for = |n: usize| {
            let seen = std::sync::Arc::clone(&seen);
            async move {
                for _ in 0..200 {
                    if seen.lock().unwrap().len() >= n {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
        };
        // Key 'A' (VK 0x41) down and up: flags, key code (LE), modifiers, zero.
        input.handle(&msg(KEY_DOWN, &[0, 0x41, 0x80, 0, 0, 0]), desktop);
        input.handle(&msg(KEY_UP, &[0, 0x41, 0x80, 0, 0, 0]), desktop);
        // Absolute move to the middle of a 1280x720 view (reference sizes less one).
        let mut abs = Vec::new();
        for v in [640i16, 360, 0, 1279, 719] {
            abs.extend_from_slice(&v.to_be_bytes());
        }
        input.handle(&msg(MOUSE_ABS, &abs), desktop);
        wait_for(3).await;
        // Relative move, then a left click.
        input.handle(&msg(MOUSE_REL, &[0, 10, 0xff, 0xf6]), desktop);
        input.handle(&msg(BUTTON_DOWN, &[1]), desktop);
        input.handle(&msg(BUTTON_UP, &[1]), desktop);
        // One notch down (negative is towards the user).
        input.handle(&msg(SCROLL, &(-120i16).to_be_bytes()), desktop);
        wait_for(7).await;
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            [
                "Key(30, true)",
                "Key(30, false)",
                "Motion(960.0, 540.0)",
                "Motion(970.0, 530.0)",
                "Button(272, true)",
                "Button(272, false)",
                "Scroll { axis: 0, steps: 1 }",
            ]
        );
    }
}

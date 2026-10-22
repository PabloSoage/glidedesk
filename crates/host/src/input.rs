//! RDP input to Mutter RemoteDesktop calls.

use ironrdp_server::{KeyboardEvent, MouseButton, MouseEvent, RdpServerInputHandler};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::mutter::RemoteDesktopSessionProxy;

// Linux evdev button codes (linux/input-event-codes.h).
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
const BTN_SIDE: i32 = 0x113;
const BTN_EXTRA: i32 = 0x114;
const KEY_LEFTSHIFT: u32 = 42;

/// RDP wheel units per notch (MS-RDPBCGR 2.2.8.1.1.3.1.1.3).
const WHEEL_DELTA: i32 = 120;

#[derive(Debug)]
enum Input {
    Key(u32, bool),
    Keysym(u32, bool),
    Motion(f64, f64),
    Button(i32, bool),
    Scroll { axis: u32, steps: i32 },
}

/// Translates a set-1 scan code (with the E0 prefix flag) to an evdev key code.
///
/// Without the prefix, codes 1..=0x58 are the evdev codes themselves. The E0 codes are
/// the navigation block, keypad Enter and Divide, right Ctrl/Alt and the Windows keys.
pub fn scancode_to_evdev(code: u8, extended: bool) -> Option<u32> {
    let code = u32::from(code);
    if !extended {
        return match code {
            1..=0x58 => Some(code),
            0x70 => Some(93),  // KEY_KATAKANAHIRAGANA
            0x73 => Some(89),  // KEY_RO
            0x79 => Some(92),  // KEY_HENKAN
            0x7b => Some(94),  // KEY_MUHENKAN
            0x7d => Some(124), // KEY_YEN
            _ => None,
        };
    }
    Some(match code {
        0x1c => 96,  // KEY_KPENTER
        0x1d => 97,  // KEY_RIGHTCTRL
        0x35 => 98,  // KEY_KPSLASH
        0x37 => 99,  // KEY_SYSRQ (Print Screen)
        0x38 => 100, // KEY_RIGHTALT (AltGr)
        0x46 => 119, // KEY_PAUSE (Ctrl+Break)
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4b => 105, // KEY_LEFT
        0x4d => 106, // KEY_RIGHT
        0x4f => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5b => 125, // KEY_LEFTMETA
        0x5c => 126, // KEY_RIGHTMETA
        0x5d => 127, // KEY_COMPOSE (menu)
        _ => return None,
    })
}

/// X keysym for a Unicode code point (keysymdef.h: Latin-1 maps directly, the rest is
/// 0x01000000 + code point).
fn unicode_keysym(cp: u16) -> u32 {
    let cp = u32::from(cp);
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
        cp
    } else {
        0x0100_0000 | cp
    }
}

/// Input handler given to IronRDP. Its callbacks are synchronous, so events go through a
/// channel to a task that makes the D-Bus calls in order.
#[derive(Clone)]
pub struct InputForwarder {
    tx: mpsc::UnboundedSender<Input>,
    /// Accumulated wheel deltas that have not reached a full notch yet.
    wheel: (i32, i32),
    /// Keys held down. Mutter rejects a release of a key that is not down and a second
    /// press of one that is; clients send both (key repeat, focus synchronisation).
    pressed: std::collections::HashSet<u32>,
}

impl InputForwarder {
    pub fn spawn(remote: RemoteDesktopSessionProxy<'static>, stream_path: String) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Input>();
        tokio::spawn(async move {
            // Mutter drops the first key event of a remote desktop session (the virtual
            // keyboard is set up on it): spend it on a Shift tap, which types nothing.
            let _ = remote.notify_keyboard_keycode(KEY_LEFTSHIFT, true).await;
            let _ = remote.notify_keyboard_keycode(KEY_LEFTSHIFT, false).await;
            let mut last_pos = None;
            let mut next = None;
            while let Some(mut ev) = match next.take() {
                Some(ev) => Some(ev),
                None => rx.recv().await,
            } {
                // Only the newest of several queued pointer positions matters.
                if matches!(ev, Input::Motion(..)) {
                    while let Ok(e) = rx.try_recv() {
                        if matches!(e, Input::Motion(..)) {
                            ev = e;
                        } else {
                            next = Some(e);
                            break;
                        }
                    }
                }
                debug!(?ev, queued = rx.len(), "Input");
                let r = match ev {
                    Input::Key(code, down) => remote.notify_keyboard_keycode(code, down).await,
                    Input::Keysym(sym, down) => remote.notify_keyboard_keysym(sym, down).await,
                    Input::Motion(x, y) => {
                        // Clients repeat the position with every button event.
                        if last_pos == Some((x, y)) {
                            continue;
                        }
                        last_pos = Some((x, y));
                        remote
                            .notify_pointer_motion_absolute(&stream_path, x, y)
                            .await
                    }
                    Input::Button(b, down) => remote.notify_pointer_button(b, down).await,
                    Input::Scroll { axis, steps } => {
                        remote.notify_pointer_axis_discrete(axis, steps).await
                    }
                };
                if let Err(e) = r {
                    warn!(%e, "Input call failed");
                }
            }
        });
        Self {
            tx,
            wheel: (0, 0),
            pressed: Default::default(),
        }
    }

    fn send(&self, ev: Input) {
        let _ = self.tx.send(ev);
    }

    fn scroll(&mut self, axis: u32, value: i32) {
        let acc = if axis == 0 {
            &mut self.wheel.0
        } else {
            &mut self.wheel.1
        };
        *acc += value;
        let steps = *acc / WHEEL_DELTA;
        if steps != 0 {
            *acc -= steps * WHEEL_DELTA;
            // RDP: positive is away from the user (up / right). libinput: positive is down
            // for the vertical axis and right for the horizontal one.
            let steps = if axis == 0 { -steps } else { steps };
            self.send(Input::Scroll { axis, steps });
        }
    }
}

fn button_code(b: MouseButton) -> Option<i32> {
    Some(match b {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::X1 => BTN_SIDE,
        MouseButton::X2 => BTN_EXTRA,
        _ => return None,
    })
}

impl RdpServerInputHandler for InputForwarder {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended }
            | KeyboardEvent::Released { code, extended } => {
                let down = matches!(event, KeyboardEvent::Pressed { .. });
                match scancode_to_evdev(code, extended) {
                    // Mutter generates key repeat itself, so a repeated press is dropped.
                    Some(k) if down && self.pressed.insert(k) => self.send(Input::Key(k, true)),
                    Some(k) if !down && self.pressed.remove(&k) => self.send(Input::Key(k, false)),
                    Some(_) => {}
                    None => debug!(code, extended, "Unmapped scan code"),
                }
            }
            KeyboardEvent::UnicodePressed(cp) => self.send(Input::Keysym(unicode_keysym(cp), true)),
            KeyboardEvent::UnicodeReleased(cp) => {
                self.send(Input::Keysym(unicode_keysym(cp), false))
            }
            KeyboardEvent::Synchronize(_) => {
                // Focus came back to the client window: release whatever we think is held.
                for k in std::mem::take(&mut self.pressed) {
                    self.send(Input::Key(k, false));
                }
            }
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => self.send(Input::Motion(f64::from(x), f64::from(y))),
            MouseEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                self.send(Input::Motion(f64::from(x), f64::from(y)));
                if let Some(b) = button_code(button) {
                    self.send(Input::Button(b, pressed));
                }
            }
            MouseEvent::ButtonRel {
                button, pressed, ..
            } => {
                if let Some(b) = button_code(button) {
                    self.send(Input::Button(b, pressed));
                }
            }
            MouseEvent::VerticalScroll { value } => self.scroll(0, i32::from(value)),
            MouseEvent::HorizontalScroll { value } => self.scroll(1, i32::from(value)),
            MouseEvent::Scroll { x, y } => {
                self.scroll(1, x);
                self.scroll(0, y);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_scancodes_are_evdev_codes() {
        assert_eq!(scancode_to_evdev(0x1e, false), Some(30)); // A
        assert_eq!(scancode_to_evdev(0x01, false), Some(1)); // Esc
        assert_eq!(scancode_to_evdev(0x58, false), Some(88)); // F12
    }

    #[test]
    fn extended_scancodes() {
        assert_eq!(scancode_to_evdev(0x48, true), Some(103)); // Up
        assert_eq!(scancode_to_evdev(0x38, true), Some(100)); // AltGr
        assert_eq!(scancode_to_evdev(0x5b, true), Some(125)); // Windows key
        assert_eq!(scancode_to_evdev(0x10, true), None);
    }

    #[test]
    fn keysyms() {
        assert_eq!(unicode_keysym('a' as u16), 0x61);
        assert_eq!(unicode_keysym(0xf1), 0xf1); // ñ
        assert_eq!(unicode_keysym(0x20ac), 0x0100_20ac); // €
    }
}

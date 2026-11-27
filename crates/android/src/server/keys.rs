//! Keyboard of the phone server: the wire's PC/AT set-1 scan codes as Android key codes, and
//! the modifier state Android expects on every key event (`KeyEvent.getMetaState()`: apps
//! read Shift and Ctrl from it, not from earlier events).

/// `android.view.KeyEvent` key codes used outside the table.
pub mod code {
    pub const HOME: i32 = 3;
    pub const BACK: i32 = 4;
    pub const SHIFT_LEFT: i32 = 59;
    pub const SHIFT_RIGHT: i32 = 60;
    pub const ALT_LEFT: i32 = 57;
    pub const ALT_RIGHT: i32 = 58;
    pub const NOTIFICATION: i32 = 83;
    pub const CTRL_LEFT: i32 = 113;
    pub const CTRL_RIGHT: i32 = 114;
    pub const CAPS_LOCK: i32 = 115;
    pub const META_LEFT: i32 = 117;
    pub const META_RIGHT: i32 = 118;
    pub const NUM_LOCK: i32 = 143;
    pub const APP_SWITCH: i32 = 187;
}

/// `KeyEvent.META_*` bits.
pub mod meta {
    pub const SHIFT_ON: i32 = 0x1;
    pub const ALT_ON: i32 = 0x2;
    pub const ALT_LEFT_ON: i32 = 0x10;
    pub const ALT_RIGHT_ON: i32 = 0x20;
    pub const SHIFT_LEFT_ON: i32 = 0x40;
    pub const SHIFT_RIGHT_ON: i32 = 0x80;
    pub const CTRL_ON: i32 = 0x1000;
    pub const CTRL_LEFT_ON: i32 = 0x2000;
    pub const CTRL_RIGHT_ON: i32 = 0x4000;
    pub const META_ON: i32 = 0x10000;
    pub const META_LEFT_ON: i32 = 0x20000;
    pub const META_RIGHT_ON: i32 = 0x40000;
    pub const CAPS_LOCK_ON: i32 = 0x100000;
    pub const NUM_LOCK_ON: i32 = 0x200000;
}

/// Android key code of a set-1 scan code (`extended`: the E0 prefix), if it has one.
pub fn keycode(scancode: u8, extended: bool) -> Option<i32> {
    let k = if extended {
        match scancode {
            0x10 => 88,  // previous track: MEDIA_PREVIOUS
            0x19 => 87,  // next track: MEDIA_NEXT
            0x1c => 160, // keypad Enter: NUMPAD_ENTER
            0x1d => code::CTRL_RIGHT,
            0x20 => 164, // mute: VOLUME_MUTE
            0x22 => 85,  // play/pause: MEDIA_PLAY_PAUSE
            0x24 => 86,  // stop: MEDIA_STOP
            0x2e => 25,  // volume down
            0x30 => 24,  // volume up
            0x35 => 154, // keypad /: NUMPAD_DIVIDE
            0x37 => 120, // Print Screen: SYSRQ
            0x38 => code::ALT_RIGHT,
            0x47 => 122, // Home: MOVE_HOME
            0x48 => 19,  // Up: DPAD_UP
            0x49 => 92,  // Page Up
            0x4b => 21,  // Left: DPAD_LEFT
            0x4d => 22,  // Right: DPAD_RIGHT
            0x4f => 123, // End: MOVE_END
            0x50 => 20,  // Down: DPAD_DOWN
            0x51 => 93,  // Page Down
            0x52 => 124, // Insert
            0x53 => 112, // Delete: FORWARD_DEL
            0x5b => code::META_LEFT,
            0x5c => code::META_RIGHT,
            0x5d => 82,  // Menu
            0x6a => code::BACK, // browser Back
            _ => return None,
        }
    } else {
        match scancode {
            0x01 => 111, // Escape
            0x02..=0x0a => 8 + i32::from(scancode - 0x02), // 1..9: KEYCODE_1..KEYCODE_9
            0x0b => 7,  // 0
            0x0c => 69, // -
            0x0d => 70, // =
            0x0e => 67, // Backspace: DEL
            0x0f => 61, // Tab
            0x10 => 45, // Q
            0x11 => 51, // W
            0x12 => 33, // E
            0x13 => 46, // R
            0x14 => 48, // T
            0x15 => 53, // Y
            0x16 => 49, // U
            0x17 => 37, // I
            0x18 => 43, // O
            0x19 => 44, // P
            0x1a => 71, // [
            0x1b => 72, // ]
            0x1c => 66, // Enter
            0x1d => code::CTRL_LEFT,
            0x1e => 29, // A
            0x1f => 47, // S
            0x20 => 32, // D
            0x21 => 34, // F
            0x22 => 35, // G
            0x23 => 36, // H
            0x24 => 38, // J
            0x25 => 39, // K
            0x26 => 40, // L
            0x27 => 74, // ;
            0x28 => 75, // '
            0x29 => 68, // `
            0x2a => code::SHIFT_LEFT,
            0x2b => 73, // \
            0x2c => 54, // Z
            0x2d => 52, // X
            0x2e => 31, // C
            0x2f => 50, // V
            0x30 => 30, // B
            0x31 => 42, // N
            0x32 => 41, // M
            0x33 => 55, // ,
            0x34 => 56, // .
            0x35 => 76, // /
            0x36 => code::SHIFT_RIGHT,
            0x37 => 155, // keypad *: NUMPAD_MULTIPLY
            0x38 => code::ALT_LEFT,
            0x39 => 62, // Space
            0x3a => code::CAPS_LOCK,
            0x3b..=0x44 => 131 + i32::from(scancode - 0x3b), // F1..F10
            0x45 => code::NUM_LOCK,
            0x46 => 116, // Scroll Lock
            0x47 => 151, // keypad 7
            0x48 => 152, // keypad 8
            0x49 => 153, // keypad 9
            0x4a => 156, // keypad -: NUMPAD_SUBTRACT
            0x4b => 148, // keypad 4
            0x4c => 149, // keypad 5
            0x4d => 150, // keypad 6
            0x4e => 157, // keypad +: NUMPAD_ADD
            0x4f => 145, // keypad 1
            0x50 => 146, // keypad 2
            0x51 => 147, // keypad 3
            0x52 => 144, // keypad 0
            0x53 => 158, // keypad .: NUMPAD_DOT
            // The ISO key left of Z (Linux KEY_102ND), which Android's Generic.kl maps to
            // BACKSLASH.
            0x56 => 73,
            0x57 => 141, // F11
            0x58 => 142, // F12
            0x70 => 215, // Katakana/Hiragana
            0x73 => 217, // Ro
            0x79 => 214, // Henkan
            0x7b => 213, // Muhenkan
            0x7d => 216, // Yen
            _ => return None,
        }
    };
    Some(k)
}

/// Modifier bit of a held key code.
fn modifier(keycode: i32) -> i32 {
    match keycode {
        code::SHIFT_LEFT => meta::SHIFT_ON | meta::SHIFT_LEFT_ON,
        code::SHIFT_RIGHT => meta::SHIFT_ON | meta::SHIFT_RIGHT_ON,
        code::CTRL_LEFT => meta::CTRL_ON | meta::CTRL_LEFT_ON,
        code::CTRL_RIGHT => meta::CTRL_ON | meta::CTRL_RIGHT_ON,
        code::ALT_LEFT => meta::ALT_ON | meta::ALT_LEFT_ON,
        code::ALT_RIGHT => meta::ALT_ON | meta::ALT_RIGHT_ON,
        code::META_LEFT => meta::META_ON | meta::META_LEFT_ON,
        code::META_RIGHT => meta::META_ON | meta::META_RIGHT_ON,
        _ => 0,
    }
}

/// Keys held down and the lock toggles, for the meta state and to release everything when
/// the client leaves.
#[derive(Debug)]
pub struct KeyState {
    held: Vec<i32>,
    caps: bool,
    num: bool,
}

impl Default for KeyState {
    fn default() -> Self {
        // Num Lock on, as desktops start: the keypad types digits.
        Self {
            held: Vec::new(),
            caps: false,
            num: true,
        }
    }
}

impl KeyState {
    /// Takes a key change; returns the meta state for its event. Repeats (a down for a key
    /// already held) keep the state as it is.
    pub fn key(&mut self, keycode: i32, down: bool) -> i32 {
        if down {
            if !self.held.contains(&keycode) {
                self.held.push(keycode);
                match keycode {
                    code::CAPS_LOCK => self.caps = !self.caps,
                    code::NUM_LOCK => self.num = !self.num,
                    _ => {}
                }
            }
        } else {
            self.held.retain(|&k| k != keycode);
        }
        self.meta()
    }

    pub fn meta(&self) -> i32 {
        let mut m = self.held.iter().fold(0, |m, &k| m | modifier(k));
        if self.caps {
            m |= meta::CAPS_LOCK_ON;
        }
        if self.num {
            m |= meta::NUM_LOCK_ON;
        }
        m
    }

    /// The keys still down, cleared (each needs its up event).
    pub fn release_all(&mut self) -> Vec<i32> {
        std::mem::take(&mut self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_extended_keys() {
        assert_eq!(keycode(0x1e, false), Some(29)); // A
        assert_eq!(keycode(0x2c, false), Some(54)); // Z
        assert_eq!(keycode(0x02, false), Some(8)); // 1
        assert_eq!(keycode(0x0a, false), Some(16)); // 9
        assert_eq!(keycode(0x0b, false), Some(7)); // 0
        assert_eq!(keycode(0x3b, false), Some(131)); // F1
        assert_eq!(keycode(0x44, false), Some(140)); // F10
        assert_eq!(keycode(0x58, false), Some(142)); // F12
        assert_eq!(keycode(0x48, true), Some(19)); // Up
        assert_eq!(keycode(0x48, false), Some(152)); // keypad 8
        assert_eq!(keycode(0x1d, true), Some(code::CTRL_RIGHT));
        assert_eq!(keycode(0x7f, false), None);
        assert_eq!(keycode(0x01, true), None);
    }

    #[test]
    fn modifiers_and_locks() {
        let mut k = KeyState::default();
        assert_eq!(k.meta(), meta::NUM_LOCK_ON);
        let shift = meta::SHIFT_ON | meta::SHIFT_LEFT_ON;
        assert_eq!(k.key(code::SHIFT_LEFT, true) & shift, shift);
        let m = k.key(29, true);
        assert_ne!(m & meta::SHIFT_ON, 0);
        k.key(29, false);
        let m = k.key(code::SHIFT_LEFT, false);
        assert_eq!(m & meta::SHIFT_ON, 0);
        // Caps Lock toggles on its down, and a repeat does not toggle it back.
        k.key(code::CAPS_LOCK, true);
        k.key(code::CAPS_LOCK, true);
        assert_ne!(k.key(code::CAPS_LOCK, false) & meta::CAPS_LOCK_ON, 0);
        k.key(code::CAPS_LOCK, true);
        assert_eq!(k.key(code::CAPS_LOCK, false) & meta::CAPS_LOCK_ON, 0);
    }

    #[test]
    fn release_all_lists_the_held_keys() {
        let mut k = KeyState::default();
        k.key(code::CTRL_LEFT, true);
        k.key(46, true);
        let mut held = k.release_all();
        held.sort_unstable();
        assert_eq!(held, vec![46, code::CTRL_LEFT]);
        assert_eq!(k.meta() & meta::CTRL_ON, 0);
        assert!(k.release_all().is_empty());
    }
}

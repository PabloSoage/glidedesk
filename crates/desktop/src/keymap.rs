//! Physical keys (winit `KeyCode`, positions on a US layout) to PC/AT set-1 scan codes, which
//! RDP and the native protocol carry. The server maps them to its own layout, so any
//! keyboard layout works as long as both ends use the same one.

use winit::keyboard::KeyCode;

/// Scan code and whether it has the E0 prefix.
pub fn scancode(key: KeyCode) -> Option<(u8, bool)> {
    use KeyCode::*;
    let (code, ext) = match key {
        Escape => (0x01, false),
        Digit1 => (0x02, false),
        Digit2 => (0x03, false),
        Digit3 => (0x04, false),
        Digit4 => (0x05, false),
        Digit5 => (0x06, false),
        Digit6 => (0x07, false),
        Digit7 => (0x08, false),
        Digit8 => (0x09, false),
        Digit9 => (0x0a, false),
        Digit0 => (0x0b, false),
        Minus => (0x0c, false),
        Equal => (0x0d, false),
        Backspace => (0x0e, false),
        Tab => (0x0f, false),
        KeyQ => (0x10, false),
        KeyW => (0x11, false),
        KeyE => (0x12, false),
        KeyR => (0x13, false),
        KeyT => (0x14, false),
        KeyY => (0x15, false),
        KeyU => (0x16, false),
        KeyI => (0x17, false),
        KeyO => (0x18, false),
        KeyP => (0x19, false),
        BracketLeft => (0x1a, false),
        BracketRight => (0x1b, false),
        Enter => (0x1c, false),
        ControlLeft => (0x1d, false),
        KeyA => (0x1e, false),
        KeyS => (0x1f, false),
        KeyD => (0x20, false),
        KeyF => (0x21, false),
        KeyG => (0x22, false),
        KeyH => (0x23, false),
        KeyJ => (0x24, false),
        KeyK => (0x25, false),
        KeyL => (0x26, false),
        Semicolon => (0x27, false),
        Quote => (0x28, false),
        Backquote => (0x29, false),
        ShiftLeft => (0x2a, false),
        Backslash => (0x2b, false),
        KeyZ => (0x2c, false),
        KeyX => (0x2d, false),
        KeyC => (0x2e, false),
        KeyV => (0x2f, false),
        KeyB => (0x30, false),
        KeyN => (0x31, false),
        KeyM => (0x32, false),
        Comma => (0x33, false),
        Period => (0x34, false),
        Slash => (0x35, false),
        ShiftRight => (0x36, false),
        NumpadMultiply => (0x37, false),
        AltLeft => (0x38, false),
        Space => (0x39, false),
        CapsLock => (0x3a, false),
        F1 => (0x3b, false),
        F2 => (0x3c, false),
        F3 => (0x3d, false),
        F4 => (0x3e, false),
        F5 => (0x3f, false),
        F6 => (0x40, false),
        F7 => (0x41, false),
        F8 => (0x42, false),
        F9 => (0x43, false),
        F10 => (0x44, false),
        NumLock => (0x45, false),
        ScrollLock => (0x46, false),
        Numpad7 => (0x47, false),
        Numpad8 => (0x48, false),
        Numpad9 => (0x49, false),
        NumpadSubtract => (0x4a, false),
        Numpad4 => (0x4b, false),
        Numpad5 => (0x4c, false),
        Numpad6 => (0x4d, false),
        NumpadAdd => (0x4e, false),
        Numpad1 => (0x4f, false),
        Numpad2 => (0x50, false),
        Numpad3 => (0x51, false),
        Numpad0 => (0x52, false),
        NumpadDecimal => (0x53, false),
        IntlBackslash => (0x56, false),
        F11 => (0x57, false),
        F12 => (0x58, false),
        IntlRo => (0x73, false),
        IntlYen => (0x7d, false),
        NumpadEnter => (0x1c, true),
        ControlRight => (0x1d, true),
        NumpadDivide => (0x35, true),
        PrintScreen => (0x37, true),
        AltRight => (0x38, true),
        Pause => (0x46, true),
        Home => (0x47, true),
        ArrowUp => (0x48, true),
        PageUp => (0x49, true),
        ArrowLeft => (0x4b, true),
        ArrowRight => (0x4d, true),
        End => (0x4f, true),
        ArrowDown => (0x50, true),
        PageDown => (0x51, true),
        Insert => (0x52, true),
        Delete => (0x53, true),
        SuperLeft => (0x5b, true),
        SuperRight => (0x5c, true),
        ContextMenu => (0x5d, true),
        _ => return None,
    };
    Some((code, ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn some_keys() {
        assert_eq!(scancode(KeyCode::KeyA), Some((0x1e, false)));
        assert_eq!(scancode(KeyCode::ArrowUp), Some((0x48, true)));
        assert_eq!(scancode(KeyCode::AltRight), Some((0x38, true)));
        assert_eq!(scancode(KeyCode::F24), None);
    }
}

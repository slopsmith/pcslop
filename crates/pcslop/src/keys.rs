//! Key names and host key codes to PS/2 set-1 scancodes (0xE0xx for extended keys).

pub fn by_name(name: &str) -> Option<u16> {
    let n = name.to_ascii_lowercase();
    Some(match n.as_str() {
        "esc" | "escape" => 0x01,
        "backspace" => 0x0e,
        "tab" => 0x0f,
        "enter" | "return" => 0x1c,
        "lctrl" | "ctrl" => 0x1d,
        "lshift" | "shift" => 0x2a,
        "rshift" => 0x36,
        "lalt" | "alt" => 0x38,
        "space" => 0x39,
        "capslock" => 0x3a,
        "f1" => 0x3b,
        "f2" => 0x3c,
        "f3" => 0x3d,
        "f4" => 0x3e,
        "f5" => 0x3f,
        "f6" => 0x40,
        "f7" => 0x41,
        "f8" => 0x42,
        "f9" => 0x43,
        "f10" => 0x44,
        "f11" => 0x57,
        "f12" => 0x58,
        "numlock" => 0x45,
        "scrolllock" => 0x46,
        "rctrl" => 0xe01d,
        "ralt" => 0xe038,
        "home" => 0xe047,
        "up" => 0xe048,
        "pageup" | "pgup" => 0xe049,
        "left" => 0xe04b,
        "right" => 0xe04d,
        "end" => 0xe04f,
        "down" => 0xe050,
        "pagedown" | "pgdn" => 0xe051,
        "insert" => 0xe052,
        "delete" | "del" => 0xe053,
        "lwin" => 0xe05b,
        "rwin" => 0xe05c,
        "menu" => 0xe05d,
        _ => {
            let mut chars = n.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            return char_key(c).map(|(code, _)| code);
        }
    })
}

/// Scancode and whether Shift is needed, for a US layout.
pub fn char_key(c: char) -> Option<(u16, bool)> {
    const ROW: [(&str, &str, u16); 4] = [
        ("1234567890-=", "!@#$%^&*()_+", 0x02),
        ("qwertyuiop[]", "QWERTYUIOP{}", 0x10),
        ("asdfghjkl;'`", "ASDFGHJKL:\"~", 0x1e),
        ("zxcvbnm,./", "ZXCVBNM<>?", 0x2c),
    ];
    match c {
        ' ' => return Some((0x39, false)),
        '\n' => return Some((0x1c, false)),
        '\t' => return Some((0x0f, false)),
        '\\' => return Some((0x2b, false)),
        '|' => return Some((0x2b, true)),
        _ => {}
    }
    for (plain, shifted, base) in ROW {
        if let Some(i) = plain.find(c) {
            return Some((base + i as u16, false));
        }
        if let Some(i) = shifted.find(c) {
            return Some((base + i as u16, true));
        }
    }
    None
}

/// The scancode for the key at `code`'s position on a US keyboard. Cmd is the Windows key.
#[cfg(feature = "window")]
pub fn from_keycode(code: winit::keyboard::KeyCode) -> Option<u16> {
    use winit::keyboard::KeyCode::*;
    Some(match code {
        Digit0 => 0x0b,
        Digit1 => 0x02,
        Digit2 => 0x03,
        Digit3 => 0x04,
        Digit4 => 0x05,
        Digit5 => 0x06,
        Digit6 => 0x07,
        Digit7 => 0x08,
        Digit8 => 0x09,
        Digit9 => 0x0a,
        KeyA => 0x1e,
        KeyB => 0x30,
        KeyC => 0x2e,
        KeyD => 0x20,
        KeyE => 0x12,
        KeyF => 0x21,
        KeyG => 0x22,
        KeyH => 0x23,
        KeyI => 0x17,
        KeyJ => 0x24,
        KeyK => 0x25,
        KeyL => 0x26,
        KeyM => 0x32,
        KeyN => 0x31,
        KeyO => 0x18,
        KeyP => 0x19,
        KeyQ => 0x10,
        KeyR => 0x13,
        KeyS => 0x1f,
        KeyT => 0x14,
        KeyU => 0x16,
        KeyV => 0x2f,
        KeyW => 0x11,
        KeyX => 0x2d,
        KeyY => 0x15,
        KeyZ => 0x2c,
        F1 => 0x3b,
        F2 => 0x3c,
        F3 => 0x3d,
        F4 => 0x3e,
        F5 => 0x3f,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        F11 => 0x57,
        F12 => 0x58,
        ArrowDown => 0xe050,
        ArrowLeft => 0xe04b,
        ArrowRight => 0xe04d,
        ArrowUp => 0xe048,
        Quote => 0x28,
        Backquote => 0x29,
        Backslash => 0x2b,
        Comma => 0x33,
        Equal => 0x0d,
        BracketLeft => 0x1a,
        Minus => 0x0c,
        Period => 0x34,
        BracketRight => 0x1b,
        Semicolon => 0x27,
        Slash => 0x35,
        IntlBackslash => 0x56,
        Backspace => 0x0e,
        Delete => 0xe053,
        End => 0xe04f,
        Enter => 0x1c,
        Escape => 0x01,
        Home => 0xe047,
        Insert => 0xe052,
        ContextMenu => 0xe05d,
        PageDown => 0xe051,
        PageUp => 0xe049,
        Space => 0x39,
        Tab => 0x0f,
        NumLock => 0x45,
        CapsLock => 0x3a,
        ScrollLock => 0x46,
        PrintScreen => 0xe037,
        ShiftLeft => 0x2a,
        ShiftRight => 0x36,
        ControlLeft => 0x1d,
        ControlRight => 0xe01d,
        AltLeft => 0x38,
        AltRight => 0xe038,
        SuperLeft => 0xe05b,
        SuperRight => 0xe05c,
        Numpad0 => 0x52,
        Numpad1 => 0x4f,
        Numpad2 => 0x50,
        Numpad3 => 0x51,
        Numpad4 => 0x4b,
        Numpad5 => 0x4c,
        Numpad6 => 0x4d,
        Numpad7 => 0x47,
        Numpad8 => 0x48,
        Numpad9 => 0x49,
        NumpadDecimal => 0x53,
        NumpadDivide => 0xe035,
        NumpadMultiply => 0x37,
        NumpadSubtract => 0x4a,
        NumpadAdd => 0x4e,
        NumpadEnter => 0xe01c,
        _ => return None,
    })
}

#[cfg(all(test, feature = "window"))]
mod tests {
    use super::*;
    use winit::keyboard::KeyCode;

    #[test]
    fn letters_and_digits_match_the_typing_table() {
        let letters = [
            KeyCode::KeyA, KeyCode::KeyB, KeyCode::KeyC, KeyCode::KeyD, KeyCode::KeyE, KeyCode::KeyF, KeyCode::KeyG, KeyCode::KeyH, KeyCode::KeyI,
            KeyCode::KeyJ, KeyCode::KeyK, KeyCode::KeyL, KeyCode::KeyM, KeyCode::KeyN, KeyCode::KeyO, KeyCode::KeyP, KeyCode::KeyQ, KeyCode::KeyR,
            KeyCode::KeyS, KeyCode::KeyT, KeyCode::KeyU, KeyCode::KeyV, KeyCode::KeyW, KeyCode::KeyX, KeyCode::KeyY, KeyCode::KeyZ,
        ];
        for (code, c) in letters.into_iter().zip('a'..='z') {
            assert_eq!(from_keycode(code), char_key(c).map(|k| k.0), "{c}");
        }
        let digits = [
            KeyCode::Digit0, KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4,
            KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9,
        ];
        for (code, c) in digits.into_iter().zip('0'..='9') {
            assert_eq!(from_keycode(code), char_key(c).map(|k| k.0), "{c}");
        }
    }

    #[test]
    fn modifiers_and_named_keys_match_their_names() {
        for (code, name) in [
            (KeyCode::ControlLeft, "lctrl"),
            (KeyCode::ControlRight, "rctrl"),
            (KeyCode::AltLeft, "lalt"),
            (KeyCode::AltRight, "ralt"),
            (KeyCode::ShiftLeft, "lshift"),
            (KeyCode::ShiftRight, "rshift"),
            (KeyCode::SuperLeft, "lwin"),
            (KeyCode::SuperRight, "rwin"),
            (KeyCode::ContextMenu, "menu"),
            (KeyCode::Escape, "esc"),
            (KeyCode::Enter, "enter"),
            (KeyCode::ArrowUp, "up"),
            (KeyCode::Delete, "delete"),
            (KeyCode::F11, "f11"),
            (KeyCode::Backquote, "`"),
            (KeyCode::Quote, "'"),
        ] {
            assert_eq!(from_keycode(code), by_name(name), "{name}");
        }
        assert_eq!(from_keycode(KeyCode::NumpadEnter), Some(0xe01c));
        assert_eq!(from_keycode(KeyCode::Fn), None);
    }
}

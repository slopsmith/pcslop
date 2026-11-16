//! Key names and minifb keys to PS/2 set-1 scancodes (0xE0xx for extended keys).

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

#[cfg(feature = "window")]
pub fn from_minifb(k: minifb::Key) -> Option<u16> {
    use minifb::Key::*;
    Some(match k {
        Key0 => 0x0b,
        Key1 => 0x02,
        Key2 => 0x03,
        Key3 => 0x04,
        Key4 => 0x05,
        Key5 => 0x06,
        Key6 => 0x07,
        Key7 => 0x08,
        Key8 => 0x09,
        Key9 => 0x0a,
        A => 0x1e,
        B => 0x30,
        C => 0x2e,
        D => 0x20,
        E => 0x12,
        F => 0x21,
        G => 0x22,
        H => 0x23,
        I => 0x17,
        J => 0x24,
        K => 0x25,
        L => 0x26,
        M => 0x32,
        N => 0x31,
        O => 0x18,
        P => 0x19,
        Q => 0x10,
        R => 0x13,
        S => 0x1f,
        T => 0x14,
        U => 0x16,
        V => 0x2f,
        W => 0x11,
        X => 0x2d,
        Y => 0x15,
        Z => 0x2c,
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
        Down => 0xe050,
        Left => 0xe04b,
        Right => 0xe04d,
        Up => 0xe048,
        Apostrophe => 0x28,
        Backquote => 0x29,
        Backslash => 0x2b,
        Comma => 0x33,
        Equal => 0x0d,
        LeftBracket => 0x1a,
        Minus => 0x0c,
        Period => 0x34,
        RightBracket => 0x1b,
        Semicolon => 0x27,
        Slash => 0x35,
        Backspace => 0x0e,
        Delete => 0xe053,
        End => 0xe04f,
        Enter => 0x1c,
        Escape => 0x01,
        Home => 0xe047,
        Insert => 0xe052,
        Menu => 0xe05d,
        PageDown => 0xe051,
        PageUp => 0xe049,
        Space => 0x39,
        Tab => 0x0f,
        NumLock => 0x45,
        CapsLock => 0x3a,
        ScrollLock => 0x46,
        LeftShift => 0x2a,
        RightShift => 0x36,
        LeftCtrl => 0x1d,
        RightCtrl => 0xe01d,
        NumPad0 => 0x52,
        NumPad1 => 0x4f,
        NumPad2 => 0x50,
        NumPad3 => 0x51,
        NumPad4 => 0x4b,
        NumPad5 => 0x4c,
        NumPad6 => 0x4d,
        NumPad7 => 0x47,
        NumPad8 => 0x48,
        NumPad9 => 0x49,
        NumPadDot => 0x53,
        NumPadSlash => 0xe035,
        NumPadAsterisk => 0x37,
        NumPadMinus => 0x4a,
        NumPadPlus => 0x4e,
        NumPadEnter => 0xe01c,
        LeftAlt => 0x38,
        RightAlt => 0xe038,
        LeftSuper => 0xe05b,
        RightSuper => 0xe05c,
        _ => return None,
    })
}

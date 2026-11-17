//! X11 keysyms, as VNC clients send them, to PS/2 set-1 scancodes. Values are
//! from X11's keysymdef.h.

/// The scancode of the key that types `keysym` on a US layout. Shifted
/// symbols map to their unshifted key, since clients send Shift themselves.
pub fn scancode(keysym: u32) -> Option<u16> {
    Some(match keysym {
        0x20..=0x7e => return crate::keys::char_key(char::from(keysym as u8)).map(|(code, _)| code),
        0xff08 => 0x0e,          // BackSpace
        0xff09 | 0xfe20 => 0x0f, // Tab, ISO_Left_Tab
        0xff0d => 0x1c,          // Return
        0xff14 => 0x46,          // Scroll_Lock
        0xff15 => 0x54,          // Sys_Req
        0xff1b => 0x01,          // Escape
        0xff50 => 0xe047,        // Home
        0xff51 => 0xe04b,        // Left
        0xff52 => 0xe048,        // Up
        0xff53 => 0xe04d,        // Right
        0xff54 => 0xe050,        // Down
        0xff55 => 0xe049,        // Page_Up
        0xff56 => 0xe051,        // Page_Down
        0xff57 => 0xe04f,        // End
        0xff61 => 0xe037,        // Print
        0xff63 => 0xe052,        // Insert
        0xff67 => 0xe05d,        // Menu
        0xff7f => 0x45,          // Num_Lock
        0xff8d => 0xe01c,        // KP_Enter
        0xff95 => 0x47,          // KP_Home
        0xff96 => 0x4b,          // KP_Left
        0xff97 => 0x48,          // KP_Up
        0xff98 => 0x4d,          // KP_Right
        0xff99 => 0x50,          // KP_Down
        0xff9a => 0x49,          // KP_Page_Up
        0xff9b => 0x51,          // KP_Page_Down
        0xff9c => 0x4f,          // KP_End
        0xff9d => 0x4c,          // KP_Begin
        0xff9e => 0x52,          // KP_Insert
        0xff9f => 0x53,          // KP_Delete
        0xffaa => 0x37,          // KP_Multiply
        0xffab => 0x4e,          // KP_Add
        0xffad => 0x4a,          // KP_Subtract
        0xffac | 0xffae => 0x53, // KP_Separator, KP_Decimal
        0xffaf => 0xe035,        // KP_Divide
        0xffb0 => 0x52,          // KP_0
        0xffb1 => 0x4f,
        0xffb2 => 0x50,
        0xffb3 => 0x51,
        0xffb4 => 0x4b,
        0xffb5 => 0x4c,
        0xffb6 => 0x4d,
        0xffb7 => 0x47,
        0xffb8 => 0x48,
        0xffb9 => 0x49,          // KP_9
        0xffbe..=0xffc7 => 0x3b + (keysym - 0xffbe) as u16, // F1..F10
        0xffc8 => 0x57,          // F11
        0xffc9 => 0x58,          // F12
        0xffe1 => 0x2a,          // Shift_L
        0xffe2 => 0x36,          // Shift_R
        0xffe3 => 0x1d,          // Control_L
        0xffe4 => 0xe01d,        // Control_R
        0xffe5 => 0x3a,          // Caps_Lock
        0xffe7 | 0xffeb => 0xe05b, // Meta_L, Super_L
        0xffe8 | 0xffec => 0xe05c, // Meta_R, Super_R
        0xffe9 => 0x38,          // Alt_L
        0xffea | 0xfe03 => 0xe038, // Alt_R, ISO_Level3_Shift (AltGr)
        0xffff => 0xe053,        // Delete
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_map_to_the_same_key_in_either_case() {
        assert_eq!(scancode('a' as u32), Some(0x1e));
        assert_eq!(scancode('A' as u32), Some(0x1e));
    }

    #[test]
    fn shifted_symbols_map_to_their_base_key() {
        assert_eq!(scancode('!' as u32), scancode('1' as u32));
        assert_eq!(scancode('?' as u32), Some(0x35));
        assert_eq!(scancode('|' as u32), Some(0x2b));
        assert_eq!(scancode(' ' as u32), Some(0x39));
    }

    #[test]
    fn function_keys() {
        assert_eq!(scancode(0xffbe), Some(0x3b));
        assert_eq!(scancode(0xffc7), Some(0x44));
        assert_eq!(scancode(0xffc8), Some(0x57));
        assert_eq!(scancode(0xffc9), Some(0x58));
    }

    #[test]
    fn editing_and_modifier_keys() {
        assert_eq!(scancode(0xff0d), Some(0x1c));
        assert_eq!(scancode(0xffff), Some(0xe053));
        assert_eq!(scancode(0xff52), Some(0xe048));
        assert_eq!(scancode(0xffe3), Some(0x1d));
        assert_eq!(scancode(0xffeb), Some(0xe05b));
        assert_eq!(scancode(0xfe20), Some(0x0f));
    }

    #[test]
    fn unknown_keysyms_have_no_scancode() {
        assert_eq!(scancode(0), None);
        assert_eq!(scancode(0x1f), None);
        assert_eq!(scancode(0x10000a0), None);
    }
}

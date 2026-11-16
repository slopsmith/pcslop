//! Input status 1 retrace bits, derived from the CRTC timings and the clock.

use crate::Vga;

const DOT_CLOCKS_HZ: [u64; 4] = [25_175_000, 28_322_000, 25_175_000, 25_175_000];

const IS1_DISPLAY_DISABLED: u8 = 0x01;
const IS1_VRETRACE: u8 = 0x08;

impl Vga {
    pub(crate) fn input_status1(&mut self, now_ns: u64) -> u8 {
        let cr = &self.crtc;
        let ov = cr[7] as u32;
        let htotal = cr[0] as u64 + 5;
        let hde = cr[1] as u64 + 1;
        let vtotal = (cr[6] as u32 | (ov & 1) << 8 | (ov & 0x20) << 4) + 2;
        let vde = (cr[0x12] as u32 | (ov & 2) << 7 | (ov & 0x40) << 3) + 1;
        let vrs = cr[0x10] as u32 | (ov & 4) << 6 | (ov & 0x80) << 2;
        let vre_len = match (cr[0x11] as u32).wrapping_sub(vrs) & 0xf {
            0 => 16,
            n => n,
        };
        if vtotal < 32 || vrs >= vtotal {
            self.retrace_toggle ^= IS1_DISPLAY_DISABLED | IS1_VRETRACE;
            return self.retrace_toggle;
        }

        let dots = if self.seq[1] & 1 != 0 { 8 } else { 9 };
        let clock = DOT_CLOCKS_HZ[((self.misc >> 2) & 3) as usize] >> ((self.seq[1] >> 3) & 1);
        let char_ns = dots * 1_000_000_000 / clock;
        let line_ns = (char_ns * htotal).max(1);
        let frame_ns = line_ns * vtotal as u64;
        let t = now_ns % frame_ns;
        let line = (t / line_ns) as u32;
        let col = (t % line_ns) / char_ns.max(1);

        let mut v = 0;
        if line >= vde || col >= hde {
            v |= IS1_DISPLAY_DISABLED;
        }
        if line >= vrs && line < vrs + vre_len {
            v |= IS1_VRETRACE;
        }
        v
    }
}

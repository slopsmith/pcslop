//! Scan-out to a 0x00RRGGBB framebuffer.
//!
//! Output is at the CRTC's dot resolution: 720x400 for mode 3, 640x400 for
//! mode 13h, 640x480 for mode 12h, XRES x YRES for VBE modes.

use crate::Vga;

const FRAME_NS: u64 = 1_000_000_000 / 60;
const CURSOR_HALF_PERIOD_NS: u64 = 16 * FRAME_NS;
const BLINK_HALF_PERIOD_NS: u64 = 32 * FRAME_NS;
const MAX_DIM: usize = 16384;

/// CRTC scan-out parameters for one frame.
struct Scan {
    clocks: usize,
    height: usize,
    start: u32,
    /// Address counter increment per character row.
    step: u32,
    max_scan: u32,
    double_scan: bool,
    line_compare: u32,
    preset_row_scan: u32,
    /// Plane offset equals the address counter (VBE planar modes).
    linear: bool,
}

/// Address counter and row scan for one scanline, and whether it is below a line compare split.
struct Line {
    row: u32,
    row_scan: u32,
    split: bool,
}

struct Graphics {
    shift256: bool,
    interleave: bool,
    half_clock: bool,
    count_by: u32,
    pan: usize,
}

impl Scan {
    fn lines(&self) -> Vec<Line> {
        let mut v = Vec::with_capacity(self.height);
        let mut row = self.start;
        let mut rs = self.preset_row_scan;
        let mut split = false;
        for y in 0..self.height {
            v.push(Line { row, row_scan: rs, split });
            if y as u32 == self.line_compare {
                row = 0;
                rs = 0;
                split = true;
                continue;
            }
            if !self.double_scan || y & 1 == 1 {
                if rs >= self.max_scan {
                    rs = 0;
                    row = row.wrapping_add(self.step);
                } else {
                    rs += 1;
                }
            }
        }
        v
    }
}

fn valid(w: usize, h: usize) -> bool {
    (1..=MAX_DIM).contains(&w) && (1..=MAX_DIM).contains(&h)
}

fn expand5(v: u32) -> u32 {
    (v << 3) | (v >> 2)
}

fn expand6(v: u32) -> u32 {
    (v << 2) | (v >> 4)
}

impl Vga {
    /// Draws the current screen; returns (width, height) or None if the display is off/blanked.
    /// Each call advances text blinking by one 60 Hz frame.
    pub fn render(&mut self, out: &mut Vec<u32>) -> Option<(u32, u32)> {
        self.frames += 1;
        self.render_at(out, self.frames * FRAME_NS)
    }

    /// Like [`Vga::render`], with the text blink phases taken from `now_ns`.
    pub fn render_at(&mut self, out: &mut Vec<u32>, now_ns: u64) -> Option<(u32, u32)> {
        if self.attr_index & 0x20 == 0 || self.seq[1] & 0x20 != 0 {
            return None;
        }
        if self.vbe.enabled() {
            if self.vbe.bits() == 4 {
                let s = self.vbe_planar_scan();
                let g = Graphics { shift256: false, interleave: false, half_clock: false, count_by: 0, pan: 0 };
                return self.render_graphics(out, &s, &g);
            }
            return self.render_vbe(out);
        }
        let s = self.std_scan();
        if self.gfx[6] & 1 == 0 {
            return self.render_text(out, &s, now_ns);
        }
        let shift256 = self.gfx[5] & 0x40 != 0;
        let pan = self.attr[0x13] as usize;
        let g = Graphics {
            shift256,
            interleave: self.gfx[5] & 0x20 != 0,
            half_clock: self.seq[1] & 0x08 != 0,
            count_by: if self.crtc[0x14] & 0x20 != 0 {
                2
            } else if self.crtc[0x17] & 0x08 != 0 {
                1
            } else {
                0
            },
            pan: if shift256 { (pan >> 1) & 3 } else { pan & 7 },
        };
        self.render_graphics(out, &s, &g)
    }

    fn dac_rgb(&self, index: u8) -> u32 {
        let i = (index & self.pel_mask) as usize * 3;
        let c = |v: u8| if self.dac_8bit { v as u32 } else { expand6((v & 0x3f) as u32) };
        c(self.dac[i]) << 16 | c(self.dac[i + 1]) << 8 | c(self.dac[i + 2])
    }

    fn palette16(&self) -> [u32; 16] {
        let a = &self.attr;
        std::array::from_fn(|v| {
            let p = a[v & (a[0x12] & 0x0f) as usize];
            let index = if a[0x10] & 0x80 != 0 { (a[0x14] & 0x0f) << 4 | (p & 0x0f) } else { (a[0x14] & 0x0c) << 4 | (p & 0x3f) };
            self.dac_rgb(index)
        })
    }

    pub fn palette256(&self) -> [u32; 256] {
        std::array::from_fn(|i| self.dac_rgb(i as u8))
    }

    fn std_scan(&self) -> Scan {
        let cr = &self.crtc;
        let ov = cr[7] as u32;
        Scan {
            clocks: cr[1] as usize + 1,
            height: (cr[0x12] as u32 | (ov & 2) << 7 | (ov & 0x40) << 3) as usize + 1,
            start: (cr[0x0c] as u32) << 8 | cr[0x0d] as u32,
            step: cr[0x13] as u32 * 2,
            max_scan: (cr[9] & 0x1f) as u32,
            double_scan: cr[9] & 0x80 != 0,
            line_compare: cr[0x18] as u32 | (ov & 0x10) << 4 | (cr[9] as u32 & 0x40) << 3,
            preset_row_scan: (cr[8] & 0x1f) as u32,
            linear: false,
        }
    }

    fn vbe_planar_scan(&self) -> Scan {
        use crate::vbe::{INDEX_XRES, INDEX_YRES};
        Scan {
            clocks: self.vbe.reg(INDEX_XRES) as usize / 8,
            height: self.vbe.reg(INDEX_YRES) as usize,
            start: self.vbe.start / 4,
            step: self.vbe.line_offset / 4,
            max_scan: 0,
            double_scan: false,
            line_compare: u32::MAX,
            preset_row_scan: 0,
            linear: true,
        }
    }

    /// Maps a CRTC address counter value to a plane offset, applying
    /// byte/word/doubleword mode and the CGA/Hercules row scan substitution.
    fn crtc_address(&self, c: u32, row_scan: u32) -> usize {
        let cr17 = self.crtc[0x17];
        let c = c & 0xffff;
        let mut a = if self.crtc[0x14] & 0x40 != 0 {
            (c << 2) | ((c >> 14) & 3)
        } else if cr17 & 0x40 == 0 {
            let wrap = if cr17 & 0x20 != 0 { 15 } else { 13 };
            (c << 1) | ((c >> wrap) & 1)
        } else {
            c
        };
        if cr17 & 1 == 0 {
            a = (a & !(1 << 13)) | ((row_scan & 1) << 13);
        }
        if cr17 & 2 == 0 {
            a = (a & !(1 << 14)) | ((row_scan & 2) << 13);
        }
        (a & 0xffff) as usize
    }

    fn planes(&self, offset: usize) -> [u8; 4] {
        let i = (offset * 4) % self.vram.len();
        [self.vram[i], self.vram[i + 1], self.vram[i + 2], self.vram[i + 3]]
    }

    fn render_text(&mut self, out: &mut Vec<u32>, s: &Scan, now_ns: u64) -> Option<(u32, u32)> {
        let cw = if self.seq[1] & 1 != 0 { 8 } else { 9 };
        let dw = if self.seq[1] & 0x08 != 0 { 2 } else { 1 };
        let width = s.clocks * cw * dw;
        if !valid(width, s.height) {
            return None;
        }
        let pal = self.palette16();
        let cr = &self.crtc;
        let ar10 = self.attr[0x10];
        let blink_enabled = ar10 & 0x08 != 0;
        let blink_on = (now_ns / BLINK_HALF_PERIOD_NS).is_multiple_of(2);
        let line_graphics = ar10 & 0x04 != 0;
        let pan = match self.attr[0x13] {
            p if cw == 9 && p < 8 => p as usize + 1,
            p if cw == 8 && p < 8 => p as usize,
            _ => 0,
        };

        let sr3 = self.seq[3] as usize;
        let font_offset = |sel: usize| ((sel & 3) << 14) | ((sel >> 2) << 13);
        let font_b = font_offset((sr3 & 3) | ((sr3 >> 2) & 4));
        let font_a = font_offset(((sr3 >> 2) & 3) | ((sr3 >> 3) & 4));

        let skew = ((cr[0x0b] >> 5) & 3) as u32;
        let cursor = (((cr[0x0e] as u32) << 8 | cr[0x0f] as u32) + skew) & 0xffff;
        let cursor_first = (cr[0x0a] & 0x1f) as u32;
        let cursor_last = (cr[0x0b] & 0x1f) as u32;
        let cursor_on = cr[0x0a] & 0x20 == 0 && (now_ns / CURSOR_HALF_PERIOD_NS).is_multiple_of(2);
        let underline = (cr[0x14] & 0x1f) as u32;

        out.clear();
        out.resize(width * s.height, 0);
        let mut line = std::mem::take(&mut self.scratch);
        for (y, l) in s.lines().into_iter().enumerate() {
            line.clear();
            for i in 0..=s.clocks as u32 {
                let c = (l.row + i) & 0xffff;
                let [ch, at, _, _] = self.planes(self.crtc_address(c, l.row_scan));
                let fg = pal[(at & 0x0f) as usize];
                let bg = pal[if blink_enabled { (at >> 4) & 7 } else { at >> 4 } as usize];
                let blinked = blink_enabled && at & 0x80 != 0 && !blink_on;
                let font = if at & 0x08 != 0 { font_a } else { font_b };
                let mut glyph = self.planes((font + ch as usize * 32 + l.row_scan as usize) & 0xffff)[2] as u16;
                glyph = if cw == 9 && line_graphics && (0xc0..=0xdf).contains(&ch) {
                    glyph << 1 | (glyph & 1)
                } else {
                    glyph << 1
                };
                if at & 0x77 == 0x01 && l.row_scan == underline {
                    glyph = 0x1ff;
                }
                if blinked {
                    glyph = 0;
                }
                if cursor_on && c == cursor && (cursor_first..=cursor_last).contains(&l.row_scan) {
                    glyph = 0x1ff;
                }
                for bit in (9 - cw..9).rev() {
                    line.push(if glyph >> bit & 1 != 0 { fg } else { bg });
                }
            }
            let pan = if l.split && ar10 & 0x20 != 0 { 0 } else { pan };
            let row = &mut out[y * width..(y + 1) * width];
            for (x, px) in row.iter_mut().enumerate() {
                *px = line[x / dw + pan];
            }
        }
        self.scratch = line;
        Some((width as u32, s.height as u32))
    }

    fn render_graphics(&mut self, out: &mut Vec<u32>, s: &Scan, g: &Graphics) -> Option<(u32, u32)> {
        let dots_per_clock = if g.half_clock { 16 } else { 8 };
        let width = s.clocks * dots_per_clock;
        if !valid(width, s.height) {
            return None;
        }
        let pixels_per_clock = if g.shift256 { 4 } else { 8 };
        let dots_per_pixel = dots_per_clock / pixels_per_clock;
        let lut: [u32; 256] = if g.shift256 {
            self.palette256()
        } else {
            let p = self.palette16();
            std::array::from_fn(|i| p[i & 15])
        };
        let split_pan_reset = self.attr[0x10] & 0x20 != 0;

        out.clear();
        out.resize(width * s.height, 0);
        let mut pix = std::mem::take(&mut self.pixels);
        for (y, l) in s.lines().into_iter().enumerate() {
            pix.clear();
            for i in 0..=s.clocks as u32 {
                let c = l.row.wrapping_add(i >> g.count_by);
                let a = if s.linear { c as usize } else { self.crtc_address(c, l.row_scan) };
                let p = self.planes(a);
                if g.shift256 {
                    pix.extend_from_slice(&p);
                } else if g.interleave {
                    for (lo, hi) in [(p[0], p[2]), (p[1], p[3])] {
                        for sh in [6, 4, 2, 0] {
                            pix.push((lo >> sh) & 3 | ((hi >> sh) & 3) << 2);
                        }
                    }
                } else {
                    for sh in (0..8).rev() {
                        pix.push((p[0] >> sh) & 1 | ((p[1] >> sh) & 1) << 1 | ((p[2] >> sh) & 1) << 2 | ((p[3] >> sh) & 1) << 3);
                    }
                }
            }
            let pan = if l.split && split_pan_reset { 0 } else { g.pan };
            let row = &mut out[y * width..(y + 1) * width];
            for (x, px) in row.iter_mut().enumerate() {
                *px = lut[pix[x / dots_per_pixel + pan] as usize];
            }
        }
        self.pixels = pix;
        Some((width as u32, s.height as u32))
    }

    fn render_vbe(&mut self, out: &mut Vec<u32>) -> Option<(u32, u32)> {
        use crate::vbe::{INDEX_BPP, INDEX_XRES, INDEX_YRES};
        let w = self.vbe.reg(INDEX_XRES) as usize;
        let h = self.vbe.reg(INDEX_YRES) as usize;
        if !valid(w, h) {
            return None;
        }
        let bpp = self.vbe.reg(INDEX_BPP);
        let bytes = self.vbe.bits() as usize / 8;
        let pal = if bpp == 8 { self.palette256() } else { [0; 256] };
        let vram = &self.vram;
        let len = vram.len();

        out.clear();
        out.resize(w * h, 0);
        for y in 0..h {
            let base = self.vbe.start as usize + y * self.vbe.line_offset as usize;
            let row = &mut out[y * w..(y + 1) * w];
            let byte = |i: usize| vram[(base + i) % len] as u32;
            for (x, px) in row.iter_mut().enumerate() {
                let i = x * bytes;
                *px = match bpp {
                    8 => pal[byte(i) as usize],
                    15 => {
                        let v = byte(i) | byte(i + 1) << 8;
                        expand5((v >> 10) & 31) << 16 | expand5((v >> 5) & 31) << 8 | expand5(v & 31)
                    }
                    16 => {
                        let v = byte(i) | byte(i + 1) << 8;
                        expand5((v >> 11) & 31) << 16 | expand6((v >> 5) & 63) << 8 | expand5(v & 31)
                    }
                    _ => byte(i + 2) << 16 | byte(i + 1) << 8 | byte(i),
                };
            }
        }
        Some((w as u32, h as u32))
    }
}

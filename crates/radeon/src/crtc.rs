//! The primary CRTC in extended mode: timing, scanout and the vertical blank.

use crate::Radeon;
use crate::regs::*;

pub(crate) const FRAME_NS: u64 = 1_000_000_000 / 60;
const MAX_DIM: u32 = 4096;

/// Scanout parameters of the primary CRTC, decoded from its registers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub bits: u32,
    /// Bytes from one line to the next.
    pub pitch: u32,
    /// VRAM byte offset of the top-left pixel.
    pub offset: u32,
    pub double_scan: bool,
}

impl Radeon {
    pub(crate) fn reg(&self, r: u32) -> u32 {
        self.regs[r as usize / 4]
    }

    /// True while the CRTC scans out from its own registers instead of the VGA's.
    pub fn extended(&self) -> bool {
        self.reg(CRTC_GEN_CNTL) & CRTC_EXT_DISP_EN != 0
    }

    pub fn mode(&self) -> Option<Mode> {
        let ctl = self.reg(CRTC_GEN_CNTL);
        let bits: u32 = match (ctl >> 8) & 0xf {
            2 => 8,
            3 => 15,
            4 => 16,
            5 => 24,
            6 => 32,
            _ => return None,
        };
        let double_scan = ctl & CRTC_DBL_SCAN_EN != 0;
        let width = (((self.reg(CRTC_H_TOTAL_DISP) >> 16) & 0x1ff) + 1) * 8;
        let mut height = ((self.reg(CRTC_V_TOTAL_DISP) >> 16) & 0x7ff) + 1;
        if double_scan {
            height /= 2;
        }
        let bytes = bits.div_ceil(8);
        let pitch = (self.reg(CRTC_PITCH) & 0x7ff) * 8 * bytes;
        let mut offset = self.vram_offset(self.shown_offset());
        if self.reg(CRTC_OFFSET_CNTL) & R300_CRTC_X_Y_MODE_EN != 0 {
            let xy = self.reg(R300_CRTC_TILE_X0_Y0);
            let (x, y) = (xy & 0xfff, (xy >> 16) & 0xfff);
            offset = offset.wrapping_add(y * pitch + x * bytes) % crate::VRAM_SIZE as u32;
        }
        (width <= MAX_DIM && height <= MAX_DIM && height > 0).then_some(Mode {
            width,
            height,
            bits,
            pitch,
            offset,
            double_scan,
        })
    }

    /// VRAM offset of a CRTC or cursor offset, which count from
    /// DISPLAY_BASE_ADDR in the memory controller's address space.
    fn vram_offset(&self, off: u32) -> u32 {
        let fb_start = (self.reg(MC_FB_LOCATION) & 0xffff) << 16;
        let mc = self.reg(DISPLAY_BASE_ADDR).wrapping_add(off & 0x07ff_ffff);
        mc.wrapping_sub(fb_start) % crate::VRAM_SIZE as u32
    }

    /// Scanline the CRTC is on now, from its vertical total and a 60 Hz frame.
    pub(crate) fn vline(&self) -> u32 {
        let total = (self.reg(CRTC_V_TOTAL_DISP) & 0x7ff) + 1;
        ((self.now_ns % FRAME_NS) * total as u64 / FRAME_NS) as u32
    }

    pub(crate) fn in_vblank(&self) -> bool {
        let disp = (self.reg(CRTC_V_TOTAL_DISP) >> 16) & 0x7ff;
        disp == 0 || self.vline() > disp
    }

    pub(crate) fn frame(&self) -> u64 {
        self.now_ns / FRAME_NS
    }

    pub(crate) fn next_frame_ns(&self) -> u64 {
        (self.frame() + 1) * FRAME_NS
    }

    /// The picture in extended modes, or None without waiting if `wait` is
    /// false and queued Metal work still draws into it.
    pub(crate) fn render_extended(&mut self, out: &mut Vec<u32>, wait: bool) -> Option<Option<(u32, u32)>> {
        let ctl = self.reg(CRTC_GEN_CNTL);
        if ctl & CRTC_EN == 0 || self.reg(CRTC_EXT_CNTL) & CRTC_DISPLAY_DIS != 0 {
            return Some(None);
        }
        let Some(m) = self.mode() else { return Some(None) };
        let shown = m.offset as usize..m.offset as usize + m.pitch as usize * m.height as usize;
        if self.metal.as_mut().is_some_and(|g| g.writes_pending(shown.start, shown.end)) {
            if !wait {
                return None;
            }
            self.gpu_sync();
        }
        let pal = if m.bits == 8 {
            self.vga.palette256()
        } else {
            [0; 256]
        };
        let vram = self.vga.vram();
        let (w, h) = (m.width as usize, m.height as usize);
        out.clear();
        out.resize(w * h, 0);
        let bytes_pp = match m.bits {
            8 => 1,
            15 | 16 => 2,
            24 => 3,
            _ => 4,
        };
        let row_bytes = w * bytes_pp;
        let mut wrapped = Vec::new();
        for (y, row) in out.chunks_exact_mut(w).enumerate() {
            let base = m.offset as usize + y * m.pitch as usize;
            let src = match vram.get(base..base + row_bytes) {
                Some(s) => s,
                None => {
                    wrapped.clear();
                    wrapped.extend((base..base + row_bytes).map(|i| vram[i % vram.len()]));
                    &wrapped
                }
            };
            convert_row(m.bits, src, row, &pal);
        }
        self.draw_cursor(out, &m);
        Some(Some((m.width, m.height)))
    }
}

fn convert_row(bits: u32, src: &[u8], out: &mut [u32], pal: &[u32; 256]) {
    match bits {
        8 => out.iter_mut().zip(src).for_each(|(o, &i)| *o = pal[i as usize]),
        15 => out.iter_mut().zip(src.chunks_exact(2)).for_each(|(o, b)| {
            let v = u16::from_le_bytes([b[0], b[1]]) as u32;
            *o = expand5(v >> 10) << 16 | expand5(v >> 5) << 8 | expand5(v);
        }),
        16 => out.iter_mut().zip(src.chunks_exact(2)).for_each(|(o, b)| {
            let v = u16::from_le_bytes([b[0], b[1]]) as u32;
            *o = expand5(v >> 11) << 16 | expand6(v >> 5) << 8 | expand5(v);
        }),
        24 => out
            .iter_mut()
            .zip(src.chunks_exact(3))
            .for_each(|(o, b)| *o = (b[2] as u32) << 16 | (b[1] as u32) << 8 | b[0] as u32),
        _ => out
            .iter_mut()
            .zip(src.chunks_exact(4))
            .for_each(|(o, b)| *o = u32::from_le_bytes([b[0], b[1], b[2], 0])),
    }
}

fn expand5(v: u32) -> u32 {
    let v = v & 31;
    (v << 3) | (v >> 2)
}

fn expand6(v: u32) -> u32 {
    let v = v & 63;
    (v << 2) | (v >> 4)
}

impl Radeon {
    /// Blends the 64x64 hardware cursor over `out`. Only the ARGB mode that
    /// xf86-video-ati uses (CUR_MODE 2, premultiplied alpha) is drawn.
    fn draw_cursor(&self, out: &mut [u32], m: &Mode) {
        let ctl = self.reg(CRTC_GEN_CNTL);
        if ctl & CRTC_CUR_EN == 0 || (ctl >> 20) & 7 != 2 {
            return;
        }
        let posn = self.reg(CUR_HORZ_VERT_POSN);
        let hot = self.reg(CUR_HORZ_VERT_OFF);
        let (x0, y0) = ((posn >> 16) & 0x3fff, posn & 0xfff);
        let (skip_x, skip_y) = ((hot >> 16) & 0x3f, hot & 0x3f);
        let base = self.vram_offset(self.reg(CUR_OFFSET)) as usize;
        let vram = self.vga.vram();
        for j in 0..64 - skip_y {
            let y = y0 + j;
            if y >= m.height {
                break;
            }
            for i in 0..64 - skip_x {
                let x = x0 + i;
                if x >= m.width {
                    break;
                }
                let a = (base + j as usize * 256 + (skip_x + i) as usize * 4) % vram.len();
                let src = u32::from_le_bytes(vram[a..a + 4].try_into().unwrap());
                let alpha = src >> 24;
                let dst = &mut out[(y * m.width + x) as usize];
                *dst = blend(src, *dst, alpha);
            }
        }
    }
}

fn blend(src: u32, dst: u32, alpha: u32) -> u32 {
    let mut v = 0;
    for shift in [0, 8, 16] {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        v |= (s + d * (255 - alpha) / 255).min(255) << shift;
    }
    v
}

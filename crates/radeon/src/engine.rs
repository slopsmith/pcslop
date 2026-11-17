//! The 2D engine, run on the CPU: fills, blits with any ROP3, host data
//! uploads with colour expansion, and lines. Register writes and the CP's
//! 2D packets both end here.
//!
//! How DP_GUI_MASTER_CNTL splits into DP_DATATYPE and DP_MIX and loads the
//! default pitch, offset and scissor follows QEMU's ati-vga
//! (hw/display/ati.c, BALATON Zoltan, GPL-2.0-or-later); the packet layouts
//! follow AMD's R5xx Acceleration guide (v1.5, section 6.2.2).
//!
//! Tiled surfaces are kept linear: the CRTC scans them out linearly too, and
//! CPU accesses through the SURFACE_* ranges are linear on real hardware.

use crate::cp::op;
use crate::mem::BusMemory;
use crate::regs::*;
use crate::{Radeon, VRAM_SIZE};

const COORD_MAX: i32 = 8192;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Surface {
    /// In the memory controller's address space.
    pub offset: u32,
    pub pitch: u32,
}

impl Surface {
    fn from_pitch_offset(v: u32) -> Self {
        Surface {
            offset: (v & 0x3f_ffff) << 10,
            pitch: ((v >> 22) & 0xff) * 64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A blit waiting for its source pixels through HOST_DATA.
#[derive(Debug)]
pub(crate) struct HostBlit {
    rect: Rect,
    data: Vec<u32>,
    needed: usize,
}

#[derive(Default)]
pub(crate) struct Engine {
    pub dst: Surface,
    pub src: Surface,
    pub default: Surface,
    /// Left, top, right and bottom; right and bottom are exclusive.
    pub sc: [i32; 4],
    pub dst_x: i32,
    pub dst_y: i32,
    pub src_x: i32,
    pub src_y: i32,
    pub width: i32,
    pub height: i32,
    pub line: (i32, i32),
    pub host: Option<HostBlit>,
    pub ops: u64,
}

struct Cursor<'a> {
    body: &'a [u32],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn next(&mut self) -> Option<u32> {
        let v = self.body.get(self.i).copied();
        self.i += 1;
        v
    }

    fn take(&mut self, n: usize) -> &'a [u32] {
        let start = self.i.min(self.body.len());
        let end = (start + n).min(self.body.len());
        self.i = end;
        &self.body[start..end]
    }
}

/// Source pixels for one operation.
enum Src<'a> {
    None,
    /// A rectangle of `bpp`-byte pixels, `stride` pixels per row.
    Pixels {
        data: &'a [u8],
        stride: usize,
    },
    /// One bit per pixel; `row_bits` apart, starting at bit 0 of the rectangle.
    Mono {
        data: &'a [u32],
        row_bits: usize,
        lsb_first: bool,
    },
}

fn sext16(v: u32) -> i32 {
    v as u16 as i16 as i32
}

/// The Windows ROP3 of pattern, source and destination, bit by bit.
pub(crate) fn rop3(rop: u8, p: u32, s: u32, d: u32) -> u32 {
    let mut r = 0;
    for i in 0..8 {
        if rop >> i & 1 != 0 {
            let pm = if i & 4 != 0 { p } else { !p };
            let sm = if i & 2 != 0 { s } else { !s };
            let dm = if i & 1 != 0 { d } else { !d };
            r |= pm & sm & dm;
        }
    }
    r
}

fn uses_src(rop: u8) -> bool {
    (rop >> 2 ^ rop) & 0x33 != 0
}

fn uses_pat(rop: u8) -> bool {
    (rop >> 4 ^ rop) & 0x0f != 0
}

/// Bytes per pixel of a GMC/DP_DATATYPE destination type.
pub(crate) fn bytes_per_pixel(datatype: u32) -> Option<usize> {
    match datatype & 0xf {
        2 | 7 | 8 | 9 => Some(1),
        3 | 4 | 11 | 12 | 15 => Some(2),
        6 | 14 => Some(4),
        _ => None,
    }
}

fn pixel_mask(bpp: usize) -> u32 {
    if bpp >= 4 {
        u32::MAX
    } else {
        (1 << (8 * bpp)) - 1
    }
}

fn mono_bit(data: &[u32], bit: usize, lsb_first: bool) -> bool {
    let word = data.get(bit / 32).copied().unwrap_or(0);
    let i = bit % 32;
    let shift = if lsb_first {
        i
    } else {
        (i & !7) | (7 - (i & 7))
    };
    word >> shift & 1 != 0
}

impl Radeon {
    fn datatype(&self) -> u32 {
        self.reg(DP_DATATYPE)
    }

    fn rop(&self) -> u8 {
        (self.reg(DP_MIX) >> 16) as u8
    }

    /// DP_MIX's source select: 2 is VRAM, 3 and 4 are HOST_DATA.
    fn src_source(&self) -> u32 {
        (self.reg(DP_MIX) >> 8) & 7
    }

    fn default_scissor(&self) -> [i32; 4] {
        let br = self.reg(DEFAULT_SC_BOTTOM_RIGHT);
        [0, 0, (br & 0x3fff) as i32, ((br >> 16) & 0x3fff) as i32]
    }

    fn gui_master_written(&mut self, g: u32) {
        let dt = self.reg(DP_DATATYPE) & !0x4003_0f0f;
        self.regs[DP_DATATYPE as usize / 4] =
            dt | (g >> 8 & 0xf) | (g >> 4 & 0xf) << 8 | (g >> 12 & 3) << 16 | (g >> 14 & 1) << 30;
        let mix = self.reg(DP_MIX) & !0x00ff_0700;
        self.regs[DP_MIX as usize / 4] = mix | (g & 0x00ff_0000) | (g >> 24 & 7) << 8;
        if g & GMC_SRC_PITCH_OFFSET_CNTL == 0 {
            self.engine.src = self.engine.default;
        }
        if g & GMC_DST_PITCH_OFFSET_CNTL == 0 {
            self.engine.dst = self.engine.default;
        }
        if g & GMC_DST_CLIPPING == 0 {
            self.engine.sc = self.default_scissor();
        }
        if g & GMC_WR_MSK_DIS != 0 {
            self.regs[DP_WRITE_MASK as usize / 4] = u32::MAX;
        }
        if g & GMC_CLR_CMP_CNTL_DIS != 0 {
            self.regs[CLR_CMP_CNTL as usize / 4] = 0;
        }
    }

    /// Side effects of writing a 2D engine register; false if `reg` has none.
    pub(crate) fn engine_register_written(&mut self, reg: u32, v: u32) -> bool {
        let e = &mut self.engine;
        match reg {
            DP_GUI_MASTER_CNTL | DP_GUI_MASTER_CNTL_C => self.gui_master_written(v),
            DST_PITCH_OFFSET | DST_PITCH_OFFSET_C => e.dst = Surface::from_pitch_offset(v),
            SRC_PITCH_OFFSET => e.src = Surface::from_pitch_offset(v),
            DEFAULT_OFFSET => e.default = Surface::from_pitch_offset(v),
            DST_OFFSET => e.dst.offset = v,
            DST_PITCH => e.dst.pitch = v & 0x3fff,
            SRC_OFFSET => e.src.offset = v,
            SRC_PITCH => e.src.pitch = v & 0x3fff,
            SC_TOP_LEFT | SC_TOP_LEFT_C => {
                e.sc[0] = sext16(v);
                e.sc[1] = sext16(v >> 16);
            }
            SC_BOTTOM_RIGHT | SC_BOTTOM_RIGHT_C => {
                e.sc[2] = sext16(v);
                e.sc[3] = sext16(v >> 16);
            }
            SC_LEFT => e.sc[0] = sext16(v),
            SC_TOP => e.sc[1] = sext16(v),
            SC_RIGHT => e.sc[2] = sext16(v),
            SC_BOTTOM => e.sc[3] = sext16(v),
            SRC_Y_X => (e.src_y, e.src_x) = (sext16(v >> 16), sext16(v)),
            SRC_X_Y => (e.src_x, e.src_y) = (sext16(v >> 16), sext16(v)),
            SRC_X => e.src_x = sext16(v),
            SRC_Y => e.src_y = sext16(v),
            DST_Y_X => (e.dst_y, e.dst_x) = (sext16(v >> 16), sext16(v)),
            DST_X_Y => (e.dst_x, e.dst_y) = (sext16(v >> 16), sext16(v)),
            DST_X => e.dst_x = sext16(v),
            DST_Y => e.dst_y = sext16(v),
            DST_WIDTH => e.width = (v & 0x3fff) as i32,
            DST_HEIGHT => e.height = (v & 0x3fff) as i32,
            DST_HEIGHT_WIDTH | DST_HEIGHT_WIDTH_8 => {
                (e.height, e.width) = ((v >> 16 & 0x3fff) as i32, (v & 0x3fff) as i32);
                self.blit_from_registers();
            }
            DST_WIDTH_HEIGHT => {
                (e.width, e.height) = ((v >> 16 & 0x3fff) as i32, (v & 0x3fff) as i32);
                self.blit_from_registers();
            }
            DST_LINE_START => e.line = (sext16(v), sext16(v >> 16)),
            DST_LINE_END => {
                let from = e.line;
                let to = (sext16(v), sext16(v >> 16));
                self.line(from, to);
                self.engine.line = to;
            }
            HOST_DATA0..=HOST_DATA_LAST => self.host_data(&[v], reg == HOST_DATA_LAST),
            _ => return false,
        }
        true
    }

    /// Starts the blit that DST_WIDTH_HEIGHT and friends trigger, honouring
    /// DP_CNTL: a right-to-left or bottom-to-top blit names its far edge.
    fn blit_from_registers(&mut self) {
        let e = &self.engine;
        let ctl = self.reg(DP_CNTL);
        let (w, h) = (e.width, e.height);
        let (mut x, mut y, mut sx, mut sy) = (e.dst_x, e.dst_y, e.src_x, e.src_y);
        if ctl & DST_X_LEFT_TO_RIGHT == 0 {
            x += 1 - w;
            sx += 1 - w;
        }
        if ctl & DST_Y_TOP_TO_BOTTOM == 0 {
            y += 1 - h;
            sy += 1 - h;
        }
        self.blit(Rect { x, y, w, h }, (sx, sy));
    }

    fn blit(&mut self, rect: Rect, src: (i32, i32)) {
        if rect.w <= 0 || rect.h <= 0 {
            return;
        }
        if matches!(self.src_source(), 3 | 4) && uses_src(self.rop()) {
            self.finish_host_blit();
            let needed = self.host_dwords(rect);
            self.engine.host = Some(HostBlit {
                rect,
                data: Vec::with_capacity(needed),
                needed,
            });
            return;
        }
        if uses_src(self.rop()) && self.src_source() == 2 {
            self.copy_from_vram(rect, src);
        } else {
            self.draw(rect, Src::None);
        }
    }

    /// Dwords of HOST_DATA a blit of `rect` takes.
    fn host_dwords(&self, rect: Rect) -> usize {
        let (w, h) = (rect.w as usize, rect.h as usize);
        if (self.datatype() >> 16) & 3 != 3 {
            if self.src_source() == 4 {
                (w.div_ceil(8) * h).div_ceil(4)
            } else {
                (w * h).div_ceil(32)
            }
        } else {
            let bpp = bytes_per_pixel(self.datatype()).unwrap_or(4);
            (w * h * bpp).div_ceil(4)
        }
    }

    fn host_data(&mut self, data: &[u32], last: bool) {
        let Some(hb) = self.engine.host.as_mut() else {
            return;
        };
        hb.data.extend_from_slice(data);
        if last || hb.data.len() >= hb.needed {
            self.finish_host_blit();
        }
    }

    fn finish_host_blit(&mut self) {
        let Some(hb) = self.engine.host.take() else {
            return;
        };
        let w = hb.rect.w as usize;
        if (self.datatype() >> 16) & 3 != 3 {
            let row_bits = if self.src_source() == 4 {
                w.div_ceil(8) * 8
            } else {
                w
            };
            let lsb_first = self.datatype() & 1 << 30 != 0;
            self.draw(
                hb.rect,
                Src::Mono {
                    data: &hb.data,
                    row_bits,
                    lsb_first,
                },
            );
        } else {
            let bytes: Vec<u8> = hb.data.iter().flat_map(|d| d.to_le_bytes()).collect();
            self.draw(
                hb.rect,
                Src::Pixels {
                    data: &bytes,
                    stride: w,
                },
            );
        }
    }

    fn copy_from_vram(&mut self, rect: Rect, (sx, sy): (i32, i32)) {
        let Some(bpp) = bytes_per_pixel(self.datatype()) else {
            return;
        };
        let src = self.engine.src;
        let base = self.vram_of(src.offset);
        let pitch = src.pitch as usize;
        let (w, h) = (rect.w as usize, rect.h as usize);
        if self.simple_copy(rect, (sx, sy), bpp) {
            return;
        }
        let vram = self.vga.vram();
        let mut pixels = vec![0u8; w * h * bpp];
        for j in 0..h {
            let row = sy + j as i32;
            if row < 0 {
                continue;
            }
            for i in 0..w {
                let col = sx + i as i32;
                if col < 0 {
                    continue;
                }
                let a = base + row as usize * pitch + col as usize * bpp;
                if a + bpp <= vram.len() {
                    pixels[(j * w + i) * bpp..][..bpp].copy_from_slice(&vram[a..a + bpp]);
                }
            }
        }
        self.draw(
            rect,
            Src::Pixels {
                data: &pixels,
                stride: w,
            },
        );
    }

    /// The common SRCCOPY with every plane written, done a row at a time.
    fn simple_copy(&mut self, rect: Rect, (sx, sy): (i32, i32), bpp: usize) -> bool {
        if self.rop() != 0xcc
            || self.write_mask(bpp) != pixel_mask(bpp)
            || self.colour_compare().is_some()
        {
            return false;
        }
        let Some(clip) = self.clip(rect) else {
            return true;
        };
        let (dx0, dy0) = (clip.x, clip.y);
        let (sx0, sy0) = (sx + (clip.x - rect.x), sy + (clip.y - rect.y));
        if sx0 < 0 || sy0 < 0 {
            return false;
        }
        let (src, dst) = (self.engine.src, self.engine.dst);
        let (sbase, dbase) = (self.vram_of(src.offset), self.vram_of(dst.offset));
        let (sp, dp) = (src.pitch as usize, dst.pitch as usize);
        let row_bytes = clip.w as usize * bpp;
        let last = |base: usize, pitch: usize, x: i32, y: i32| {
            base + (y as usize + clip.h as usize - 1) * pitch + x as usize * bpp + row_bytes
        };
        if last(sbase, sp, sx0, sy0) > VRAM_SIZE || last(dbase, dp, dx0, dy0) > VRAM_SIZE {
            return false;
        }
        let vram = self.vga.vram_mut();
        let rows: Box<dyn Iterator<Item = usize>> =
            if dbase + dy0 as usize * dp > sbase + sy0 as usize * sp {
                Box::new((0..clip.h as usize).rev())
            } else {
                Box::new(0..clip.h as usize)
            };
        for j in rows {
            let s = sbase + (sy0 as usize + j) * sp + sx0 as usize * bpp;
            let d = dbase + (dy0 as usize + j) * dp + dx0 as usize * bpp;
            vram.copy_within(s..s + row_bytes, d);
        }
        self.engine.ops += 1;
        true
    }

    fn write_mask(&self, bpp: usize) -> u32 {
        self.reg(DP_WRITE_MASK) & pixel_mask(bpp)
    }

    /// CLR_CMP_CNTL's source compare: the reference colour, and whether a
    /// matching pixel is the one left alone.
    fn colour_compare(&self) -> Option<(u32, u32, bool)> {
        let mask = self.reg(CLR_CMP_MASK);
        let reference = self.reg(CLR_CMP_CLR_SRC) & mask;
        match self.reg(CLR_CMP_CNTL) & 7 {
            4 => Some((reference, mask, true)),
            5 => Some((reference, mask, false)),
            _ => None,
        }
    }

    /// `rect` cut to the scissor and the engine's coordinate range.
    fn clip(&self, rect: Rect) -> Option<Rect> {
        let sc = self.engine.sc;
        let x0 = rect.x.max(sc[0]).max(0);
        let y0 = rect.y.max(sc[1]).max(0);
        let x1 = (rect.x + rect.w).min(sc[2]).min(COORD_MAX);
        let y1 = (rect.y + rect.h).min(sc[3]).min(COORD_MAX);
        (x1 > x0 && y1 > y0).then_some(Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }

    /// The brush at a screen pixel, or None where a transparent brush leaves
    /// the destination alone.
    fn pattern(&self, x: i32, y: i32, bpp: usize) -> Option<u32> {
        let fg = self.reg(DP_BRUSH_FRGD_CLR);
        let bg = self.reg(DP_BRUSH_BKGD_CLR);
        let org = self.reg(BRUSH_Y_X);
        let (px, py) = ((x + (org & 7) as i32) & 7, (y + (org >> 8 & 7) as i32) & 7);
        match (self.datatype() >> 8) & 0xf {
            t @ (0 | 1) => {
                let word = self.reg(BRUSH_DATA0 + 4 * (py as u32 / 4));
                let row = word >> (8 * (py % 4));
                if row >> px & 1 != 0 {
                    Some(fg)
                } else {
                    (t == 0).then_some(bg)
                }
            }
            10 => {
                let stride = if bpp == 3 { 4 } else { bpp };
                let off = (py as usize * 8 + px as usize) * stride;
                let word = self.reg(BRUSH_DATA0 + (off / 4) as u32);
                Some(word >> (8 * (off % 4)))
            }
            15 => Some(0),
            _ => Some(fg),
        }
    }

    /// Writes `rect` through the scissor with the current ROP, brush and
    /// write mask, taking source pixels from `src`.
    fn draw(&mut self, rect: Rect, src: Src) {
        let Some(bpp) = bytes_per_pixel(self.datatype()) else {
            return;
        };
        let Some(clip) = self.clip(rect) else { return };
        self.engine.ops += 1;
        let rop = self.rop();
        let pmask = pixel_mask(bpp);
        let mask = self.write_mask(bpp);
        let compare = self.colour_compare();
        let dst = self.engine.dst;
        let base = self.vram_of(dst.offset);
        let pitch = dst.pitch as usize;
        let need_pat = uses_pat(rop);
        let need_src = uses_src(rop);
        let mono = (self.datatype() >> 16) & 3;
        let (sfg, sbg) = (self.reg(DP_SRC_FRGD_CLR), self.reg(DP_SRC_BKGD_CLR));
        let solid = matches!((self.datatype() >> 8) & 0xf, 13 | 14);
        if solid
            && !need_src
            && rop == 0xf0
            && mask == pmask
            && self.fill_fast(clip, base, pitch, bpp)
        {
            return;
        }
        let pats: Vec<Option<u32>> = match (need_pat, solid) {
            (false, _) => vec![Some(0)],
            (true, true) => vec![Some(self.reg(DP_BRUSH_FRGD_CLR))],
            (true, false) => (0..64)
                .map(|i| self.pattern(clip.x + (i % 8), clip.y + (i / 8), bpp))
                .collect(),
        };
        let vram = self.vga.vram_mut();
        for j in 0..clip.h {
            let y = clip.y + j;
            let row = (y - rect.y) as usize;
            for i in 0..clip.w {
                let x = clip.x + i;
                let col = (x - rect.x) as usize;
                let k = if pats.len() == 64 {
                    ((j & 7) * 8 + (i & 7)) as usize
                } else {
                    0
                };
                let Some(p) = pats[k] else { continue };
                let s = if need_src {
                    match &src {
                        Src::None => 0,
                        Src::Pixels { data, stride } => {
                            let k = (row * stride + col) * bpp;
                            match data.get(k..k + bpp) {
                                Some(b) => b.iter().rev().fold(0, |v, &b| v << 8 | b as u32),
                                None => 0,
                            }
                        }
                        Src::Mono {
                            data,
                            row_bits,
                            lsb_first,
                        } => {
                            if mono_bit(data, row * row_bits + col, *lsb_first) {
                                sfg
                            } else if mono == 1 {
                                continue;
                            } else {
                                sbg
                            }
                        }
                    }
                } else {
                    0
                };
                if need_src
                    && let Some((reference, cmask, skip_equal)) = compare
                    && ((s & cmask) == reference) == skip_equal
                {
                    continue;
                }
                let a = base + y as usize * pitch + x as usize * bpp;
                let Some(px) = vram.get_mut(a..a + bpp) else {
                    continue;
                };
                let d = px.iter().rev().fold(0, |v, &b| v << 8 | b as u32);
                let r = (rop3(rop, p, s, d) & mask) | (d & !mask);
                px.copy_from_slice(&r.to_le_bytes()[..bpp]);
            }
        }
    }

    fn fill_fast(&mut self, clip: Rect, base: usize, pitch: usize, bpp: usize) -> bool {
        let end = base + (clip.y + clip.h - 1) as usize * pitch + (clip.x + clip.w) as usize * bpp;
        if end > VRAM_SIZE {
            return false;
        }
        let colour = self.reg(DP_BRUSH_FRGD_CLR).to_le_bytes();
        let vram = self.vga.vram_mut();
        for y in clip.y..clip.y + clip.h {
            let a = base + y as usize * pitch + clip.x as usize * bpp;
            for px in vram[a..a + clip.w as usize * bpp].chunks_exact_mut(bpp) {
                px.copy_from_slice(&colour[..bpp]);
            }
        }
        true
    }

    /// A line from `from` to `to` without its last pixel, as the engine draws
    /// DST_LINE_START to DST_LINE_END.
    fn line(&mut self, from: (i32, i32), to: (i32, i32)) {
        let (mut x, mut y) = from;
        let (dx, dy) = ((to.0 - x).abs(), -(to.1 - y).abs());
        let (stepx, stepy) = (if x < to.0 { 1 } else { -1 }, if y < to.1 { 1 } else { -1 });
        let mut err = dx + dy;
        let brush = (self.datatype() >> 8) & 0xf;
        let mut count = self.reg(DST_LINE_PATCOUNT) & 0x1f;
        let pat = self.reg(BRUSH_DATA0);
        let (fg, bg) = (self.reg(DP_BRUSH_FRGD_CLR), self.reg(DP_BRUSH_BKGD_CLR));
        let saved = self.regs[DP_BRUSH_FRGD_CLR as usize / 4];
        let saved_type = self.regs[DP_DATATYPE as usize / 4];
        while (x, y) != to {
            let colour = match brush {
                6 | 7 => {
                    let on = pat >> count & 1 != 0;
                    count = (count + 1) & 31;
                    if on {
                        Some(fg)
                    } else {
                        (brush == 6).then_some(bg)
                    }
                }
                _ => Some(fg),
            };
            if let Some(c) = colour {
                self.regs[DP_BRUSH_FRGD_CLR as usize / 4] = c;
                self.regs[DP_DATATYPE as usize / 4] = (saved_type & !0xf00) | 13 << 8;
                self.draw(Rect { x, y, w: 1, h: 1 }, Src::None);
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += stepx;
            }
            if e2 <= dx {
                err += dx;
                y += stepy;
            }
        }
        self.regs[DP_BRUSH_FRGD_CLR as usize / 4] = saved;
        self.regs[DP_DATATYPE as usize / 4] = saved_type;
    }

    /// A type-3 2D packet: its SETTINGS become register writes, then its
    /// DATA_BLOCK draws.
    pub(crate) fn packet_2d(&mut self, mem: &mut dyn BusMemory, opcode: u8, body: &[u32]) {
        let Some(start) = self.packet_settings(mem, body) else {
            return;
        };
        let mut c = Cursor { body, i: start };
        let mut next = || c.next();
        let xy = |v: u32| (sext16(v >> 16), sext16(v));
        match opcode {
            op::PAINT_MULTI => {
                while let (Some(pos), Some(size)) = (next(), next()) {
                    let ((x, y), (w, h)) = (xy(pos), xy(size));
                    self.blit(Rect { x, y, w, h }, (0, 0));
                }
            }
            op::PAINT => {
                while let (Some(tl), Some(br)) = (next(), next()) {
                    let ((y, x), (b, r)) = (xy(tl), xy(br));
                    self.blit(
                        Rect {
                            x,
                            y,
                            w: r - x,
                            h: b - y,
                        },
                        (0, 0),
                    );
                }
            }
            op::BITBLT | op::BITBLT_MULTI => {
                while let (Some(s), Some(d), Some(size)) = (next(), next(), next()) {
                    let (s, (x, y)) = (xy(s), xy(d));
                    let (w, h) = ((size >> 16 & 0x3fff) as i32, (size & 0x3fff) as i32);
                    self.blit(Rect { x, y, w, h }, s);
                }
            }
            op::HOSTDATA_BLT => {
                drop(next);
                let (Some(fg), Some(bg)) = (c.next(), c.next()) else {
                    return;
                };
                self.regs[DP_SRC_FRGD_CLR as usize / 4] = fg;
                self.regs[DP_SRC_BKGD_CLR as usize / 4] = bg;
                while let (Some(pos), Some(size), Some(n)) = (c.next(), c.next(), c.next()) {
                    let ((y, x), (h, w)) = (xy(pos), xy(size));
                    let data = c.take((n & 0x3fff) as usize);
                    self.blit(Rect { x, y, w, h }, (0, 0));
                    self.host_data(data, true);
                }
            }
            op::POLYLINE => {
                let mut prev = None;
                while let Some(p) = next() {
                    let (y, x) = xy(p);
                    if let Some(from) = prev {
                        self.line(from, (x, y));
                    }
                    prev = Some((x, y));
                }
            }
            op::POLYSCANLINES => {
                let Some(count) = next() else { return };
                for _ in 0..count {
                    let (Some(n), Some(top)) = (next(), next()) else {
                        return;
                    };
                    let (h, y) = xy(top);
                    for _ in 0..(n & 0x3fff) {
                        let Some(seg) = next() else { return };
                        let (end, start) = xy(seg);
                        self.blit(
                            Rect {
                                x: start,
                                y,
                                w: end - start,
                                h,
                            },
                            (0, 0),
                        );
                    }
                }
            }
            _ => self.note(&format!("cp: 2D opcode {opcode:#04x} not drawn")),
        }
    }

    /// Applies a 2D packet's SETTINGS and returns where its DATA_BLOCK starts.
    fn packet_settings(&mut self, mem: &mut dyn BusMemory, body: &[u32]) -> Option<usize> {
        let g = *body.first()?;
        let mut i = 1;
        let mut take = |n: usize| {
            let v = body.get(i..i + n);
            i += n;
            v
        };
        let mut writes = vec![(DP_GUI_MASTER_CNTL, g)];
        if g & GMC_SRC_PITCH_OFFSET_CNTL != 0 {
            writes.push((SRC_PITCH_OFFSET, take(1)?[0]));
        }
        if g & GMC_DST_PITCH_OFFSET_CNTL != 0 {
            writes.push((DST_PITCH_OFFSET, take(1)?[0]));
        }
        if g & GMC_SRC_CLIPPING != 0 {
            take(1)?;
        }
        if g & GMC_DST_CLIPPING != 0 {
            let sc = take(2)?;
            writes.push((SC_TOP_LEFT, sc[0]));
            writes.push((SC_BOTTOM_RIGHT, sc[1]));
        }
        let brush: &[u32] = match (g >> 4) & 0xf {
            0 => &[
                DP_BRUSH_BKGD_CLR,
                DP_BRUSH_FRGD_CLR,
                BRUSH_DATA0,
                BRUSH_DATA0 + 4,
            ],
            1 => &[DP_BRUSH_FRGD_CLR, BRUSH_DATA0, BRUSH_DATA0 + 4],
            6 => &[DP_BRUSH_BKGD_CLR, DP_BRUSH_FRGD_CLR, BRUSH_DATA0],
            7 => &[DP_BRUSH_FRGD_CLR, BRUSH_DATA0],
            13 | 14 => &[DP_BRUSH_FRGD_CLR],
            _ => &[],
        };
        for &reg in brush {
            writes.push((reg, take(1)?[0]));
        }
        if (g >> 4) & 0xf == 10 {
            let bpp = bytes_per_pixel(g >> 8).unwrap_or(4);
            for (k, &v) in take(16 * bpp)?.iter().enumerate() {
                writes.push((BRUSH_DATA0 + 4 * k as u32, v));
            }
        }
        if g & GMC_LD_BRUSH_Y_X != 0 {
            writes.push((BRUSH_Y_X, take(1)?[0]));
        }
        for (reg, v) in writes {
            self.write_reg(reg, v, u32::MAX, mem);
        }
        Some(i)
    }
}

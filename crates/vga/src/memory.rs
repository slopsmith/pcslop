//! CPU accesses to the legacy 0xA0000..0xC0000 window.

use crate::Vga;

const PLANE_MASK: usize = 0xffff;

/// Where a window access lands in planar memory.
struct Target {
    offset: usize,
    /// Plane returned by read mode 0.
    read_plane: usize,
    /// Planes a write may touch, before the map mask.
    write_planes: u8,
}

fn expand(bit: bool) -> u8 {
    if bit { 0xff } else { 0 }
}

impl Vga {
    /// Offset into the window selected by the graphics controller's memory map field.
    fn window_offset(&self, addr: u32) -> Option<usize> {
        let (base, size) = match (self.gfx[6] >> 2) & 3 {
            0 => (0xa_0000, 0x2_0000),
            1 => (0xa_0000, 0x1_0000),
            2 => (0xb_0000, 0x8000),
            _ => (0xb_8000, 0x8000),
        };
        let off = addr.wrapping_sub(base);
        (off < size).then_some(off as usize)
    }

    fn target(&self, addr: u32) -> Option<Target> {
        if self.vbe.enabled() {
            let off = addr.wrapping_sub(0xa_0000);
            if off >= 0x1_0000 {
                return None;
            }
            return Some(Target {
                offset: off as usize + self.vbe.bank_offset as usize,
                read_plane: (self.gfx[4] & 3) as usize,
                write_planes: 0x0f,
            });
        }
        let off = self.window_offset(addr)?;
        let t = if self.seq[4] & 0x08 != 0 {
            Target { offset: off & !3, read_plane: off & 3, write_planes: 1 << (off & 3) }
        } else {
            let read_plane =
                if self.gfx[5] & 0x10 != 0 { (self.gfx[4] & 2) as usize | (off & 1) } else { (self.gfx[4] & 3) as usize };
            let write_planes = if self.seq[4] & 0x04 == 0 { if off & 1 != 0 { 0x0a } else { 0x05 } } else { 0x0f };
            let offset = if self.gfx[6] & 0x02 != 0 { off & !1 } else { off };
            Target { offset, read_plane, write_planes }
        };
        Some(Target { offset: t.offset & PLANE_MASK, ..t })
    }

    /// Byte offset in VRAM of the linear (packed-pixel VBE) window, if active.
    fn linear_window(&self, addr: u32) -> Option<Option<usize>> {
        if !self.vbe.enabled() || self.vbe.bits() == 4 {
            return None;
        }
        let off = addr.wrapping_sub(0xa_0000);
        Some((off < 0x1_0000).then(|| (self.vbe.bank_offset + off) as usize % self.vram.len()))
    }

    pub fn mem_read8(&mut self, addr: u32) -> u8 {
        if let Some(lin) = self.linear_window(addr) {
            return lin.map_or(0xff, |i| self.vram[i]);
        }
        let Some(t) = self.target(addr) else { return 0xff };
        let base = t.offset * 4;
        if base + 4 > self.vram.len() {
            return 0xff;
        }
        self.latch.copy_from_slice(&self.vram[base..base + 4]);
        if self.gfx[5] & 0x08 == 0 {
            return self.latch[t.read_plane];
        }
        let compare = self.gfx[2];
        let care = self.gfx[7];
        let mut diff = 0;
        for p in 0..4 {
            if care >> p & 1 != 0 {
                diff |= self.latch[p] ^ expand(compare >> p & 1 != 0);
            }
        }
        !diff
    }

    pub fn mem_write8(&mut self, addr: u32, v: u8) {
        if let Some(lin) = self.linear_window(addr) {
            if let Some(i) = lin {
                self.vram[i] = v;
            }
            return;
        }
        let Some(t) = self.target(addr) else { return };
        let base = t.offset * 4;
        if base + 4 > self.vram.len() {
            return;
        }
        let planes = self.seq[2] & t.write_planes;
        let g = &self.gfx;
        let rotate = g[3] & 7;
        let mut data = [0u8; 4];
        let mut bit_mask = g[8];
        match g[5] & 3 {
            0 => {
                let r = v.rotate_right(rotate as u32);
                for (p, d) in data.iter_mut().enumerate() {
                    *d = if g[1] >> p & 1 != 0 { expand(g[0] >> p & 1 != 0) } else { r };
                }
            }
            1 => {
                for p in 0..4 {
                    if planes >> p & 1 != 0 {
                        self.vram[base + p] = self.latch[p];
                    }
                }
                return;
            }
            2 => {
                for (p, d) in data.iter_mut().enumerate() {
                    *d = expand(v >> p & 1 != 0);
                }
            }
            _ => {
                bit_mask &= v.rotate_right(rotate as u32);
                for (p, d) in data.iter_mut().enumerate() {
                    *d = expand(g[0] >> p & 1 != 0);
                }
            }
        }
        let func = (g[3] >> 3) & 3;
        for (p, &d) in data.iter().enumerate() {
            if planes >> p & 1 == 0 {
                continue;
            }
            let l = self.latch[p];
            let d = match func {
                0 => d,
                1 => d & l,
                2 => d | l,
                _ => d ^ l,
            };
            self.vram[base + p] = (d & bit_mask) | (l & !bit_mask);
        }
    }
}

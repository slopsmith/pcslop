//! The Z buffer's compression mask (ZMASK). The card keeps 2 bits per 4x4
//! tile of the Z buffer in on-chip RAM, and with `ZB_BW_CNTL.FAST_FILL` a
//! tile whose bits are 0 reads as `ZB_DEPTHCLEARVALUE` instead of what VRAM
//! holds. Drivers clear depth by zeroing the mask with `3D_CLEAR_ZMASK` and
//! never write the Z buffer: Catalyst does this for OpenGL.
//!
//! We keep the Z buffer uncompressed, so the first draw that reads it with
//! FAST_FILL on writes the clear value into every cleared block inside the
//! scissor and marks the block uncompressed. The mask has no idea of the
//! Z buffer's height and one mask serves whichever Z buffer is bound: the
//! card only ever touches blocks it draws into, so filling blocks outside
//! the scissor ran past small Z buffers into whatever followed them. Mask semantics, and the block a dword covers, as
//! Mesa's r300 driver (r300_blit.c, r300_texture_desc.c; Marek Olšák and
//! the Mesa authors) describes them for R300's two Z pipes: one dword per
//! 32x16 pixels, row by row across `ZB_ZMASK_PITCH`.

use super::regs::*;
use crate::Radeon;

/// Dwords of ZMASK RAM: 4096 for each of R300's two Z pipes.
const DWORDS: usize = 2 * 4096;
const BLOCK_W: usize = 32;
const BLOCK_H: usize = 16;

#[derive(snapshot::Snap)]
pub(crate) struct ZMask {
    words: Vec<u32>,
    /// Some dword is 0 and waits for its clear value.
    cleared: bool,
}

impl Default for ZMask {
    fn default() -> Self {
        ZMask { words: vec![u32::MAX; DWORDS], cleared: false }
    }
}

impl ZMask {
    /// 3D_CLEAR_ZMASK: sets `count` dwords from `start` to `value`.
    pub fn clear(&mut self, start: u32, count: u32, value: u32) {
        let start = (start as usize).min(DWORDS);
        let end = start.saturating_add(count as usize).min(DWORDS);
        self.words[start..end].fill(value);
        self.cleared |= value == 0 && start < end;
    }
}

/// A run of Z buffer blocks to fill: byte offset in VRAM, bytes per row,
/// bytes to write in each row, rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fill {
    pub offset: usize,
    pub stride: usize,
    pub width: usize,
    pub rows: usize,
}

impl Radeon {
    /// Writes the clear value into the Z buffer blocks the mask says are
    /// cleared, if the next draw reads them.
    pub(crate) fn resolve_zmask(&mut self) {
        if !self.r3d.zmask.cleared || self.reg(ZB_BW_CNTL) & FAST_FILL == 0 || self.reg(ZB_CNTL) & 3 == 0 {
            return;
        }
        let Some(base) = self.surface_vram(self.reg(ZB_DEPTHOFFSET) & !31) else { return };
        let pitch = (self.reg(ZB_DEPTHPITCH) & 0x3ffc) as usize;
        let mask_pitch = (self.reg(ZB_ZMASK_PITCH) & 0x3ffe) as usize;
        let bpp = if self.reg(ZB_FORMAT) & 15 < 2 { 2 } else { 4 };
        let value = self.reg(ZB_DEPTHCLEARVALUE);
        let offset = if self.reg(GB_TILE_CONFIG) & (1 << 16) != 0 { 1088 } else { 1440 };
        let scissor = self.rect(SC_SCISSOR0, SC_SCISSOR1, offset);
        let fills = take_cleared(&mut self.r3d.zmask, base, pitch, mask_pitch, bpp, scissor);
        let vram_len = self.vga.vram_mut().len();
        let fills: Vec<Fill> = fills.into_iter().filter_map(|f| clip(f, vram_len)).collect();
        if fills.is_empty() {
            return;
        }
        if let Some(m) = self.metal.as_mut()
            && fills.iter().all(|f| m.fill(*f, bpp, value))
        {
            self.vga.touch();
            return;
        }
        self.gpu_sync();
        let vram = self.vga.vram_mut();
        for f in fills {
            fill_cpu(vram, f, bpp, value);
        }
        self.vga.touch();
    }
}

/// Marks every cleared dword whose block meets `scissor` (inclusive x0, y0,
/// x1, y1 in pixels) uncompressed and returns the Z buffer rows it covered
/// inside the scissor, with blocks next to each other in a row merged.
pub(crate) fn take_cleared(z: &mut ZMask, base: usize, pitch: usize, mask_pitch: usize, bpp: usize, scissor: [i32; 4]) -> Vec<Fill> {
    let per_row = mask_pitch / BLOCK_W;
    if per_row == 0 || pitch == 0 {
        return Vec::new();
    }
    let [sx0, sy0, sx1, sy1] = scissor.map(|v| v.max(0) as usize);
    let (sx1, sy1) = ((sx1 + 1).min(pitch), sy1 + 1);
    let mut fills: Vec<Fill> = Vec::new();
    let mut left = false;
    for (d, w) in z.words.iter_mut().enumerate() {
        if *w != 0 {
            continue;
        }
        let (bx, by) = (d % per_row * BLOCK_W, d / per_row * BLOCK_H);
        let (x0, x1) = (bx.max(sx0), (bx + BLOCK_W).min(sx1));
        let (y0, y1) = (by.max(sy0), (by + BLOCK_H).min(sy1));
        if x0 >= x1 || y0 >= y1 {
            left = true;
            continue;
        }
        *w = u32::MAX;
        let width = (x1 - x0) * bpp;
        let offset = base + (y0 * pitch + x0) * bpp;
        let rows = y1 - y0;
        match fills.last_mut() {
            Some(f) if f.offset + f.width == offset && f.rows == rows => f.width += width,
            _ => fills.push(Fill { offset, stride: pitch * bpp, width, rows }),
        }
    }
    z.cleared = left;
    fills
}

fn clip(mut f: Fill, len: usize) -> Option<Fill> {
    if f.offset + f.width > len {
        return None;
    }
    f.rows = f.rows.min((len - f.offset - f.width) / f.stride + 1);
    Some(f)
}

pub(crate) fn fill_cpu(vram: &mut [u8], f: Fill, bpp: usize, value: u32) {
    for r in 0..f.rows {
        let row = &mut vram[f.offset + r * f.stride..][..f.width];
        for px in row.chunks_exact_mut(bpp) {
            px.copy_from_slice(&value.to_le_bytes()[..bpp]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERYWHERE: [i32; 4] = [0, 0, 8191, 8191];

    #[test]
    fn each_dword_covers_32x16_pixels_row_by_row_and_neighbours_merge() {
        let mut z = ZMask::default();
        z.clear(1, 2, 0);
        z.clear(45, 1, 0);
        let fills = take_cleared(&mut z, 0x1000, 1440, 1440, 4, EVERYWHERE);
        assert_eq!(
            fills,
            [
                Fill { offset: 0x1000 + 32 * 4, stride: 1440 * 4, width: 64 * 4, rows: 16 },
                Fill { offset: 0x1000 + 16 * 1440 * 4, stride: 1440 * 4, width: 32 * 4, rows: 16 },
            ]
        );
        assert!(take_cleared(&mut z, 0x1000, 1440, 1440, 4, EVERYWHERE).is_empty());
    }

    #[test]
    fn a_nonzero_clear_leaves_tiles_reading_from_memory() {
        let mut z = ZMask::default();
        z.clear(0, 0xa05, u32::MAX);
        assert!(!z.cleared);
        z.clear(0, 0xa05, 0);
        z.clear(0, 0xa05, u32::MAX);
        assert!(take_cleared(&mut z, 0, 1440, 1440, 4, EVERYWHERE).is_empty());
    }

    #[test]
    fn only_blocks_inside_the_scissor_are_filled_and_the_rest_stay_cleared() {
        let mut z = ZMask::default();
        z.clear(0, 4096, 0);
        let fills = take_cleared(&mut z, 0x1000, 288, 256, 4, [0, 0, 255, 255]);
        let end = fills.iter().map(|f| f.offset + (f.rows - 1) * f.stride + f.width).max().unwrap();
        assert_eq!(end, 0x1000 + (255 * 288 + 256) * 4, "nothing past the 256x256 scissor");
        assert!(z.cleared);
        let rest = take_cleared(&mut z, 0x1000, 640, 640, 4, [0, 0, 639, 479]);
        assert!(!rest.is_empty());
    }
}

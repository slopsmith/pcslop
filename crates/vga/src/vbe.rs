//! Bochs VBE "DISPI" registers at ports 0x1CE (index) and 0x1CF (data).
//!
//! Register semantics and the sanity fixups follow QEMU hw/display/vga.c
//! (vbe_ioport_*, vbe_fixup_regs), by Fabrice Bellard and contributors.

use crate::{VRAM_SIZE, Vga};

pub const INDEX_ID: u16 = 0;
pub const INDEX_XRES: u16 = 1;
pub const INDEX_YRES: u16 = 2;
pub const INDEX_BPP: u16 = 3;
pub const INDEX_ENABLE: u16 = 4;
pub const INDEX_BANK: u16 = 5;
pub const INDEX_VIRT_WIDTH: u16 = 6;
pub const INDEX_VIRT_HEIGHT: u16 = 7;
pub const INDEX_X_OFFSET: u16 = 8;
pub const INDEX_Y_OFFSET: u16 = 9;
pub const INDEX_VIDEO_MEMORY_64K: u16 = 10;

pub const ID0: u16 = 0xb0c0;
pub const ID5: u16 = 0xb0c5;

pub const ENABLED: u16 = 0x01;
pub const GETCAPS: u16 = 0x02;
pub const DAC_8BIT: u16 = 0x20;
pub const LFB_ENABLED: u16 = 0x40;
pub const NOCLEARMEM: u16 = 0x80;

const MAX_XRES: u16 = 16000;
const MAX_YRES: u16 = 12000;
const MAX_BPP: u16 = 32;
const BANK_MASK: u16 = (VRAM_SIZE >> 16) as u16 - 1;

pub(crate) struct Vbe {
    index: u16,
    regs: [u16; 10],
    /// Bytes per scanline.
    pub line_offset: u32,
    /// VRAM byte offset of the first displayed pixel.
    pub start: u32,
    pub bank_offset: u32,
}

impl Vbe {
    pub fn new() -> Self {
        let mut regs = [0; 10];
        regs[INDEX_ID as usize] = ID5;
        Vbe { index: 0, regs, line_offset: 0, start: 0, bank_offset: 0 }
    }

    pub fn enabled(&self) -> bool {
        self.regs[INDEX_ENABLE as usize] & ENABLED != 0
    }

    pub fn reg(&self, i: u16) -> u16 {
        self.regs[i as usize]
    }

    /// Bits per pixel in VRAM; 15 bpp takes 16.
    pub fn bits(&self) -> u32 {
        match self.reg(INDEX_BPP) {
            15 => 16,
            b => b as u32,
        }
    }

    fn fixup(&mut self) {
        if !self.enabled() {
            return;
        }
        let r = &mut self.regs;
        let bits = match r[INDEX_BPP as usize] {
            4 | 8 | 16 | 24 | 32 => r[INDEX_BPP as usize] as u32,
            15 => 16,
            _ => {
                r[INDEX_BPP as usize] = 8;
                8
            }
        };

        let xres = &mut r[INDEX_XRES as usize];
        *xres = (*xres & !7).clamp(8, MAX_XRES);
        let xres = *xres;
        let vw = &mut r[INDEX_VIRT_WIDTH as usize];
        *vw = (*vw & !7).min(MAX_XRES).max(xres);

        let line = r[INDEX_VIRT_WIDTH as usize] as u32 * bits / 8;
        let max_y = VRAM_SIZE as u32 / line;
        let yres = &mut r[INDEX_YRES as usize];
        *yres = (*yres).clamp(1, MAX_YRES).min(max_y.min(u16::MAX as u32) as u16);
        let yres = *yres as u32;

        r[INDEX_X_OFFSET as usize] = r[INDEX_X_OFFSET as usize].min(MAX_XRES);
        r[INDEX_Y_OFFSET as usize] = r[INDEX_Y_OFFSET as usize].min(MAX_YRES);
        let size = VRAM_SIZE as u32;
        let mut offset = r[INDEX_X_OFFSET as usize] as u32 * bits / 8 + r[INDEX_Y_OFFSET as usize] as u32 * line;
        if offset + yres * line > size {
            r[INDEX_Y_OFFSET as usize] = 0;
            offset = r[INDEX_X_OFFSET as usize] as u32 * bits / 8;
            if offset + yres * line > size {
                r[INDEX_X_OFFSET as usize] = 0;
                offset = 0;
            }
        }

        r[INDEX_VIRT_HEIGHT as usize] = max_y.min(u16::MAX as u32) as u16;
        self.line_offset = line;
        self.start = offset;
    }
}

impl Vga {
    pub(crate) fn vbe_read(&mut self, port: u16) -> u16 {
        let v = &self.vbe;
        if port == 0x1ce {
            return v.index;
        }
        match v.index {
            i @ 0..=9 => {
                if v.regs[INDEX_ENABLE as usize] & GETCAPS != 0 {
                    match i {
                        INDEX_XRES => return MAX_XRES,
                        INDEX_YRES => return MAX_YRES,
                        INDEX_BPP => return MAX_BPP,
                        _ => {}
                    }
                }
                v.regs[i as usize]
            }
            INDEX_VIDEO_MEMORY_64K => (VRAM_SIZE >> 16) as u16,
            _ => 0,
        }
    }

    pub(crate) fn vbe_write(&mut self, port: u16, val: u16) {
        if port == 0x1ce {
            self.vbe.index = val;
            return;
        }
        let v = &mut self.vbe;
        match v.index {
            INDEX_ID => {
                if (ID0..=ID5).contains(&val) {
                    v.regs[0] = val;
                }
            }
            INDEX_XRES | INDEX_YRES | INDEX_BPP | INDEX_VIRT_WIDTH | INDEX_X_OFFSET | INDEX_Y_OFFSET => {
                v.regs[v.index as usize] = val;
                v.fixup();
            }
            INDEX_BANK => {
                let b = val & BANK_MASK;
                v.regs[INDEX_BANK as usize] = b;
                v.bank_offset = (b as u32) << 16;
            }
            INDEX_ENABLE => {
                if val & ENABLED != 0 && !v.enabled() {
                    v.regs[INDEX_VIRT_WIDTH as usize] = 0;
                    v.regs[INDEX_X_OFFSET as usize] = 0;
                    v.regs[INDEX_Y_OFFSET as usize] = 0;
                    v.regs[INDEX_ENABLE as usize] |= ENABLED;
                    v.fixup();
                    if val & NOCLEARMEM == 0 {
                        let n = (v.reg(INDEX_YRES) as usize * v.line_offset as usize).min(VRAM_SIZE);
                        self.vram[..n].fill(0);
                    }
                } else {
                    v.bank_offset = 0;
                }
                self.dac_8bit = val & DAC_8BIT != 0;
                self.vbe.regs[INDEX_ENABLE as usize] = val;
            }
            _ => {}
        }
    }
}

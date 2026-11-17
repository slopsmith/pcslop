//! ATI Radeon 9700 (R300): the PCI function's registers and its legacy VGA.
//!
//! The register file is reached three ways, all ending in [`Radeon::access`]:
//! BAR2 (64 KiB of MMIO), BAR1 (256 I/O ports that mirror the first 256 bytes
//! of MMIO), and MM_INDEX/MM_DATA, which reach any register or VRAM through
//! those first bytes. The legacy VGA keeps its planes interleaved at the start
//! of VRAM, as QEMU's stdvga and ati-vga do.

mod names;
pub mod regs;
mod trace;

#[cfg(test)]
mod tests;

use std::io::Write;

use regs::*;
pub use trace::Trace;
use vga::Vga;

pub const PCI_VENDOR: u16 = 0x1002;
/// Radeon 9700 Pro ("ND"), the ID in the supplied ROM's PCI data structure.
pub const PCI_DEVICE: u16 = 0x4e44;
pub const PCI_CLASS: u32 = 0x03_00_00;
pub const VRAM_SIZE: usize = 128 << 20;
pub const IO_SIZE: u32 = 256;
pub const MMIO_SIZE: u32 = 64 << 10;
/// BAR0 holds two apertures onto VRAM, the second one byte-swapping on real
/// cards. CONFIG_APER_SIZE reports the size of one, as in QEMU's ati-vga
/// (hw/display/ati.c, BALATON Zoltan).
pub const APER_SIZE: u32 = VRAM_SIZE as u32 / 2;

/// Where an access to the register file came from, for the trace.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// BAR2.
    Mmio,
    /// BAR1.
    Io,
    /// MM_INDEX/MM_DATA.
    Indexed,
}

/// Bus addresses of the BARs, as the PCI configuration space last placed them.
#[derive(Clone, Copy, Default, Debug)]
pub struct Bars {
    pub fb: u32,
    pub io: u16,
    pub mmio: u32,
}

pub struct Radeon {
    vga: Vga,
    rom: Vec<u8>,
    regs: Vec<u32>,
    pll: [u32; 64],
    pub bars: Bars,
    /// The PCI configuration space, which MMIO 0xF00..0x1000 mirrors.
    pub config: [u8; 256],
    now_ns: u64,
    trace: Option<Trace>,
}

impl Radeon {
    pub fn new(rom: Vec<u8>) -> Self {
        let mut r = Radeon {
            vga: Vga::with_vram(VRAM_SIZE, false),
            rom,
            regs: vec![0; MMIO_SIZE as usize / 4],
            pll: [0; 64],
            bars: Bars::default(),
            config: [0; 256],
            now_ns: 0,
            trace: None,
        };
        r.reset();
        r
    }

    /// Power-on register values.
    pub fn reset(&mut self) {
        self.regs.fill(0);
        self.pll = [0; 64];
    }

    pub fn set_trace(&mut self, out: Box<dyn Write + Send>) {
        self.trace = Some(Trace::new(out));
    }

    /// Writes a free-form line to the trace, if there is one.
    pub fn note(&mut self, text: &str) {
        if let Some(t) = &mut self.trace {
            t.line(text.to_string());
        }
    }

    pub fn set_time(&mut self, now_ns: u64) {
        self.now_ns = now_ns;
    }

    pub fn rom(&self) -> &[u8] {
        &self.rom
    }

    pub fn vga(&mut self) -> &mut Vga {
        &mut self.vga
    }

    pub fn vram(&self) -> &[u8] {
        self.vga.vram()
    }

    pub fn vram_mut(&mut self) -> &mut [u8] {
        self.vga.vram_mut()
    }

    /// VRAM offset that byte `off` of BAR0 reaches.
    pub fn fb_offset(off: u32) -> usize {
        (off % APER_SIZE) as usize
    }

    pub fn mmio_read(&mut self, off: u32, size: u8) -> u32 {
        self.access(Source::Mmio, off, size, None)
    }

    pub fn mmio_write(&mut self, off: u32, size: u8, v: u32) {
        self.access(Source::Mmio, off, size, Some(v));
    }

    pub fn io_read(&mut self, off: u16, size: u8) -> u32 {
        self.access(Source::Io, off as u32 & 0xff, size, None)
    }

    pub fn io_write(&mut self, off: u16, size: u8, v: u32) {
        self.access(Source::Io, off as u32 & 0xff, size, Some(v));
    }

    /// A legacy VGA port. None if the VGA does not decode it.
    pub fn vga_io_read(&mut self, port: u16, size: u8) -> Option<u32> {
        let v = self.vga.io_read(port, size, self.now_ns)?;
        if let Some(t) = &mut self.trace {
            t.line(format!("vga R{size} {port:#05x} = {v:#x}"));
        }
        Some(v)
    }

    pub fn vga_io_write(&mut self, port: u16, size: u8, v: u32) -> bool {
        let ours = self.vga.io_write(port, size, v);
        if ours && let Some(t) = &mut self.trace {
            t.line(format!("vga W{size} {port:#05x} = {v:#x}"));
        }
        ours
    }

    /// Reads or writes `size` bytes of the register file at byte offset `off`.
    pub fn access(&mut self, src: Source, off: u32, size: u8, write: Option<u32>) -> u32 {
        let reg = off & 0xfffc;
        let shift = (off & 3) * 8;
        let lanes = if size >= 4 { u32::MAX } else { (1u32 << (8 * size as u32)) - 1 };
        let mask = lanes << shift;
        match reg {
            MM_DATA => {
                let index = self.regs[MM_INDEX as usize / 4];
                if index & 0x8000_0000 != 0 {
                    return self.vram_dword(index & 0x7fff_fffc, shift, mask, write);
                }
                return self.access(Source::Indexed, (index & 0xfffc) + (off & 3), size, write);
            }
            CLOCK_CNTL_DATA => return self.pll_access(shift, mask, write),
            _ => {}
        }
        match write {
            None => {
                let v = (self.read_reg(reg) & mask) >> shift;
                if reg != MM_INDEX && reg != CLOCK_CNTL_INDEX {
                    self.trace_reg(src, 'R', size, off, v);
                }
                v
            }
            Some(v) => {
                let v = (v << shift) & mask;
                if reg != MM_INDEX && reg != CLOCK_CNTL_INDEX {
                    self.trace_reg(src, 'W', size, off, v >> shift);
                }
                self.write_reg(reg, v, mask);
                0
            }
        }
    }

    fn vram_dword(&mut self, addr: u32, shift: u32, mask: u32, write: Option<u32>) -> u32 {
        let a = addr as usize % VRAM_SIZE;
        let mut d = u32::from_le_bytes(self.vga.vram()[a..a + 4].try_into().unwrap());
        match write {
            None => (d & mask) >> shift,
            Some(v) => {
                d = (d & !mask) | ((v << shift) & mask);
                self.vga.vram_mut()[a..a + 4].copy_from_slice(&d.to_le_bytes());
                0
            }
        }
    }

    fn pll_access(&mut self, shift: u32, mask: u32, write: Option<u32>) -> u32 {
        let index_reg = self.regs[CLOCK_CNTL_INDEX as usize / 4];
        let index = (index_reg & 0x3f) as usize;
        match write {
            None => {
                let v = self.read_pll(index);
                let name = names::pll_name(index as u32);
                if let Some(t) = &mut self.trace {
                    t.line(format!("pll R {index:#04x} {name} = {v:#010x}"));
                }
                (v & mask) >> shift
            }
            Some(v) => {
                let v = (v << shift) & mask;
                let enabled = index_reg & PLL_WR_EN != 0;
                let name = names::pll_name(index as u32);
                if let Some(t) = &mut self.trace {
                    let note = if enabled { "" } else { " (PLL_WR_EN clear: dropped)" };
                    t.line(format!("pll W {index:#04x} {name} = {:#010x} mask {mask:#010x}{note}", v));
                }
                if enabled {
                    self.write_pll(index, v, mask);
                }
                0
            }
        }
    }

    fn read_pll(&self, index: usize) -> u32 {
        self.pll[index]
    }

    fn write_pll(&mut self, index: usize, v: u32, mask: u32) {
        let p = &mut self.pll[index];
        *p = (*p & !mask) | (v & mask);
    }

    fn read_reg(&self, reg: u32) -> u32 {
        match reg {
            CONFIG_APER_0_BASE => self.bars.fb,
            CONFIG_APER_1_BASE => self.bars.fb.wrapping_add(APER_SIZE),
            CONFIG_APER_SIZE => APER_SIZE,
            CONFIG_REG_1_BASE => self.bars.mmio,
            CONFIG_REG_APER_SIZE => MMIO_SIZE,
            RBBM_STATUS => RBBM_FIFOCNT_IDLE,
            0x0f00..=0x0ffc => {
                let i = (reg - 0x0f00) as usize;
                u32::from_le_bytes(self.config[i..i + 4].try_into().unwrap())
            }
            _ => self.regs[reg as usize / 4],
        }
    }

    fn write_reg(&mut self, reg: u32, v: u32, mask: u32) {
        let r = &mut self.regs[reg as usize / 4];
        *r = (*r & !mask) | (v & mask);
        if reg == DAC_CNTL {
            let on = self.regs[DAC_CNTL as usize / 4] & DAC_8BIT_EN != 0;
            self.vga.set_dac_8bit(on);
        }
    }

    fn trace_reg(&mut self, src: Source, dir: char, size: u8, off: u32, v: u32) {
        if let Some(t) = &mut self.trace {
            let src = match src {
                Source::Mmio => "mmio",
                Source::Io => "io",
                Source::Indexed => "idx",
            };
            let name = names::mmio_name(off & 0xfffc);
            let byte = if off & 3 != 0 { format!("+{}", off & 3) } else { String::new() };
            t.line(format!("{src} {dir}{size} {:#06x}{byte} {name} = {v:#x}", off & 0xfffc));
        }
    }

    /// Draws the current picture into `out` as 0x00RRGGBB pixels.
    pub fn render(&mut self, out: &mut Vec<u32>) -> Option<(u32, u32)> {
        self.vga.render(out)
    }

    pub fn take_changed(&mut self) -> bool {
        self.vga.take_changed()
    }
}

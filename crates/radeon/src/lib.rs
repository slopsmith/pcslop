//! ATI Radeon 9700 (R300): the PCI function's registers and its legacy VGA.
//!
//! The register file is reached three ways, all ending in [`Radeon::access`]:
//! BAR2 (64 KiB of MMIO), BAR1 (256 I/O ports that mirror the first 256 bytes
//! of MMIO), and MM_INDEX/MM_DATA, which reach any register or VRAM through
//! those first bytes. The legacy VGA keeps its planes interleaved at the start
//! of VRAM, as QEMU's stdvga and ati-vga do.

mod cp;
mod crtc;
mod ddc;
mod engine;
mod i2c;
mod mem;
mod names;
mod r3d;
pub mod regs;
mod trace;

#[cfg(test)]
mod cp_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod tests;

use std::io::Write;

pub use crtc::Mode;
pub use mem::{BusMemory, NoBus};
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
    /// A packet the command processor executed.
    Cp,
}

/// Bus addresses of the BARs, as the PCI configuration space last placed them.
#[derive(Clone, Copy, Default, Debug)]
pub struct Bars {
    pub fb: u32,
    pub io: u16,
    pub mmio: u32,
}

#[derive(snapshot::Snap)]
pub struct Radeon {
    vga: Vga,
    #[snap(skip)]
    rom: Vec<u8>,
    regs: Vec<u32>,
    pll: [u32; 64],
    #[snap(skip)]
    pub bars: Bars,
    /// The PCI configuration space, which MMIO 0xF00..0x1000 mirrors.
    #[snap(skip)]
    pub config: [u8; 256],
    #[snap(skip)]
    now_ns: u64,
    /// Frames whose vertical blank CRTC_STATUS and GEN_INT_STATUS have been acknowledged up to.
    vblank_acked: [u64; 2],
    /// GEN_INT_STATUS bits other than the vertical blank, which follows the clock.
    int_status: u32,
    cp: cp::Cp,
    engine: engine::Engine,
    i2c: i2c::I2cEngine,
    r3d: r3d::ThreeD,
    /// The monitor on the VGA connector's DDC lines.
    monitor: ddc::Ddc,
    #[snap(skip)]
    trace: Option<Trace>,
    #[snap(skip)]
    metal: Option<Box<r3d::metal::MetalR3d>>,
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
            vblank_acked: [0; 2],
            int_status: 0,
            cp: cp::Cp::default(),
            engine: engine::Engine::default(),
            i2c: i2c::I2cEngine::default(),
            r3d: r3d::ThreeD::default(),
            monitor: ddc::Ddc::default(),
            trace: None,
            metal: None,
        };
        r.reset();
        r
    }

    /// Power-on register values.
    pub fn reset(&mut self) {
        self.regs.fill(0);
        self.pll = [0; 64];
        self.int_status = 0;
        self.cp = cp::Cp::default();
        self.engine = engine::Engine::default();
        self.i2c = i2c::I2cEngine::default();
        self.r3d = r3d::ThreeD::default();
        for r in [DEFAULT_SC_BOTTOM_RIGHT, SC_BOTTOM_RIGHT] {
            self.regs[r as usize / 4] = 0x1fff_1fff;
            self.engine_register_written(r, 0x1fff_1fff, &mut NoBus);
        }
        self.regs[DP_WRITE_MASK as usize / 4] = u32::MAX;
        self.regs[CLR_CMP_MASK as usize / 4] = u32::MAX;
        self.vga.set_bank_window(None);
        self.monitor = ddc::Ddc::default();
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
        self.access(Source::Mmio, off, size, None, &mut NoBus)
    }

    /// Writes a register. `mem` is what the card reaches by bus mastering.
    pub fn mmio_write(&mut self, off: u32, size: u8, v: u32, mem: &mut dyn BusMemory) {
        self.access(Source::Mmio, off, size, Some(v), mem);
    }

    pub fn io_read(&mut self, off: u16, size: u8) -> u32 {
        self.access(Source::Io, off as u32 & 0xff, size, None, &mut NoBus)
    }

    pub fn io_write(&mut self, off: u16, size: u8, v: u32, mem: &mut dyn BusMemory) {
        self.access(Source::Io, off as u32 & 0xff, size, Some(v), mem);
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
        if ours && port & 0xfff0 == if self.vga_mono() { 0x3b0 } else { 0x3d0 } {
            let data = matches!((port & 0xf, size), (4, 2..) | (5, _));
            if data {
                self.shadow_vga_crtc(self.vga.crtc_index());
            }
        }
        ours
    }

    fn vga_mono(&mut self) -> bool {
        self.vga
            .io_read(0x3cc, 1, self.now_ns)
            .is_some_and(|m| m & 1 == 0)
    }

    // The ROM's mode set writes CR12 and checks that CRTC_V_TOTAL_DISP follows,
    // or it never clears CRTC_EXT_DISP_EN again.
    fn shadow_vga_crtc(&mut self, index: u8) {
        let cr = |i: u8| self.vga.crtc(i) as u32;
        let ov = cr(7);
        let field = |reg: u32, shift: u32, bits: u32, v: u32| {
            let mask = ((1u32 << bits) - 1) << shift;
            (reg & !mask) | ((v << shift) & mask)
        };
        match index {
            0 => {
                let r = field(self.reg(CRTC_H_TOTAL_DISP), 0, 10, cr(0) + 4);
                self.regs[CRTC_H_TOTAL_DISP as usize / 4] = r;
            }
            1 => {
                let r = field(self.reg(CRTC_H_TOTAL_DISP), 16, 9, cr(1));
                self.regs[CRTC_H_TOTAL_DISP as usize / 4] = r;
            }
            6 | 7 | 0x12 => {
                let total = cr(6) | (ov & 1) << 8 | (ov & 0x20) << 4;
                let disp = cr(0x12) | (ov & 2) << 7 | (ov & 0x40) << 3;
                let r = field(self.reg(CRTC_V_TOTAL_DISP), 0, 11, total + 1);
                self.regs[CRTC_V_TOTAL_DISP as usize / 4] = field(r, 16, 11, disp);
            }
            _ => {}
        }
    }

    /// Reads or writes `size` bytes of the register file at byte offset `off`.
    pub fn access(
        &mut self,
        src: Source,
        off: u32,
        size: u8,
        write: Option<u32>,
        mem: &mut dyn BusMemory,
    ) -> u32 {
        let reg = off & 0xfffc;
        let shift = (off & 3) * 8;
        let lanes = if size >= 4 {
            u32::MAX
        } else {
            (1u32 << (8 * size as u32)) - 1
        };
        let mask = lanes << shift;
        match reg {
            MM_DATA => {
                let index = self.regs[MM_INDEX as usize / 4];
                if index & 0x8000_0000 != 0 {
                    return self.vram_dword(index & 0x7fff_fffc, shift, mask, write);
                }
                return self.access(
                    Source::Indexed,
                    (index & 0xfffc) + (off & 3),
                    size,
                    write,
                    mem,
                );
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
                self.write_reg(reg, v, mask, mem);
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
                    let note = if enabled {
                        ""
                    } else {
                        " (PLL_WR_EN clear: dropped)"
                    };
                    t.line(format!(
                        "pll W {index:#04x} {name} = {:#010x} mask {mask:#010x}{note}",
                        v
                    ));
                }
                if enabled {
                    self.write_pll(index, v, mask);
                }
                0
            }
        }
    }

    fn read_pll(&self, index: usize) -> u32 {
        match index {
            PPLL_REF_DIV | P2PLL_REF_DIV => self.pll[index] & !PLL_ATOMIC_UPDATE,
            _ => self.pll[index],
        }
    }

    fn write_pll(&mut self, index: usize, v: u32, mask: u32) {
        let p = &mut self.pll[index];
        *p = (*p & !mask) | (v & mask);
    }

    fn read_reg(&mut self, reg: u32) -> u32 {
        if idle_polled(reg) {
            self.gpu_sync();
        }
        match reg {
            CRTC_STATUS => {
                let saved = if self.frame() > self.vblank_acked[0] {
                    CRTC_VBLANK_SAVE
                } else {
                    0
                };
                saved | (self.in_vblank() as u32 * CRTC_VBLANK_CUR)
            }
            GEN_INT_STATUS => self.gen_int_status(),
            CP_RB_RPTR => self.cp_rptr(),
            I2C_DATA => self.i2c.pop() as u32,
            CP_STAT | CP_CSQ_STAT => 0,
            0x0790 | 0x07b0 | 0x0a10 => IDLE_QUEUE_STATUS.iter().find(|e| e.0 == reg).map_or(0, |e| e.1),
            RB2D_DSTCACHE_CTLSTAT
            | DSTCACHE_CTLSTAT
            | RB3D_DSTCACHE_CTLSTAT
            | R300_RB3D_DSTCACHE_CTLSTAT
            | R300_RB3D_ZCACHE_CTLSTAT => self.reg(reg) & !CACHE_BUSY,
            DAC_CNTL => {
                let v = self.reg(DAC_CNTL);
                if v & DAC_CMP_EN != 0 {
                    v | DAC_CMP_OUTPUT
                } else {
                    v & !DAC_CMP_OUTPUT
                }
            }
            GPIO_VGA_DDC | GPIO_DVI_DDC | GPIO_MONID | GPIO_CRT2_DDC => {
                let v = self.reg(reg);
                let (scl, mut sda) = gpio_lines(v);
                if reg == GPIO_VGA_DDC {
                    sda &= self.monitor.sda_line();
                }
                (v & !(GPIO_Y_0 | GPIO_Y_1)) | ((sda as u32) * GPIO_Y_0) | ((scl as u32) * GPIO_Y_1)
            }
            CRTC_VLINE_CRNT_VLINE => self.vline() << 16,
            CRTC_CRNT_FRAME => self.frame() as u32 & 0x1f_ffff,
            PALETTE_DATA | PALETTE_30_DATA => {
                let index = self.reg(PALETTE_INDEX);
                let [r, g, b] = self.vga.dac_entry((index >> 16) as u8);
                self.regs[PALETTE_INDEX as usize / 4] =
                    (index & !0x00ff_0000) | ((index + 0x1_0000) & 0x00ff_0000);
                if reg == PALETTE_DATA {
                    (r as u32) << 16 | (g as u32) << 8 | b as u32
                } else {
                    (r as u32) << 22 | (g as u32) << 12 | (b as u32) << 2
                }
            }
            CONFIG_APER_0_BASE => self.bars.fb,
            CONFIG_APER_1_BASE => self.bars.fb.wrapping_add(APER_SIZE),
            CONFIG_APER_SIZE => APER_SIZE,
            CONFIG_REG_1_BASE => self.bars.mmio,
            CONFIG_REG_APER_SIZE => MMIO_SIZE,
            RBBM_STATUS => RBBM_FIFOCNT_IDLE,
            IDCT_STATUS => self.reg(IDCT_STATUS) | IDCT_IDLE,
            MC_STATUS => self.reg(MC_STATUS) | MC_STATUS_IDLE,
            0x0f00..=0x0ffc => {
                let i = (reg - 0x0f00) as usize;
                u32::from_le_bytes(self.config[i..i + 4].try_into().unwrap())
            }
            _ => self.regs[reg as usize / 4],
        }
    }

    fn write_reg(&mut self, reg: u32, v: u32, mask: u32, mem: &mut dyn BusMemory) {
        if fence_written(reg, v & mask) {
            self.gpu_sync();
        }
        let old = self.reg(reg);
        let new = (old & !mask) | (v & mask);
        match reg {
            CRTC_STATUS => {
                if new & CRTC_VBLANK_SAVE != 0 {
                    self.vblank_acked[0] = self.frame();
                }
                return;
            }
            GEN_INT_STATUS => {
                if v & mask & (VSYNC_INT | CRTC_VBLANK_STAT) != 0 {
                    self.vblank_acked[1] = self.frame();
                }
                self.int_status &= !(v & mask & SW_INT_TEST);
                if v & mask & SW_INT_FIRE != 0 {
                    self.int_status |= SW_INT_TEST;
                }
                return;
            }
            PALETTE_DATA | PALETTE_30_DATA => {
                let rgb = if reg == PALETTE_DATA {
                    [(new >> 16) as u8, (new >> 8) as u8, new as u8]
                } else {
                    [(new >> 22) as u8, (new >> 12) as u8, (new >> 2) as u8]
                };
                let index = self.reg(PALETTE_INDEX);
                self.vga.set_dac_entry(index as u8, rgb);
                self.regs[PALETTE_INDEX as usize / 4] = (index & !0xff) | ((index + 1) & 0xff);
                return;
            }
            _ => {}
        }
        self.regs[reg as usize / 4] = new;
        match reg {
            I2C_CNTL_0 => {
                let cntl0 = self.i2c.control(new, self.reg(I2C_CNTL_1), self.monitor.edid());
                self.regs[reg as usize / 4] = cntl0;
                return;
            }
            I2C_DATA => return self.i2c.push(new as u8),
            _ => {}
        }
        if self.cp_register_written(reg, mem)
            || self.engine_register_written(reg, new, mem)
            || self.r3d_register_written(reg, new)
            || self.zpass_written(reg, new, mem)
        {
            return;
        }
        match reg {
            DAC_CNTL => self.vga.set_dac_8bit(new & DAC_8BIT_EN != 0),
            GPIO_VGA_DDC => {
                let (scl, sda) = gpio_lines(new);
                self.monitor.drive(scl, sda);
            }
            CRTC_GEN_CNTL | CRTC_EXT_CNTL | MEM_VGA_WP_SEL | MEM_VGA_RP_SEL => {
                self.update_bank_window();
                self.vga.touch();
            }
            0x0200..=0x027c => self.vga.touch(),
            _ => {}
        }
    }

    // Page units of 8 KiB, or 32 KiB with VGA_ATI_LINEAR, follow 86Box's Rage 128
    // model (vid_ati_rage128_display.c); this ROM's VBE window calls agree.
    fn update_bank_window(&mut self) {
        let ext = self.reg(CRTC_EXT_CNTL);
        let paged = self.extended() || ext & VGA_MEM_PS_EN != 0;
        let shift = if ext & VGA_ATI_LINEAR != 0 { 15 } else { 13 };
        let pages = |r: u32| [(r & 0x3ff) << shift, ((r >> 16) & 0x3ff) << shift];
        let w = paged.then(|| vga::BankWindow {
            read: pages(self.reg(MEM_VGA_RP_SEL)),
            write: pages(self.reg(MEM_VGA_WP_SEL)),
        });
        self.vga.set_bank_window(w);
    }

    fn trace_reg(&mut self, src: Source, dir: char, size: u8, off: u32, v: u32) {
        if let Some(t) = &mut self.trace {
            let src = match src {
                Source::Mmio => "mmio",
                Source::Io => "io",
                Source::Indexed => "idx",
                Source::Cp => "cp",
            };
            let name = names::mmio_name(off & 0xfffc);
            let byte = if off & 3 != 0 {
                format!("+{}", off & 3)
            } else {
                String::new()
            };
            t.line(format!(
                "{src} {dir}{size} {:#06x}{byte} {name} = {v:#x}",
                off & 0xfffc
            ));
        }
    }

    /// Draws the current picture into `out` as 0x00RRGGBB pixels.
    pub fn render(&mut self, out: &mut Vec<u32>) -> Option<(u32, u32)> {
        if self.extended() {
            return self.render_extended(out);
        }
        self.vga.render(out)
    }

    /// GEN_INT_STATUS: the latched vertical blank and the software interrupt.
    fn gen_int_status(&self) -> u32 {
        let vblank = if self.frame() > self.vblank_acked[1] {
            VSYNC_INT | CRTC_VBLANK_STAT
        } else {
            0
        };
        self.int_status | vblank
    }

    /// Level of the card's INTA line.
    pub fn irq(&self) -> bool {
        self.gen_int_status()
            & self.reg(GEN_INT_CNTL)
            & (CRTC_VBLANK_STAT | VSYNC_INT | SW_INT_TEST)
            != 0
    }

    /// When the next vertical blank interrupt is due, if one is enabled and not pending.
    pub fn next_irq_ns(&self) -> Option<u64> {
        let enabled = self.reg(GEN_INT_CNTL) & (CRTC_VBLANK_STAT | VSYNC_INT) != 0;
        (enabled && !self.irq()).then(|| self.next_frame_ns())
    }

    /// Commands, 2D operations and 3D draws run so far.
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.cp.packets, self.engine.ops, self.r3d.draws)
    }

    pub fn take_changed(&mut self) -> bool {
        self.vga.take_changed()
    }
}

/// SCL and SDA as the card drives them: a line is low only while its output is
/// enabled with a 0 (A_1 and EN_1 are SCL, A_0 and EN_0 are SDA).
fn gpio_lines(v: u32) -> (bool, bool) {
    let low = |a: u32, en: u32| v & en != 0 && v & a == 0;
    (!low(GPIO_A_1, GPIO_EN_1), !low(GPIO_A_0, GPIO_EN_0))
}

/// Writes that tell the guest its earlier drawing finished: scratch
/// registers and fences, software interrupts, WAIT_UNTIL and the occlusion
/// counter.
fn fence_written(reg: u32, v: u32) -> bool {
    matches!(reg, SCRATCH_REG0..=SCRATCH_REG7 | CP_RESYNC_DATA | WAIT_UNTIL | r3d::regs::ZB_ZPASS_DATA | r3d::regs::ZB_ZPASS_ADDR)
        || (reg == GEN_INT_STATUS && v & SW_INT_FIRE != 0)
}

/// Status registers drivers poll until the engines go idle.
fn idle_polled(reg: u32) -> bool {
    matches!(
        reg,
        RBBM_STATUS
            | CP_STAT
            | CP_CSQ_STAT
            | RB2D_DSTCACHE_CTLSTAT
            | DSTCACHE_CTLSTAT
            | RB3D_DSTCACHE_CTLSTAT
            | R300_RB3D_DSTCACHE_CTLSTAT
            | R300_RB3D_ZCACHE_CTLSTAT
            | IDCT_STATUS
            | 0x0790
            | 0x07b0
            | 0x0a10
    )
}

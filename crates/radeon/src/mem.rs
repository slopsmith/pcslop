//! The memory controller's address space as the command processor sees it:
//! VRAM at MC_FB_LOCATION, the AGP aperture at MC_AGP_LOCATION, the PCI GART
//! between AIC_LO_ADDR and AIC_HI_ADDR, and the system bus everywhere else.

use crate::regs::*;
use crate::{Radeon, VRAM_SIZE};

/// Guest physical memory as the card's bus master reaches it. The chipset
/// translates its own AGP aperture.
pub trait BusMemory {
    fn read(&mut self, addr: u32, buf: &mut [u8]);
    fn write(&mut self, addr: u32, buf: &[u8]);
}

/// The bus while the guest keeps bus mastering off.
pub struct NoBus;

impl BusMemory for NoBus {
    fn read(&mut self, _addr: u32, buf: &mut [u8]) {
        buf.fill(0xff);
    }

    fn write(&mut self, _addr: u32, _buf: &[u8]) {}
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Place {
    Vram(usize),
    Bus(u32),
}

const PAGE: u32 = 0x1000;

/// Start and inclusive end of a MC_*_LOCATION range, in 64 KiB units.
fn location(v: u32) -> (u32, u32) {
    ((v & 0xffff) << 16, (v & 0xffff_0000) | 0xffff)
}

impl Radeon {
    /// Where the memory controller sends `addr`.
    pub(crate) fn mc_place(&self, mem: &mut dyn BusMemory, addr: u32) -> Place {
        let (fb, fb_end) = location(self.reg(MC_FB_LOCATION));
        if (fb..=fb_end).contains(&addr) {
            return Place::Vram((addr - fb) as usize % VRAM_SIZE);
        }
        let (agp, agp_end) = location(self.reg(MC_AGP_LOCATION));
        if (agp..=agp_end).contains(&addr) {
            return Place::Bus(self.reg(AGP_BASE).wrapping_add(addr - agp));
        }
        let (lo, hi) = (self.reg(AIC_LO_ADDR), self.reg(AIC_HI_ADDR));
        if self.reg(AIC_CNTL) & PCIGART_TRANSLATE_EN != 0 && (lo..=hi).contains(&addr) {
            let entry_addr = (self.reg(AIC_PT_BASE) & !0xfff).wrapping_add((addr - lo) / PAGE * 4);
            let mut entry = [0; 4];
            mem.read(entry_addr, &mut entry);
            return Place::Bus((u32::from_le_bytes(entry) & !0xfff) | (addr & 0xfff));
        }
        Place::Bus(addr)
    }

    pub(crate) fn mc_read(&mut self, mem: &mut dyn BusMemory, addr: u32, buf: &mut [u8]) {
        let mut done = 0;
        while done < buf.len() {
            let a = addr.wrapping_add(done as u32);
            let n = ((PAGE - a % PAGE) as usize).min(buf.len() - done);
            let chunk = &mut buf[done..done + n];
            match self.mc_place(mem, a) {
                Place::Vram(off) => {
                    let vram = self.vga.vram();
                    for (i, b) in chunk.iter_mut().enumerate() {
                        *b = vram[(off + i) % VRAM_SIZE];
                    }
                }
                Place::Bus(p) => mem.read(p, chunk),
            }
            done += n;
        }
    }

    pub(crate) fn mc_write(&mut self, mem: &mut dyn BusMemory, addr: u32, buf: &[u8]) {
        let mut done = 0;
        while done < buf.len() {
            let a = addr.wrapping_add(done as u32);
            let n = ((PAGE - a % PAGE) as usize).min(buf.len() - done);
            let chunk = &buf[done..done + n];
            match self.mc_place(mem, a) {
                Place::Vram(off) => {
                    let vram = self.vga.vram_mut();
                    for (i, &b) in chunk.iter().enumerate() {
                        vram[(off + i) % VRAM_SIZE] = b;
                    }
                }
                Place::Bus(p) => mem.write(p, chunk),
            }
            done += n;
        }
    }

    pub(crate) fn mc_read_dwords(
        &mut self,
        mem: &mut dyn BusMemory,
        addr: u32,
        count: usize,
    ) -> Vec<u32> {
        let mut bytes = vec![0; count * 4];
        self.mc_read(mem, addr, &mut bytes);
        bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c))
            .collect()
    }

    pub(crate) fn mc_write_dword(&mut self, mem: &mut dyn BusMemory, addr: u32, v: u32) {
        self.mc_write(mem, addr, &v.to_le_bytes());
    }

    /// VRAM offset of a surface at `mc`, or None if the memory controller
    /// sends it to system memory through the AGP aperture or the PCI GART.
    pub(crate) fn surface_vram(&self, mc: u32) -> Option<usize> {
        let (agp, agp_end) = location(self.reg(MC_AGP_LOCATION));
        let (lo, hi) = (self.reg(AIC_LO_ADDR), self.reg(AIC_HI_ADDR));
        let gart = self.reg(AIC_CNTL) & PCIGART_TRANSLATE_EN != 0 && (lo..=hi).contains(&mc);
        let (fb, fb_end) = location(self.reg(MC_FB_LOCATION));
        let in_fb = (fb..=fb_end).contains(&mc);
        (in_fb || !((agp..=agp_end).contains(&mc) || gart)).then(|| self.vram_of(mc))
    }

    /// VRAM offset of a surface the 2D engine addresses in the memory controller's space.
    pub(crate) fn vram_of(&self, mc: u32) -> usize {
        let (fb, _) = location(self.reg(MC_FB_LOCATION));
        mc.wrapping_sub(fb) as usize % VRAM_SIZE
    }
}

//! QEMU's "stdvga": an IBM VGA plus the Bochs VBE (DISPI) extension.
//!
//! VRAM layout follows QEMU: the four VGA planes are interleaved at the start
//! of VRAM, so plane offset `o` of plane `p` is VRAM byte `4 * o + p`.
//! Standard VGA modes use the first 256 KiB (plane offsets wrap at 64 KiB).
//! VBE packed-pixel modes treat the same VRAM as a linear framebuffer.
//!
//! Unlike QEMU, CPU accesses do not shift addresses for word or doubleword
//! mode. The CRTC applies byte/word/doubleword addressing when scanning out,
//! as the real hardware does, so plane offsets match the IBM VGA.

mod memory;
mod render;
mod timing;
mod vram;
pub mod vbe;

#[cfg(test)]
mod tests;

pub const PCI_VENDOR: u16 = 0x1234;
pub const PCI_DEVICE: u16 = 0x1111;
pub const VRAM_SIZE: usize = 16 << 20;

// Register write masks from QEMU hw/display/vga.c (Fabrice Bellard).
const SEQ_MASK: [u8; 8] = [0x03, 0x3d, 0x0f, 0x3f, 0x0e, 0x00, 0x00, 0xff];
const GFX_MASK: [u8; 16] = [0x0f, 0x0f, 0x0f, 0x1f, 0x03, 0x7b, 0x0f, 0x0f, 0xff, 0, 0, 0, 0, 0, 0, 0];

/// A packed-pixel window at 0xA0000..0xB0000 in two 32 KiB halves, each
/// mapped at its own VRAM offset for reads and for writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct BankWindow {
    pub read: [u32; 2],
    pub write: [u32; 2],
}

pub struct Vga {
    vram: vram::Vram,
    ext_window: Option<BankWindow>,
    misc: u8,
    feature: u8,
    subsys: u8,
    seq_index: u8,
    seq: [u8; 8],
    gfx_index: u8,
    gfx: [u8; 16],
    crtc_index: u8,
    crtc: [u8; 256],
    attr_index: u8,
    attr: [u8; 0x15],
    attr_flip: bool,
    dac: [u8; 768],
    dac_read: u8,
    dac_write: u8,
    dac_sub: u8,
    dac_cache: [u8; 3],
    dac_reading: bool,
    dac_8bit: bool,
    pel_mask: u8,
    latch: [u8; 4],
    vbe: vbe::Vbe,
    retrace_toggle: u8,
    /// Whether ports 0x1CE/0x1CF answer with the Bochs VBE registers.
    dispi: bool,
    frames: u64,
    scratch: Vec<u32>,
    pixels: Vec<u8>,
    changed: bool,
}

impl Default for Vga {
    fn default() -> Self {
        Self::new()
    }
}

impl Vga {
    pub fn new() -> Self {
        Self::with_vram(VRAM_SIZE, true)
    }

    /// A VGA with `vram_size` bytes of VRAM, and the Bochs VBE registers only if `dispi` is set.
    pub fn with_vram(vram_size: usize, dispi: bool) -> Self {
        Vga {
            vram: vram::Vram::new(vram_size),
            ext_window: None,
            misc: 0,
            feature: 0,
            subsys: 0,
            seq_index: 0,
            seq: [0; 8],
            gfx_index: 0,
            gfx: [0; 16],
            crtc_index: 0,
            crtc: [0; 256],
            attr_index: 0,
            attr: [0; 0x15],
            attr_flip: false,
            dac: [0; 768],
            dac_read: 0,
            dac_write: 0,
            dac_sub: 0,
            dac_cache: [0; 3],
            dac_reading: false,
            dac_8bit: false,
            pel_mask: 0xff,
            latch: [0; 4],
            vbe: vbe::Vbe::new(),
            retrace_toggle: 0,
            dispi,
            frames: 0,
            scratch: Vec::new(),
            pixels: Vec::new(),
            changed: true,
        }
    }

    /// True if VRAM or a register may have changed since the last call, or
    /// in text mode, where the cursor and blinking change the picture anyway.
    pub fn take_changed(&mut self) -> bool {
        let text = !self.vbe.enabled() && self.gfx[6] & 1 == 0;
        std::mem::replace(&mut self.changed, false) || text
    }

    pub fn vram(&self) -> &[u8] {
        &self.vram
    }

    pub fn vram_mut(&mut self) -> &mut [u8] {
        self.changed = true;
        &mut self.vram
    }

    /// Notes that VRAM was written behind the emulator's back, e.g. by a
    /// guest that has it mapped directly.
    pub fn mark_changed(&mut self) {
        self.changed = true;
    }

    /// Where VRAM lives in host memory, 16 KiB aligned, for mapping it into a VM.
    pub fn vram_ptr(&mut self) -> *mut u8 {
        self.vram.as_mut_ptr()
    }

    /// DAC entry `index` as 8-bit red, green and blue.
    pub fn dac_entry(&self, index: u8) -> [u8; 3] {
        let i = index as usize * 3;
        let c = |v: u8| if self.dac_8bit { v } else { (v << 2) | (v >> 4) };
        [c(self.dac[i]), c(self.dac[i + 1]), c(self.dac[i + 2])]
    }

    /// Sets DAC entry `index` from 8-bit red, green and blue.
    pub fn set_dac_entry(&mut self, index: u8, rgb: [u8; 3]) {
        let i = index as usize * 3;
        for (c, v) in rgb.into_iter().enumerate() {
            self.dac[i + c] = if self.dac_8bit { v } else { v >> 2 };
        }
        self.changed = true;
    }

    /// Whether DAC entries hold 8 bits per colour instead of 6.
    pub fn set_dac_8bit(&mut self, on: bool) {
        if on != self.dac_8bit {
            self.dac_8bit = on;
            self.changed = true;
        }
    }

    /// Replaces planar VGA decoding of 0xA0000..0xB0000 with a bank window, or restores it.
    pub fn set_bank_window(&mut self, w: Option<BankWindow>) {
        self.ext_window = w;
    }

    /// CRTC register `index`, and the index register's current value.
    pub fn crtc(&self, index: u8) -> u8 {
        self.crtc[index as usize]
    }

    pub fn crtc_index(&self) -> u8 {
        self.crtc_index
    }

    /// Marks the picture as changed, for VRAM or registers that live outside the VGA.
    pub fn touch(&mut self) {
        self.changed = true;
    }

    /// I/O base of the CRTC and input status 1: 0x3D0 in color mode, 0x3B0 in mono.
    fn crtc_base(&self) -> u16 {
        if self.misc & 1 != 0 { 0x3d0 } else { 0x3b0 }
    }

    /// None if the port is not ours. `now_ns` drives retrace timing.
    pub fn io_read(&mut self, port: u16, size: u8, now_ns: u64) -> Option<u32> {
        match port {
            0x1ce | 0x1cf if self.dispi => Some(self.vbe_read(port) as u32),
            0x3b0..=0x3df => {
                let mut v = 0;
                for i in 0..size.clamp(1, 4) as u16 {
                    v |= (self.port_read8(port + i, now_ns) as u32) << (8 * i);
                }
                Some(v)
            }
            _ => None,
        }
    }

    /// False if the port is not ours.
    pub fn io_write(&mut self, port: u16, size: u8, value: u32) -> bool {
        match port {
            0x1ce | 0x1cf if self.dispi => self.vbe_write(port, value as u16),
            0x3b0..=0x3df => {
                for i in 0..size.clamp(1, 4) as u16 {
                    self.port_write8(port + i, (value >> (8 * i)) as u8);
                }
            }
            _ => return false,
        }
        self.changed = true;
        true
    }

    fn port_read8(&mut self, port: u16, now_ns: u64) -> u8 {
        let base = self.crtc_base();
        match port {
            0x3c0 => self.attr_index,
            0x3c1 => self.attr.get((self.attr_index & 0x1f) as usize).copied().unwrap_or(0),
            0x3c2 => 0,
            0x3c3 => self.subsys,
            0x3c4 => self.seq_index,
            0x3c5 => self.seq[self.seq_index as usize],
            0x3c6 => self.pel_mask,
            0x3c7 => if self.dac_reading { 3 } else { 0 },
            0x3c8 => self.dac_write,
            0x3c9 => {
                let v = self.dac[self.dac_read as usize * 3 + self.dac_sub as usize];
                self.dac_sub += 1;
                if self.dac_sub == 3 {
                    self.dac_sub = 0;
                    self.dac_read = self.dac_read.wrapping_add(1);
                }
                if self.dac_8bit { v } else { v & 0x3f }
            }
            0x3ca => self.feature,
            0x3cc => self.misc,
            0x3ce => self.gfx_index,
            0x3cf => self.gfx[self.gfx_index as usize],
            p if p == base + 4 => self.crtc_index,
            p if p == base + 5 => self.crtc[self.crtc_index as usize],
            p if p == base + 0xa => {
                self.attr_flip = false;
                self.input_status1(now_ns)
            }
            _ => 0xff,
        }
    }

    fn port_write8(&mut self, port: u16, v: u8) {
        let base = self.crtc_base();
        match port {
            0x3c0 => self.attr_write(v),
            0x3c2 => self.misc = v & !0x10,
            0x3c3 => self.subsys = v & 1,
            0x3c4 => self.seq_index = v & 7,
            0x3c5 => self.seq[self.seq_index as usize] = v & SEQ_MASK[self.seq_index as usize],
            0x3c6 => self.pel_mask = v,
            0x3c7 => {
                self.dac_read = v;
                self.dac_sub = 0;
                self.dac_reading = true;
            }
            0x3c8 => {
                self.dac_write = v;
                self.dac_sub = 0;
                self.dac_reading = false;
            }
            0x3c9 => {
                self.dac_cache[self.dac_sub as usize] = if self.dac_8bit { v } else { v & 0x3f };
                self.dac_sub += 1;
                if self.dac_sub == 3 {
                    let i = self.dac_write as usize * 3;
                    self.dac[i..i + 3].copy_from_slice(&self.dac_cache);
                    self.dac_sub = 0;
                    self.dac_write = self.dac_write.wrapping_add(1);
                }
            }
            0x3ce => self.gfx_index = v & 0x0f,
            0x3cf => self.gfx[self.gfx_index as usize] = v & GFX_MASK[self.gfx_index as usize],
            p if p == base + 4 => self.crtc_index = v,
            p if p == base + 5 => self.crtc_write(v),
            p if p == base + 0xa => self.feature = v & 0x10,
            _ => {}
        }
    }

    fn crtc_write(&mut self, v: u8) {
        let i = self.crtc_index as usize;
        if self.crtc[0x11] & 0x80 != 0 && i <= 7 {
            if i == 7 {
                self.crtc[7] = (self.crtc[7] & !0x10) | (v & 0x10);
            }
            return;
        }
        self.crtc[i] = v;
    }

    fn attr_write(&mut self, v: u8) {
        if !self.attr_flip {
            self.attr_index = v & 0x3f;
        } else {
            let i = (self.attr_index & 0x1f) as usize;
            match i {
                0x00..=0x0f => self.attr[i] = v & 0x3f,
                0x10 => self.attr[i] = v & !0x10,
                0x11 => self.attr[i] = v,
                0x12 => self.attr[i] = v & 0x3f,
                0x13 | 0x14 => self.attr[i] = v & 0x0f,
                _ => {}
            }
        }
        self.attr_flip = !self.attr_flip;
    }
}

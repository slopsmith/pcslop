//! PCI configuration mechanism #1 (ports 0xCF8/0xCFC) and simple
//! configuration-space-only devices.

pub struct ConfigSpace {
    pub data: [u8; 256],
    /// Bits the guest may change.
    pub writable: [u8; 256],
}

impl ConfigSpace {
    pub fn new(vendor: u16, device: u16, class: u32, revision: u8, header_type: u8) -> Self {
        let mut c = ConfigSpace { data: [0; 256], writable: [0; 256] };
        c.set16(0x00, vendor);
        c.set16(0x02, device);
        c.data[0x08] = revision;
        c.data[0x09] = class as u8;
        c.data[0x0a] = (class >> 8) as u8;
        c.data[0x0b] = (class >> 16) as u8;
        c.data[0x0e] = header_type;
        c.writable[0x04] = 0x07;
        c.writable[0x05] = 0x01;
        c.writable[0x0d] = 0xff;
        c.writable[0x3c] = 0xff;
        for i in 0x40..0x100 {
            c.writable[i] = 0xff;
        }
        c
    }

    pub fn set16(&mut self, off: usize, v: u16) {
        self.data[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    pub fn read(&self, off: usize, size: usize) -> u32 {
        let mut v = 0u32;
        for i in 0..size {
            v |= (self.data[(off + i) & 0xff] as u32) << (8 * i);
        }
        v
    }

    pub fn write(&mut self, off: usize, size: usize, v: u32) {
        for i in 0..size {
            let o = (off + i) & 0xff;
            let m = self.writable[o];
            self.data[o] = (self.data[o] & !m) | ((v >> (8 * i)) as u8 & m);
        }
    }
}

/// Identifies a function on bus 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bdf {
    pub dev: u8,
    pub func: u8,
}

pub fn host_bridge() -> ConfigSpace {
    let mut c = ConfigSpace::new(0x8086, 0x1237, 0x06_00_00, 0x02, 0x00);
    // SeaBIOS only takes its QEMU paths when it sees QEMU's subsystem IDs.
    c.set16(0x2c, 0x1af4);
    c.set16(0x2e, 0x1100);
    c
}

pub fn piix3_isa() -> ConfigSpace {
    let mut c = ConfigSpace::new(0x8086, 0x7000, 0x06_01_00, 0x00, 0x80);
    c.set16(0x2c, 0x1af4);
    c.set16(0x2e, 0x1100);
    for r in 0x60..0x64 {
        c.data[r] = 0x80;
    }
    c
}

/// PAM register offsets in the i440FX host bridge (440FX datasheet, section 3.2).
pub const PAM0: usize = 0x59;

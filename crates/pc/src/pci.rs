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

    fn set_mask32(&mut self, off: usize, mask: u32) {
        self.writable[off..off + 4].copy_from_slice(&mask.to_le_bytes());
    }

    /// Declares BAR `n` with a power-of-two `size`. Sizing (writing all ones and
    /// reading back) works through the writable mask.
    pub fn add_bar(&mut self, n: usize, kind: BarKind, size: u32) {
        assert!(size.is_power_of_two());
        let off = 0x10 + 4 * n;
        let (flags, mask) = match kind {
            BarKind::Io => (1, !(size - 1) & 0xffff_fffc),
            BarKind::Mem => (0, !(size - 1) & 0xffff_fff0),
            BarKind::MemPrefetch => (8, !(size - 1) & 0xffff_fff0),
        };
        self.data[off..off + 4].copy_from_slice(&(flags as u32).to_le_bytes());
        self.set_mask32(off, mask);
    }

    pub fn add_rom_bar(&mut self, size: u32) {
        assert!(size.is_power_of_two());
        self.set_mask32(0x30, !(size - 1) | 1);
    }

    fn command(&self) -> u16 {
        self.read(0x04, 2) as u16
    }

    pub fn bus_master(&self) -> bool {
        self.command() & 4 != 0
    }

    /// Base of I/O BAR `n` when I/O decoding is enabled and the BAR is placed.
    pub fn io_bar(&self, n: usize) -> Option<u16> {
        let v = self.read(0x10 + 4 * n, 4);
        let base = (v & 0xffff_fffc) as u16;
        (self.command() & 1 != 0 && v & 1 != 0 && base != 0).then_some(base)
    }

    /// Base of memory BAR `n` when memory decoding is enabled and the BAR is placed.
    pub fn mem_bar(&self, n: usize) -> Option<u32> {
        let v = self.read(0x10 + 4 * n, 4);
        let base = v & 0xffff_fff0;
        (self.command() & 2 != 0 && v & 1 == 0 && base != 0).then_some(base)
    }

    /// Base of the expansion ROM when it is enabled.
    pub fn rom_bar(&self) -> Option<u32> {
        let v = self.read(0x30, 4);
        let base = v & 0xffff_f800;
        (self.command() & 2 != 0 && v & 1 != 0 && base != 0).then_some(base)
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarKind {
    Io,
    Mem,
    MemPrefetch,
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

pub fn piix3_ide() -> ConfigSpace {
    let mut c = ConfigSpace::new(ide::PCI_VENDOR, ide::PCI_DEVICE, ide::PCI_CLASS_CODE, 0x00, 0x00);
    c.add_bar(4, BarKind::Io, 16);
    c.writable[0x04] |= 0x04;
    c
}

pub const INTERRUPT_PIN: usize = 0x3d;

/// PIRQ line (0 = PIRQA) that INTx `pin` (1 = INTA#) of a device in `slot`
/// reaches on the i440FX board. The rotation follows QEMU's
/// `pc_pci_slot_get_pirq` in hw/i386/pc_piix.c (Fabrice Bellard et al.), which
/// SeaBIOS's `piix_pci_slot_get_irq` mirrors when it fills Interrupt Line.
pub fn pirq_for(slot: u8, pin: u8) -> usize {
    (pin as usize + slot as usize + 2) & 3
}

/// PIRQ route control registers in the PIIX3 ISA bridge (82371SB datasheet, 2.2.10).
pub const PIRQRC: usize = 0x60;

/// ISA IRQ a PIRQRC register value routes to. Bit 7 disables routing, and
/// IRQ 0-2, 8 and 13 are reserved.
pub fn pirq_route(reg: u8) -> Option<u8> {
    let irq = reg & 0x0f;
    (reg & 0x80 == 0 && !matches!(irq, 0 | 1 | 2 | 8 | 13)).then_some(irq)
}

/// PIIX3 USB function (82371SB datasheet, section 2.4).
pub fn piix3_uhci() -> ConfigSpace {
    let mut c = ConfigSpace::new(usb::PCI_VENDOR, usb::PCI_DEVICE, usb::PCI_CLASS_CODE, 0x01, 0x00);
    c.add_bar(4, BarKind::Io, 32);
    c.writable[0x04] |= 0x04;
    c.data[INTERRUPT_PIN] = 4;
    // SBRN: USB 1.0.
    c.data[0x60] = 0x10;
    // LEGSUP: USB interrupts go to PIRQD. Like QEMU, the rest of the legacy
    // keyboard emulation is not modelled.
    c.set16(0xc0, 0x2000);
    c
}

/// PAM register offsets in the i440FX host bridge (440FX datasheet, section 3.2).
pub const PAM0: usize = 0x59;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_sizing_and_decode() {
        let mut c = ConfigSpace::new(0x1234, 0x1111, 0x03_00_00, 0, 0);
        c.add_bar(0, BarKind::MemPrefetch, 16 << 20);
        c.add_bar(4, BarKind::Io, 16);
        c.write(0x10, 4, 0xffff_ffff);
        assert_eq!(c.read(0x10, 4), 0xff00_0008);
        c.write(0x20, 4, 0xffff_ffff);
        assert_eq!(c.read(0x20, 4), 0xffff_fff1);
        c.write(0x10, 4, 0xe000_0000);
        c.write(0x20, 4, 0xc000);
        assert_eq!(c.mem_bar(0), None);
        c.write(0x04, 2, 3);
        assert_eq!(c.mem_bar(0), Some(0xe000_0000));
        assert_eq!(c.io_bar(4), Some(0xc000));
    }

    #[test]
    fn pirq_swizzle_matches_seabios() {
        // SeaBIOS: pci_irqs[(pin - 1 + slot - 1) & 3]
        for slot in 0..32u8 {
            for pin in 1..=4u8 {
                assert_eq!(pirq_for(slot, pin), (pin as usize - 1 + slot as usize).wrapping_sub(1) & 3);
            }
        }
        assert_eq!(pirq_for(1, 4), 3);
    }

    #[test]
    fn uhci_config_space() {
        let mut c = piix3_uhci();
        assert_eq!(c.read(0, 4), 0x7020_8086);
        assert_eq!(c.read(0x08, 4) >> 8, 0x0c_03_00);
        c.write(0x20, 4, 0xffff_ffff);
        assert_eq!(c.read(0x20, 4), 0xffff_ffe1);
        assert_eq!(c.data[INTERRUPT_PIN], 4);
        assert_eq!(pirq_for(1, c.data[INTERRUPT_PIN]), 3);
        c.write(0x04, 2, 0xffff);
        assert!(c.bus_master());
    }

    #[test]
    fn pirq_route_register() {
        assert_eq!(pirq_route(0x80), None);
        assert_eq!(pirq_route(0x8b), None);
        assert_eq!(pirq_route(0x0b), Some(11));
        assert_eq!(pirq_route(0x0a), Some(10));
        assert_eq!(pirq_route(0x02), None);
        assert_eq!(pirq_route(0x0d), None);
    }
}

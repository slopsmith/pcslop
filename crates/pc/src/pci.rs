//! PCI configuration mechanism #1 (ports 0xCF8/0xCFC) and simple
//! configuration-space-only devices.

#[derive(snapshot::Snap)]
pub struct ConfigSpace {
    pub data: [u8; 256],
    /// Bits the guest may change.
    #[snap(skip)]
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

    pub fn set_mask32(&mut self, off: usize, mask: u32) {
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

    pub fn set32(&mut self, off: usize, v: u32) {
        self.data[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Makes `[off, off + len)` read-only.
    pub fn lock(&mut self, off: usize, len: usize) {
        self.writable[off..off + len].fill(0);
    }

    /// Links a capability with `id` at `off` into the capability list, and
    /// returns `off` back.
    pub fn add_capability(&mut self, off: usize, id: u8) -> usize {
        self.data[off] = id;
        self.data[off + 1] = self.data[CAP_PTR];
        self.data[CAP_PTR] = off as u8;
        self.data[STATUS] |= STATUS_CAP_LIST;
        off
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

const STATUS: usize = 0x06;
const STATUS_CAP_LIST: u8 = 0x10;
const CAP_PTR: usize = 0x34;
pub const CAP_PM: u8 = 0x01;
pub const CAP_AGP: u8 = 0x02;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarKind {
    Io,
    Mem,
    MemPrefetch,
}

/// Identifies a function. Bus 1 is the AGP bus behind the 440BX's bridge,
/// whatever number software gives it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bdf {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl Bdf {
    pub const fn new(bus: u8, dev: u8, func: u8) -> Self {
        Bdf { bus, dev, func }
    }
}

pub fn host_bridge() -> ConfigSpace {
    let mut c = ConfigSpace::new(0x8086, 0x1237, 0x06_00_00, 0x02, 0x00);
    // SeaBIOS only takes its QEMU paths when it sees QEMU's subsystem IDs.
    c.set16(0x2c, 0x1af4);
    c.set16(0x2e, 0x1100);
    c
}

fn qemu_subsystem(c: &mut ConfigSpace) {
    c.set16(0x2c, 0x1af4);
    c.set16(0x2e, 0x1100);
}

/// 82443BX host bridge registers (440BX datasheet, section 3.3).
pub mod bx {
    pub const APBASE: usize = 0x10;
    pub const NBXCFG: usize = 0x50;
    /// NBXCFG: Aperture Access Global Enable.
    pub const APERTURE_ENABLE: u32 = 1 << 9;
    pub const DRB: usize = 0x60;
    pub const ACAPID: usize = 0xa0;
    pub const AGPSTAT: usize = 0xa4;
    pub const AGPCMD: usize = 0xa8;
    pub const AGPCTRL: usize = 0xb0;
    /// AGPCTRL: GTLB Enable; clearing it flushes the GTLB.
    pub const GTLB_ENABLE: u32 = 1 << 7;
    pub const APSIZE: usize = 0xb4;
    pub const ATTBASE: usize = 0xb8;
    /// APSIZE as a 440BX board's firmware sets it: a 64 MiB aperture.
    pub const APSIZE_64M: u8 = 0x30;
}

/// The 440BX host bridge, 8086:7190, with AGP enabled (PMCR.AGP_DIS = 0).
pub fn i440bx_host(ram_bytes: u32) -> ConfigSpace {
    let mut c = ConfigSpace::new(0x8086, 0x7190, 0x06_00_00, 0x03, 0x00);
    // SeaBIOS only takes its QEMU paths when it sees QEMU's subsystem IDs.
    qemu_subsystem(&mut c);
    c.set16(0x04, 0x0006);
    c.data[0x06] = 0x10;
    c.data[0x07] = 0x02;
    c.lock(0x2c, 4);
    c.add_bar(0, BarKind::MemPrefetch, 4 << 20);
    let cap = c.add_capability(bx::ACAPID, CAP_AGP);
    c.data[cap + 2] = 0x10;
    c.set32(bx::AGPSTAT, 0x1f00_0203);
    c.lock(bx::ACAPID, 8);
    c.set_mask32(bx::AGPCMD, 0x0000_0303);
    c.set_mask32(bx::AGPCTRL, 0x0000_a080);
    c.writable[bx::APSIZE] = 0x3f;
    c.lock(bx::APSIZE + 1, 3);
    c.set_mask32(bx::ATTBASE, 0xffff_f000);
    // Each DRAM row boundary counts 8 MiB units; all RAM sits in row 0.
    let rows = (ram_bytes >> 23).min(0xff) as u8;
    c.data[bx::DRB..bx::DRB + 8].fill(rows);
    c.data[bx::APSIZE] = bx::APSIZE_64M;
    i440bx_update(&mut c);
    c
}

/// Applies APSIZE to the bits of APBASE that software may write.
pub fn i440bx_update(c: &mut ConfigSpace) {
    let mask = 0xf000_0000 | ((c.data[bx::APSIZE] as u32 & 0x3f) << 22);
    c.set_mask32(bx::APBASE, mask);
    let v = c.read(bx::APBASE, 4);
    c.set32(bx::APBASE, (v & mask) | 8);
}

/// Bus address and size of the graphics aperture while it decodes.
pub fn i440bx_aperture(c: &ConfigSpace) -> Option<(u32, u32)> {
    let size = 0x1000_0000u32 >> (c.data[bx::APSIZE] & 0x3f).count_ones();
    let enabled = c.read(bx::NBXCFG, 4) & bx::APERTURE_ENABLE != 0;
    c.mem_bar(0).filter(|_| enabled).map(|base| (base, size))
}

/// The 440BX's PCI-to-PCI bridge to AGP, 8086:7191 (datasheet, section 3.4).
pub fn i440bx_agp_bridge() -> ConfigSpace {
    let mut c = ConfigSpace::new(0x8086, 0x7191, 0x06_04_00, 0x03, 0x01);
    c.writable[0x04] = 0x1f;
    c.data[0x06] = 0x20;
    c.data[0x07] = 0x02;
    c.lock(0x3c, 1);
    c.lock(0x40, 0xc0);
    c.writable[0x19] = 0xff;
    c.writable[0x1a] = 0xff;
    c.writable[0x1b] = 0xff;
    c.data[0x1c] = 0xf0;
    c.writable[0x1c] = 0xf0;
    c.writable[0x1d] = 0xf0;
    c.set16(0x1e, 0x02a0);
    for off in [0x20, 0x24] {
        c.set16(off, 0xfff0);
        c.set_mask32(off, 0xfff0_fff0);
    }
    c.data[BRIDGE_CONTROL] = 0x80;
    c.writable[BRIDGE_CONTROL] = 0x0f;
    c
}

pub const SECONDARY_BUS: usize = 0x19;
pub const BRIDGE_CONTROL: usize = 0x3e;
/// Bridge control: forward the legacy VGA ranges to the secondary bus.
pub const BRIDGE_VGA: u8 = 0x08;

/// The Radeon 9700's function: VRAM, I/O and MMIO BARs, the expansion ROM,
/// and power management and AGP 3.0 capabilities at 0x50 and 0x58, where
/// R300 cards have them.
pub fn radeon(rom_len: usize) -> ConfigSpace {
    let mut c = ConfigSpace::new(radeon::PCI_VENDOR, radeon::PCI_DEVICE, radeon::PCI_CLASS, 0x00, 0x00);
    c.set16(0x2c, 0x1002);
    c.set16(0x2e, 0x0002);
    c.writable[0x04] |= 0x04;
    c.data[INTERRUPT_PIN] = 1;
    c.add_bar(0, BarKind::MemPrefetch, radeon::VRAM_SIZE as u32);
    c.add_bar(1, BarKind::Io, radeon::IO_SIZE);
    c.add_bar(2, BarKind::Mem, radeon::MMIO_SIZE);
    c.add_rom_bar((rom_len as u32).next_power_of_two().max(0x2_0000));
    c.lock(0x40, 0xc0);
    let pm = c.add_capability(0x50, CAP_PM);
    c.set16(pm + 2, 0x0602);
    c.writable[pm + 4] = 0x03;
    let agp = c.add_capability(RADEON_AGP, CAP_AGP);
    c.data[agp + 2] = 0x30;
    // AGP 2.0 signalling on a 440BX: 1x, 2x and 4x, fast writes, sideband.
    c.set32(agp + 4, 0x1f00_0217);
    c.set_mask32(agp + 8, 0x0000_0317);
    c
}

/// Offset of the Radeon's AGP capability.
pub const RADEON_AGP: usize = 0x58;

/// AGP status and command bits (AGP 2.0, section 6.1).
pub mod agp {
    pub const RATE: u32 = 0x7;
    pub const FW: u32 = 1 << 4;
    pub const ENABLE: u32 = 1 << 8;
    pub const SBA: u32 = 1 << 9;
    pub const RQ: u32 = 0xff00_0000;
}

/// Enables AGP on the 440BX and the Radeon at their fastest common rate, with
/// sideband addressing and fast writes where both support them, as a 440BX
/// board's firmware does after it enumerates the AGP bus. Vista's agp440 only
/// programs AGP when the bridge comes back to D0, so its drivers rely on this.
pub fn enable_agp(host: &mut ConfigSpace, card: &mut ConfigSpace) {
    let target = host.read(bx::AGPSTAT, 4);
    let master = card.read(RADEON_AGP + 4, 4);
    let common = target & master;
    let rate = match common & agp::RATE {
        r if r & 4 != 0 => 4,
        r if r & 2 != 0 => 2,
        _ => 1,
    };
    let cmd = (target & agp::RQ) | (common & (agp::SBA | agp::FW)) | agp::ENABLE | rate;
    host.write(bx::AGPCMD, 4, cmd);
    card.write(RADEON_AGP + 8, 4, cmd);
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

    fn capabilities(c: &ConfigSpace) -> Vec<(usize, u8)> {
        let mut caps = Vec::new();
        let mut p = c.data[CAP_PTR] as usize;
        while p != 0 {
            caps.push((p, c.data[p]));
            p = c.data[p + 1] as usize;
        }
        caps
    }

    #[test]
    fn i440bx_aperture_size_follows_apsize() {
        let mut c = i440bx_host(512 << 20);
        assert_eq!(c.read(0, 4), 0x7190_8086);
        assert_eq!(capabilities(&c), vec![(0xa0, CAP_AGP)]);
        assert_eq!(c.read(bx::ACAPID, 4), 0x0010_0002);
        c.write(bx::APBASE, 4, 0xffff_ffff);
        assert_eq!(c.read(bx::APBASE, 4), 0xfc00_0008);
        c.write(bx::APSIZE, 1, 0x3f);
        i440bx_update(&mut c);
        c.write(bx::APBASE, 4, 0xffff_ffff);
        assert_eq!(c.read(bx::APBASE, 4), 0xffc0_0008);
        c.write(bx::AGPSTAT, 4, 0);
        assert_eq!(c.read(bx::AGPSTAT, 4), 0x1f00_0203);
        assert_eq!(c.data[bx::DRB + 7], 64);
    }

    #[test]
    fn firmware_agp_setup_enables_both_ends_at_their_fastest_common_rate() {
        let mut host = i440bx_host(256 << 20);
        let mut card = radeon(53 << 10);
        enable_agp(&mut host, &mut card);
        let cmd = agp::ENABLE | agp::SBA | 2;
        assert_eq!(host.read(bx::AGPCMD, 4), cmd);
        assert_eq!(card.read(RADEON_AGP + 8, 4), cmd, "the card's command must match the bridge's");
    }

    #[test]
    fn i440bx_aperture_decodes_once_enabled() {
        let mut c = i440bx_host(256 << 20);
        c.write(bx::APBASE, 4, 0xe000_0000);
        assert_eq!(i440bx_aperture(&c), None);
        c.write(bx::NBXCFG, 4, bx::APERTURE_ENABLE);
        assert_eq!(i440bx_aperture(&c), Some((0xe000_0000, 64 << 20)));
    }

    #[test]
    fn agp_bridge_is_a_pci_bridge() {
        let mut c = i440bx_agp_bridge();
        assert_eq!(c.read(0, 4), 0x7191_8086);
        assert_eq!(c.read(0x08, 4) >> 8, 0x06_04_00);
        assert_eq!(c.data[0x0e], 1);
        c.write(0x18, 4, 0xffff_ffff);
        assert_eq!(c.read(0x18, 4), 0xffff_ff00);
        c.write(0x20, 4, 0xffff_ffff);
        assert_eq!(c.read(0x20, 4), 0xfff0_fff0);
        c.write(BRIDGE_CONTROL, 1, 0xff);
        assert_eq!(c.data[BRIDGE_CONTROL], 0x8f);
    }

    #[test]
    fn radeon_bars_rom_and_capabilities() {
        let mut c = radeon(53 << 10);
        assert_eq!(c.read(0, 4), 0x4e44_1002);
        assert_eq!(c.read(0x08, 4) >> 8, 0x03_00_00);
        for (bar, size) in [(0x10, 0xf800_0008), (0x14, 0xffff_ff01), (0x18, 0xffff_0000), (0x30, 0xfffe_0001)] {
            c.write(bar, 4, 0xffff_ffff);
            assert_eq!(c.read(bar, 4), size, "BAR at {bar:#x}");
        }
        assert_eq!(capabilities(&c), vec![(0x58, CAP_AGP), (0x50, CAP_PM)]);
        assert_eq!(c.data[0x5a], 0x30);
        c.write(0x60, 4, 0xffff_ffff);
        assert_eq!(c.read(0x60, 4), 0x317);
        assert_eq!(c.data[INTERRUPT_PIN], 1);
    }
}

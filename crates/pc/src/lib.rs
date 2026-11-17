//! An i440FX + PIIX3 PC: memory map, chipset registers and legacy devices.

pub mod acpi;
pub mod apic;
pub mod battery;
pub mod fdc;
pub mod fw_cfg;
pub mod i8042;
pub mod ioapic;
pub mod pci;
pub mod pic;
pub mod pm;
pub mod pit;
pub mod ram;
pub mod rtc;
mod state;
pub mod uart;

pub use state::Machine;

use std::io::Write;
use std::time::Instant;

use pci::{Bdf, ConfigSpace, PAM0};
use x86::bus::Bus;

pub struct Config {
    pub ram_bytes: u32,
    pub bios: Vec<u8>,
    pub vgabios: Option<Vec<u8>>,
    /// Adds a local APIC and an I/O APIC.
    pub apic: bool,
    pub chipset: Chipset,
    pub gpu: Gpu,
    pub power: battery::Power,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Chipset {
    /// 440FX: the PIIX3 at 00:01, the display at 00:02.
    #[default]
    I440fx,
    /// 440BX: its AGP bridge at 00:01 leads to bus 1, and the PIIX3 moves to 00:07.
    I440bx,
}

#[derive(Default)]
pub enum Gpu {
    /// QEMU's stdvga at 00:02, running `vgabios`.
    #[default]
    StdVga,
    /// An ATI Radeon 9700 on the AGP bus, running the card's own ROM, with
    /// its 3D engine drawing through Metal when `metal` is set and the Mac
    /// supports it, and on the CPU otherwise.
    Radeon { rom: Vec<u8>, trace: Option<Box<dyn Write + Send>>, metal: bool },
}

pub enum Display {
    Std(Box<vga::Vga>),
    Radeon(Box<radeon::Radeon>),
}

impl Display {
    pub fn vga(&mut self) -> &mut vga::Vga {
        match self {
            Display::Std(v) => v,
            Display::Radeon(r) => r.vga(),
        }
    }
}

/// Physical pages holding translated code, and the ones written since the JIT last looked.
#[derive(Default)]
struct CodeWatch {
    bits: Vec<u64>,
    written: Vec<u32>,
}

impl CodeWatch {
    #[inline]
    fn note_write(&mut self, addr: u32) {
        let page = (addr >> 12) as usize;
        if let Some(word) = self.bits.get_mut(page >> 6) {
            let bit = 1u64 << (page & 63);
            if *word & bit != 0 {
                *word &= !bit;
                self.written.push(page as u32);
            }
        }
    }
}

pub struct Board {
    ram: ram::Ram,
    code_watch: CodeWatch,
    bios: Vec<u8>,
    a20: bool,
    pci_addr: u32,
    pci: Vec<(Bdf, ConfigSpace)>,
    /// ISA IRQs currently held high by PCI interrupt lines and the SCI.
    pci_irqs: u16,
    sci: bool,
    pub pic: pic::Pic,
    pub lapic: Option<apic::LocalApic>,
    pub ioapic: ioapic::IoApic,
    pub pit: pit::Pit,
    fdc: fdc::Fdc,
    /// Timer interrupts owed to the guest: Windows counts them to keep time,
    /// so ticks that arrive while one is still pending are delivered later.
    irq0_backlog: u32,
    pub rtc: rtc::Rtc,
    /// COM1 at 0x3F8, IRQ 4.
    pub com1: uart::Uart,
    pub fw_cfg: fw_cfg::FwCfg,
    pub kbc: i8042::I8042,
    pub display: Display,
    chipset: Chipset,
    pub ide: ide::Ide,
    pub uhci: usb::Uhci,
    pub pm: pm::Pm,
    /// The AC'97 controller and its index in `pci`, when the board has one.
    ac97: Option<(usize, ac97::Ac97)>,
    /// The network card and its index in `pci`, if one is plugged in.
    nic: Option<(usize, net::Rtl8139)>,
    power: battery::PowerDevice,
    vgabios: Vec<u8>,
    pub debugcon: Box<dyn Write + Send>,
    start: Instant,
    /// Board time at `start`, which a restored snapshot carries on from.
    clock_base: u64,
    pub reset_requested: bool,
    pub post_code: u8,
    /// The ISO in the CD-ROM drive.
    cdrom: Option<String>,
}

const LEGACY_START: u32 = 0xa_0000;
const HOST: usize = 0;
const PIIX3_ISA: usize = 1;
const IDE: usize = 2;
const DISPLAY: usize = 3;
const UHCI: usize = 4;
const PM: usize = 5;
/// Only on the 440BX.
const AGP_BRIDGE: usize = 6;
const UHCI_PORTS: u16 = 32;
const NIC_SLOT: u8 = 3;
const LEGACY_END: u32 = 0x10_0000;
/// The I/O APIC input that ISA IRQ 0 drives. QEMU wires it the same way
/// (hw/intc/ioapic.c), and SeaBIOS's MP table says so with etc/irq0-override.
const IRQ0_PIN: u8 = 2;
/// I/O APIC inputs wired straight to PIRQA..PIRQD.
const PIRQ_PIN: u8 = 16;
/// SMBIOS strings by (structure type, field offset within the structure).
const SMBIOS: &[(u8, u16, &str)] = &[
    (0, 4, "86slopper"),
    (0, 5, "SeaBIOS on Hypervisor.framework"),
    (0, 8, "11/17/2026"),
    (1, 4, "86slopper"),
    (1, 5, "Slop Station 86"),
    (1, 6, "Apple Silicon Edition"),
    (1, 26, "Totally Real PC"),
    (3, 4, "86slopper"),
    (4, 7, "86slopper"),
];

impl Board {
    pub fn new(cfg: Config) -> Self {
        assert!(cfg.bios.len().is_power_of_two() && cfg.bios.len() >= 0x2_0000, "BIOS must be a power of two >= 128 KiB");
        let ram_bytes = cfg.ram_bytes;
        let piix = match cfg.chipset {
            Chipset::I440fx => 1,
            Chipset::I440bx => 7,
        };
        let host = match cfg.chipset {
            Chipset::I440fx => pci::host_bridge(),
            Chipset::I440bx => pci::i440bx_host(ram_bytes),
        };
        let mut b = Board {
            ram: ram::Ram::new(ram_bytes as usize),
            code_watch: CodeWatch { bits: vec![0; (ram_bytes as usize >> 12).div_ceil(64)], written: Vec::new() },
            bios: cfg.bios,
            a20: true,
            pci_addr: 0,
            pci_irqs: 0,
            sci: false,
            pci: vec![
                (Bdf::new(0, 0, 0), host),
                (Bdf::new(0, piix, 0), pci::piix3_isa()),
                (Bdf::new(0, piix, 1), pci::piix3_ide()),
            ],
            pic: pic::Pic::default(),
            lapic: cfg.apic.then(apic::LocalApic::default),
            ioapic: ioapic::IoApic::default(),
            pit: pit::Pit::default(),
            fdc: fdc::Fdc::default(),
            irq0_backlog: 0,
            rtc: rtc::Rtc::default(),
            com1: uart::Uart::new(Box::new(std::io::sink())),
            fw_cfg: fw_cfg::FwCfg::new(),
            kbc: i8042::I8042::default(),
            display: Display::Std(Box::new(vga::Vga::new())),
            chipset: cfg.chipset,
            ide: ide::Ide::new(),
            uhci: usb::Uhci::new(),
            pm: pm::Pm::default(),
            ac97: None,
            nic: None,
            power: battery::PowerDevice::new(cfg.power),
            vgabios: Vec::new(),
            debugcon: Box::new(std::io::stdout()),
            start: Instant::now(),
            clock_base: 0,
            reset_requested: false,
            post_code: 0,
            cdrom: None,
        };
        match cfg.gpu {
            Gpu::StdVga => {
                let mut display = pci::ConfigSpace::new(vga::PCI_VENDOR, vga::PCI_DEVICE, 0x03_00_00, 0x02, 0x00);
                display.set16(0x2c, 0x1af4);
                display.set16(0x2e, 0x1100);
                display.add_bar(0, pci::BarKind::MemPrefetch, vga::VRAM_SIZE as u32);
                if let Some(rom) = cfg.vgabios {
                    let size = (rom.len() as u32).next_power_of_two().max(0x800);
                    display.add_rom_bar(size);
                    b.vgabios = rom;
                    b.vgabios.resize(size as usize, 0xff);
                }
                b.pci.push((Bdf::new(0, 2, 0), display));
            }
            Gpu::Radeon { rom, trace, metal } => {
                assert_eq!(cfg.chipset, Chipset::I440bx, "the Radeon sits on the 440BX's AGP bus");
                let display = pci::radeon(rom.len());
                let mut card = radeon::Radeon::new(rom);
                if let Some(t) = trace {
                    card.set_trace(t);
                }
                if metal && !card.use_metal() {
                    eprintln!("radeon: Metal is not available; the 3D engine draws on the CPU");
                }
                b.display = Display::Radeon(Box::new(card));
                b.pci.push((Bdf::new(1, 0, 0), display));
            }
        }
        b.pci.push((Bdf::new(0, piix, 2), pci::piix3_uhci()));
        b.pci.push((Bdf::new(0, piix, 3), pm::config_space()));
        if cfg.chipset == Chipset::I440bx {
            b.pci.push((Bdf::new(0, 1, 0), pci::i440bx_agp_bridge()));
        }
        b.sync_display_bars();
        b.uhci.attach(0, Box::new(usb::Tablet::new()));
        b.ide.attach_cdrom(1, 0, None);
        b.setup_cmos(ram_bytes);
        b.fw_cfg.set(fw_cfg::key::RAM_SIZE, (ram_bytes as u64).to_le_bytes().to_vec());
        let e820 = fw_cfg::e820(&[(0, ram_bytes as u64, 1)]);
        b.fw_cfg.add_file("etc/e820", e820);
        b.fw_cfg.set(fw_cfg::key::SMBIOS_ENTRIES, fw_cfg::smbios_fields(SMBIOS));
        let acpi = acpi::build(ram_bytes, cfg.apic, acpi::Layout { piix, agp: cfg.chipset == Chipset::I440bx });
        b.fw_cfg.add_file(acpi::TABLES_FILE, acpi.tables);
        b.fw_cfg.add_file(acpi::RSDP_FILE, acpi.rsdp);
        b.fw_cfg.add_file(acpi::LOADER_FILE, acpi.loader);
        if cfg.apic {
            b.fw_cfg.add_file("etc/irq0-override", 1u32.to_le_bytes().to_vec());
        }
        b
    }

    /// Adds an AC'97 audio controller at 00:04.0 that plays into `out`.
    pub fn add_ac97(&mut self, out: Box<dyn ac97::AudioOut>) {
        let mut c = ConfigSpace::new(ac97::PCI_VENDOR, ac97::PCI_DEVICE, ac97::PCI_CLASS_CODE, 0x01, 0x00);
        c.set16(0x2c, 0x1af4);
        c.set16(0x2e, 0x1100);
        c.add_bar(0, pci::BarKind::Io, ac97::NAM_LEN as u32);
        c.add_bar(1, pci::BarKind::Io, ac97::NABM_LEN as u32);
        c.writable[0x04] |= 0x04;
        c.data[pci::INTERRUPT_PIN] = 1;
        self.pci.push((Bdf::new(0, 4, 0), c));
        self.ac97 = Some((self.pci.len() - 1, ac97::Ac97::new(out)));
    }

    /// The sound card's state, for debugging.
    pub fn describe_audio(&self) -> Option<String> {
        self.ac97.as_ref().map(|(_, a)| a.describe())
    }

    pub fn describe_gpu(&self) -> Option<String> {
        match &self.display {
            Display::Radeon(card) => Some(card.describe()),
            Display::Std(_) => None,
        }
    }

    /// I/O BAR `bar` of the AC'97 controller, if the guest has it decoded.
    fn ac97_bar(&self, bar: usize) -> Option<u16> {
        self.ac97.as_ref().and_then(|(i, _)| self.pci[*i].1.io_bar(bar))
    }

    /// Plugs an RTL8139 into slot 3, cabled to `backend`. Call it before the
    /// guest starts, so the firmware sees the card.
    pub fn attach_nic(&mut self, mac: [u8; 6], backend: Box<dyn net::Backend>) {
        use net::rtl8139;
        let mut c = ConfigSpace::new(rtl8139::PCI_VENDOR, rtl8139::PCI_DEVICE, rtl8139::PCI_CLASS_CODE, rtl8139::PCI_REVISION, 0);
        c.set16(0x2c, 0x1af4);
        c.set16(0x2e, 0x1100);
        c.add_bar(0, pci::BarKind::Io, rtl8139::REGS_SIZE);
        c.add_bar(1, pci::BarKind::Mem, rtl8139::REGS_SIZE);
        c.data[pci::INTERRUPT_PIN] = 1;
        // Linux's 8139cp gives up on a card whose cache line size it cannot set.
        c.writable[0x0c] = 0xff;
        self.pci.push((Bdf::new(0, NIC_SLOT, 0), c));
        self.nic = Some((self.pci.len() - 1, net::Rtl8139::new(mac, backend)));
    }

    /// Register and frame counts of the network card, if there is one.
    pub fn nic_stats(&self) -> Option<net::rtl8139::Stats> {
        self.nic.as_ref().map(|(_, n)| n.stats)
    }

    fn nic_port(&self, port: u16) -> Option<u8> {
        let (i, _) = self.nic.as_ref()?;
        let off = port.wrapping_sub(self.pci[*i].1.io_bar(0)?);
        (u32::from(off) < net::rtl8139::REGS_SIZE).then_some(off as u8)
    }

    fn nic_mmio(&self, addr: u32) -> Option<u8> {
        let (i, _) = self.nic.as_ref()?;
        let off = addr.wrapping_sub(self.pci[*i].1.mem_bar(1)?);
        (off < net::rtl8139::REGS_SIZE).then_some(off as u8)
    }

    fn nic_read(&mut self, off: u8, size: u8) -> u32 {
        let now = self.now_ns();
        self.nic.as_mut().map_or(0, |(_, n)| n.read(off, size, now))
    }

    fn nic_write(&mut self, off: u8, size: u8, v: u32) {
        let now = self.now_ns();
        if let Some((i, nic)) = self.nic.as_mut() {
            if self.pci[*i].1.bus_master() {
                nic.write(off, size, v, now, &mut RamDma(&mut self.ram[..], &mut self.code_watch));
            } else {
                nic.write(off, size, v, now, &mut NoDma);
            }
        }
        self.sync_pci_irqs();
    }

    fn setup_cmos(&mut self, ram: u32) {
        let r = &mut self.rtc.ram;
        let ext_kb = ((ram.saturating_sub(0x10_0000)) / 1024).min(0xfc00);
        r[0x15] = 0x80;
        r[0x16] = 0x02;
        r[0x17] = ext_kb as u8;
        r[0x18] = (ext_kb >> 8) as u8;
        r[0x30] = ext_kb as u8;
        r[0x31] = (ext_kb >> 8) as u8;
        let above16 = (ram.saturating_sub(0x100_0000) / 0x1_0000).min(0xffff);
        r[0x34] = above16 as u8;
        r[0x35] = (above16 >> 8) as u8;
        r[0x3d] = 0x02 | 0x03 << 4;
        r[0x38] = 0x01;
        r[0x5f] = 0;
    }

    /// Host address and size of guest RAM, which starts at guest physical 0.
    pub fn ram_region(&mut self) -> (*mut u8, usize) {
        (self.ram.as_mut_ptr(), self.ram.len())
    }

    pub fn now_ns(&self) -> u64 {
        self.clock_base + self.start.elapsed().as_nanos() as u64
    }

    /// Advances timers and raises their interrupts.
    pub fn tick(&mut self) {
        let now = self.now_ns();
        self.irq0_backlog = (self.irq0_backlog + self.pit.poll_irq0(now)).min(pit::MAX_BACKLOG);
        if self.irq0_backlog > 0 && !self.irq0_waiting() {
            self.irq0_backlog -= 1;
            self.pulse_irq(0);
        }
        if self.rtc.poll(now) {
            self.pulse_irq(8);
        }
        if let Some(l) = self.lapic.as_mut() {
            l.poll(now);
        }
        if self.pci[UHCI].1.bus_master() {
            self.uhci.run(now, &mut RamDma(&mut self.ram[..], &mut self.code_watch));
        }
        if let Some((i, a)) = self.ac97.as_mut()
            && self.pci[*i].1.bus_master()
        {
            a.run(now, &mut RamDma(&mut self.ram[..], &mut self.code_watch));
        }
        if let Some((i, nic)) = self.nic.as_mut() {
            if self.pci[*i].1.bus_master() {
                nic.poll(now, &mut RamDma(&mut self.ram[..], &mut self.code_watch));
            } else {
                nic.poll(now, &mut NoDma);
            }
        }
        if let Display::Radeon(r) = &mut self.display {
            r.set_time(now);
            if r.fences_pending() {
                self.radeon_write(|r, bus| r.poll_fences(bus));
            }
        }
        self.sync_sci();
    }

    fn sync_sci(&mut self) {
        let now = self.now_ns();
        self.sci = self.pm.sci(now);
        self.sync_pci_irqs();
    }

    /// Whether the guest has asked to enter the soft-off state.
    pub fn power_off_requested(&self) -> bool {
        self.pm.power_off_requested
    }

    /// Presses the power button. The DSDT turns the GPE into a notification.
    pub fn power_button(&mut self) {
        self.pm.raise_gpe(pm::GPE_POWER_BUTTON);
        self.sync_sci();
    }

    /// Updates the battery and AC adapter, raising a GPE if the guest should look again.
    pub fn set_power(&mut self, power: battery::Power) {
        if self.power.set(power) {
            self.pm.raise_gpe(pm::GPE_BATTERY);
            self.sync_sci();
        }
    }

    pub fn power(&self) -> battery::Power {
        self.power.power()
    }

    /// Timer interrupts not yet delivered; it grows when the guest falls behind.
    pub fn timer_backlog(&self) -> u32 {
        self.irq0_backlog
    }

    /// Nanoseconds until a timer will next need attention.
    pub fn next_event_ns(&self) -> u64 {
        let now = self.now_ns();
        if self.irq0_backlog > 0 && !self.irq0_waiting() {
            return 0;
        }
        let usb = self.uhci.next_frame_ns().map(|t| t.saturating_sub(now));
        let lapic = self.lapic.as_ref().and_then(|l| l.next_event_ns(now));
        let nic = self.nic.as_ref().and_then(|(_, n)| n.next_event_ns()).map(|t| t.saturating_sub(now));
        let gpu = match &self.display {
            Display::Radeon(r) => r.next_irq_ns().map(|t| t.saturating_sub(now)),
            Display::Std(_) => None,
        };
        let others = usb.unwrap_or(u64::MAX).min(lapic.unwrap_or(u64::MAX)).min(nic.unwrap_or(u64::MAX)).min(gpu.unwrap_or(u64::MAX));
        self.pit.next_irq0_ns(now).unwrap_or(1_000_000).min(others).min(1_000_000)
    }

    /// True while the last timer interrupt still waits to be taken, on
    /// whichever interrupt controller the guest lets it through.
    fn irq0_waiting(&self) -> bool {
        let pic_reaches_cpu = self.lapic.as_ref().is_none_or(|l| l.accepts_pic());
        let pic = pic_reaches_cpu && self.pic.requested(0) && self.pic.registers()[0].2 & 1 == 0;
        let e = self.ioapic.entry(IRQ0_PIN as usize);
        let ioapic = e & ioapic::redir::MASKED == 0 && self.lapic.as_ref().is_some_and(|l| l.requested(e as u8));
        pic || ioapic
    }

    /// Drives ISA interrupt line `irq` into the 8259s and the I/O APIC.
    pub fn set_irq(&mut self, irq: u8, level: bool) {
        self.pic.set_irq(irq, level);
        if self.lapic.is_some() {
            self.ioapic.set_irq(if irq == 0 { IRQ0_PIN } else { irq }, level);
            self.service_ioapic();
        }
    }

    fn pulse_irq(&mut self, irq: u8) {
        self.set_irq(irq, true);
        self.set_irq(irq, false);
    }

    fn service_ioapic(&mut self) {
        if let Some(l) = self.lapic.as_mut() {
            self.ioapic.service(|m| l.accept(m));
        }
    }

    /// The APIC register at `addr`, if one of the APICs decodes it.
    fn apic_read(&mut self, addr: u32) -> Option<u32> {
        if self.lapic.as_ref().is_some_and(|l| l.claims(addr)) {
            let now = self.now_ns();
            let l = self.lapic.as_mut().unwrap();
            return Some(l.read(addr - l.base(), now));
        }
        if self.lapic.is_some() && addr.wrapping_sub(ioapic::BASE) < ioapic::SIZE {
            return Some(self.ioapic.read(addr - ioapic::BASE));
        }
        None
    }

    /// Writes an APIC register. Only aligned 32-bit writes do anything,
    /// as in QEMU; returns false if no APIC decodes `addr`.
    fn apic_write(&mut self, addr: u32, v: u32, size: u8) -> bool {
        let aligned = size == 4 && addr & 3 == 0;
        if self.lapic.as_ref().is_some_and(|l| l.claims(addr)) {
            let now = self.now_ns();
            let l = self.lapic.as_mut().unwrap();
            if aligned && let Some(vector) = l.write(addr - l.base(), v, now) {
                self.ioapic.eoi(vector);
                self.service_ioapic();
            }
            return true;
        }
        if self.lapic.is_some() && addr.wrapping_sub(ioapic::BASE) < ioapic::SIZE {
            if aligned {
                self.ioapic.write(addr - ioapic::BASE, v);
                self.service_ioapic();
            }
            return true;
        }
        false
    }

    fn a20_enabled(&self) -> bool {
        self.a20 || self.kbc.a20()
    }

    fn mask_a20(&self, addr: u32) -> u32 {
        if self.a20_enabled() { addr } else { addr & !(1 << 20) }
    }

    fn sync_kbc(&mut self) {
        let (kbd, aux) = self.kbc.irqs();
        self.set_irq(1, kbd);
        self.set_irq(12, aux);
        if self.kbc.reset_requested {
            self.kbc.reset_requested = false;
            self.reset_requested = true;
        }
    }

    /// Feeds bytes to the guest through COM1.
    pub fn serial_input(&mut self, bytes: &[u8]) {
        self.com1.send(bytes);
        self.set_irq(4, self.com1.irq());
    }

    pub fn key(&mut self, code: u16, pressed: bool) {
        self.kbc.key(code, pressed);
        self.sync_kbc();
    }

    /// PS/2 mouse motion (positive `dy` is up), wheel detents (positive is
    /// away from the user) and buttons.
    pub fn mouse_motion(&mut self, dx: i32, dy: i32, wheel: i32, buttons: u8) {
        self.kbc.mouse_motion(dx, dy, wheel, buttons);
        self.sync_kbc();
    }

    fn tablet_mut(&mut self) -> &mut usb::Tablet {
        self.uhci.device_mut(0).expect("the tablet sits on USB port 1")
    }

    /// Moves the USB tablet to (`x`, `y`), each 0..=`usb::MAX_COORD`.
    pub fn tablet(&mut self, x: u16, y: u16, buttons: u8, wheel: i32) {
        self.tablet_mut().input(x, y, buttons, wheel);
    }

    /// Whether the guest has a driver polling the USB tablet.
    pub fn tablet_in_use(&self) -> bool {
        self.uhci.running() && self.uhci.device::<usb::Tablet>(0).is_some_and(|t| t.in_use())
    }

    /// PAM attribute bits (bit 0 = read from RAM, bit 1 = write to RAM) for a
    /// legacy-region address in 0xC0000..0x100000.
    fn pam(&self, addr: u32) -> u8 {
        let host = &self.pci[0].1.data;
        if addr >= 0xf_0000 {
            return (host[PAM0] >> 4) & 3;
        }
        let chunk = ((addr - 0xc_0000) >> 14) as usize;
        let reg = host[PAM0 + 1 + chunk / 2];
        if chunk & 1 == 0 { reg & 3 } else { (reg >> 4) & 3 }
    }

    fn rom_byte(&self, addr: u32) -> u8 {
        if addr >= 0xe_0000 {
            let off = self.bios.len() - 0x2_0000 + (addr - 0xe_0000) as usize;
            return self.bios[off];
        }
        0xff
    }

    fn read_slow(&mut self, addr: u32) -> u8 {
        let addr = self.mask_a20(addr);
        if (LEGACY_START..LEGACY_END).contains(&addr) {
            if addr < 0xc_0000 {
                return self.display.vga().mem_read8(addr);
            }
            return if self.pam(addr) & 1 != 0 { self.ram[addr as usize] } else { self.rom_byte(addr) };
        }
        if (addr as usize) < self.ram.len() {
            return self.ram[addr as usize];
        }
        let bios_base = 0u32.wrapping_sub(self.bios.len() as u32);
        if addr >= bios_base {
            return self.bios[(addr - bios_base) as usize];
        }
        self.mmio_read(addr, 1) as u8
    }

    /// Keeps the Radeon's view of its BARs and configuration space current.
    fn sync_display_bars(&mut self) {
        if let Display::Radeon(r) = &mut self.display {
            let c = &self.pci[DISPLAY].1;
            r.bars = radeon::Bars {
                fb: c.read(0x10, 4) & !0xf,
                io: (c.read(0x14, 4) & !3) as u16,
                mmio: c.read(0x18, 4) & !0xf,
            };
            r.config = c.data;
        }
    }

    /// True if `addr` is above RAM and below the BIOS, where only PCI devices decode.
    fn is_mmio(&self, addr: u32) -> bool {
        addr as usize >= self.ram.len() && addr < 0u32.wrapping_sub(self.bios.len() as u32) && addr >= LEGACY_END
    }

    /// Reads `size` bytes (1, 2 or 4) of a PCI memory range.
    fn mmio_read(&mut self, addr: u32, size: u8) -> u32 {
        if let Some(v) = self.apic_read(addr & !3) {
            return (v >> (8 * (addr & 3))) & (u32::MAX >> (32 - 8 * size as u32));
        }
        if let Some(off) = self.nic_mmio(addr) {
            return self.nic_read(off, size);
        }
        let now = self.now_ns();
        let c = &self.pci[DISPLAY].1;
        let (fb, regs, rom) = (c.mem_bar(0), c.mem_bar(2), c.rom_bar());
        match &mut self.display {
            Display::Std(vga) => {
                if let Some(base) = fb
                    && (base..base + vga::VRAM_SIZE as u32).contains(&addr)
                {
                    return bytes_le(size, |i| vga.vram().get((addr - base) as usize + i).copied().unwrap_or(0xff));
                }
            }
            Display::Radeon(r) => {
                if let Some(base) = fb
                    && (base..base + radeon::VRAM_SIZE as u32).contains(&addr)
                {
                    let off = radeon::Radeon::fb_offset(addr - base);
                    return bytes_le(size, |i| r.vram()[(off + i) % radeon::VRAM_SIZE]);
                }
                if let Some(base) = regs
                    && (base..base + radeon::MMIO_SIZE).contains(&addr)
                {
                    r.set_time(now);
                    return r.mmio_read(addr - base, size);
                }
                if let Some(base) = rom
                    && (base..base + 0x2_0000).contains(&addr)
                {
                    let rom = r.rom();
                    return bytes_le(size, |i| rom.get((addr - base) as usize + i).copied().unwrap_or(0xff));
                }
            }
        }
        if let Display::Std(_) = self.display
            && let Some(base) = rom
            && (base..base + self.vgabios.len() as u32).contains(&addr)
        {
            return bytes_le(size, |i| self.vgabios.get((addr - base) as usize + i).copied().unwrap_or(0xff));
        }
        if let Some(phys) = self.gart(addr) {
            return bytes_le(size, |i| self.ram.get(phys as usize + i).copied().unwrap_or(0xff));
        }
        u32::MAX >> (32 - 8 * size as u32)
    }

    fn mmio_write(&mut self, addr: u32, size: u8, v: u32) {
        if self.apic_write(addr, v, size) {
            return;
        }
        if let Some(off) = self.nic_mmio(addr) {
            return self.nic_write(off, size, v);
        }
        let now = self.now_ns();
        let c = &self.pci[DISPLAY].1;
        let (fb, regs) = (c.mem_bar(0), c.mem_bar(2));
        match &mut self.display {
            Display::Std(vga) => {
                if let Some(base) = fb
                    && (base..base + vga::VRAM_SIZE as u32).contains(&addr)
                {
                    let vram = vga.vram_mut();
                    for i in 0..size as usize {
                        if let Some(b) = vram.get_mut((addr - base) as usize + i) {
                            *b = (v >> (8 * i)) as u8;
                        }
                    }
                    return;
                }
            }
            Display::Radeon(r) => {
                if let Some(base) = fb
                    && (base..base + radeon::VRAM_SIZE as u32).contains(&addr)
                {
                    let off = radeon::Radeon::fb_offset(addr - base);
                    let vram = r.vram_mut();
                    for i in 0..size as usize {
                        vram[(off + i) % radeon::VRAM_SIZE] = (v >> (8 * i)) as u8;
                    }
                    return;
                }
                if let Some(base) = regs
                    && (base..base + radeon::MMIO_SIZE).contains(&addr)
                {
                    r.set_time(now);
                    return self.radeon_write(|r, bus| r.mmio_write(addr - base, size, v, bus));
                }
            }
        }
        if let Some(phys) = self.gart(addr) {
            for i in 0..size as u32 {
                let a = phys + i;
                if (a as usize) < self.ram.len() {
                    self.code_watch.note_write(a);
                    self.ram[a as usize] = (v >> (8 * i)) as u8;
                }
            }
        }
    }

    fn gart(&self, addr: u32) -> Option<u32> {
        self.aperture().and_then(|a| a.translate(&self.ram, addr))
    }

    fn aperture(&self) -> Option<Aperture> {
        if self.chipset != Chipset::I440bx {
            return None;
        }
        let host = &self.pci[HOST].1;
        let (base, size) = pci::i440bx_aperture(host)?;
        Some(Aperture { base, size, attbase: host.read(pci::bx::ATTBASE, 4) & !0xfff })
    }

    fn write_slow(&mut self, addr: u32, v: u8) {
        let addr = self.mask_a20(addr);
        self.code_watch.note_write(addr);
        if (LEGACY_START..LEGACY_END).contains(&addr) {
            if addr < 0xc_0000 {
                self.display.vga().mem_write8(addr, v);
            } else if self.pam(addr) & 2 != 0 {
                self.ram[addr as usize] = v;
            }
            return;
        }
        if (addr as usize) < self.ram.len() {
            self.ram[addr as usize] = v;
        } else {
            self.mmio_write(addr, 1, v as u32);
        }
    }

    /// True when `[addr, addr + len)` is ordinary RAM with no special decoding.
    #[inline]
    fn plain_ram(&self, addr: u32, len: u32) -> bool {
        let end = addr as u64 + len as u64;
        self.a20_enabled() && end <= self.ram.len() as u64 && (end <= LEGACY_START as u64 || addr >= LEGACY_END)
    }

    // Index into `self.pci`. Bus 1 is whatever number the AGP bridge's secondary bus register holds.
    fn pci_function(&self) -> Option<usize> {
        let a = self.pci_addr;
        if a & 0x8000_0000 == 0 {
            return None;
        }
        let bus = ((a >> 16) & 0xff) as u8;
        let bus = match bus {
            0 => 0,
            n if self.chipset == Chipset::I440bx && self.pci[AGP_BRIDGE].1.data[pci::SECONDARY_BUS] == n => 1,
            _ => return None,
        };
        let bdf = Bdf::new(bus, ((a >> 11) & 0x1f) as u8, ((a >> 8) & 7) as u8);
        self.pci.iter().position(|(b, _)| *b == bdf)
    }

    fn io_read8(&mut self, port: u16) -> u8 {
        match port {
            0x20 | 0x21 | 0xa0 | 0xa1 | 0x4d0 | 0x4d1 => self.pic.io_read(port),
            0x40..=0x43 | 0x61 => {
                let now = self.now_ns();
                self.pit.read(port, now)
            }
            0x70 | 0x71 => self.rtc.read(port),
            0x3f0..=0x3f5 | 0x3f7 => {
                let v = self.fdc.read(port);
                self.set_irq(6, self.fdc.irq());
                v
            }
            0x92 => (self.a20 as u8) << 1,
            0x3f8..=0x3ff => {
                let v = self.com1.read(port - 0x3f8);
                self.set_irq(4, self.com1.irq());
                v
            }
            0x402 => 0xe9,
            pm::PORT_APM_CNT | pm::PORT_APM_STS => self.pm.apm_read(port),
            0x510 => 0,
            0x511 => self.fw_cfg.read_data(),
            0x60 | 0x64 => {
                let v = self.kbc.read(port);
                if port == 0x60 {
                    // Reading the output buffer drops IRQ 1/12; dropping them here gives the
                    // PIC an edge for the next byte even when it is loaded straight away.
                    self.set_irq(1, false);
                    self.set_irq(12, false);
                }
                self.sync_kbc();
                v
            }
            _ => 0xff,
        }
    }

    fn io_write8(&mut self, port: u16, v: u8) {
        match port {
            0x20 | 0x21 | 0xa0 | 0xa1 | 0x4d0 | 0x4d1 => self.pic.io_write(port, v),
            0x40..=0x43 | 0x61 => {
                let now = self.now_ns();
                self.pit.write(port, v, now)
            }
            0x70 | 0x71 => self.rtc.write(port, v),
            0x3f0..=0x3f5 | 0x3f7 => {
                self.fdc.write(port, v);
                self.set_irq(6, self.fdc.irq());
            }
            0x80 => self.post_code = v,
            0x3f8..=0x3ff => {
                self.com1.write(port - 0x3f8, v);
                self.set_irq(4, self.com1.irq());
            }
            0x92 => {
                self.a20 = v & 2 != 0;
                if v & 1 != 0 {
                    self.reset_requested = true;
                }
            }
            0x402 => {
                let _ = self.debugcon.write_all(&[v]);
                if v == b'\n' {
                    let _ = self.debugcon.flush();
                }
            }
            pm::PORT_APM_CNT | pm::PORT_APM_STS => self.pm.apm_write(port, v),
            0xcf9 if v & 4 != 0 => self.reset_requested = true,
            0x60 | 0x64 => {
                self.kbc.write(port, v);
                self.sync_kbc();
            }
            _ => {}
        }
    }

    /// Draws the display into `out` as 0x00RRGGBB pixels.
    pub fn render(&mut self, out: &mut Vec<u32>) -> Option<(u32, u32)> {
        match &mut self.display {
            Display::Std(v) => v.render(out),
            Display::Radeon(r) => r.render(out),
        }
    }

    /// True if the picture may differ from the last time this was asked.
    pub fn display_changed(&mut self) -> bool {
        match &mut self.display {
            Display::Std(v) => v.take_changed(),
            Display::Radeon(r) => r.take_changed(),
        }
    }

    /// The display's linear framebuffer: its guest physical base, host memory
    /// and size, while the guest has it decoded.
    pub fn framebuffer(&mut self) -> Option<(u32, *mut u8, usize)> {
        let base = self.pci[DISPLAY].1.mem_bar(0)?;
        match &mut self.display {
            Display::Std(v) => Some((base, v.vram_ptr(), vga::VRAM_SIZE)),
            Display::Radeon(r) => Some((base, r.vga().vram_ptr(), radeon::APER_SIZE as usize)),
        }
    }

    /// Notes that the guest wrote the framebuffer through a direct mapping.
    pub fn framebuffer_written(&mut self) {
        self.display.vga().mark_changed();
    }

    /// For each 16 KiB chunk of 0xC0000..0x100000: `Some(writable)` when the
    /// PAM registers send reads to RAM (a shadowed ROM), `None` when they go
    /// to ROM.
    pub fn shadowed_rom(&self) -> [Option<bool>; 16] {
        std::array::from_fn(|i| {
            let pam = self.pam(0xc_0000 + i as u32 * 0x4000);
            (pam & 1 != 0).then_some(pam & 2 != 0)
        })
    }

    /// Inserts an ISO image into the CD-ROM drive on the secondary master.
    pub fn attach_cdrom(&mut self, path: &str) -> std::io::Result<()> {
        self.change_cdrom(Some(path))
    }

    /// Puts the ISO at `path` in the CD-ROM drive, or empties it with `None`.
    /// The guest sees a medium change, as if someone swapped the disc, even
    /// if it had locked the tray.
    pub fn change_cdrom(&mut self, path: Option<&str>) -> std::io::Result<()> {
        let medium = match path {
            Some(p) => Some(Box::new(ide::FileBackend::open(p, true)?) as Box<dyn ide::BlockBackend>),
            None => None,
        };
        self.ide.attach_cdrom(1, 0, medium);
        self.cdrom = path.map(str::to_string);
        Ok(())
    }

    /// The ISO in the CD-ROM drive, if any.
    pub fn cdrom(&self) -> Option<&str> {
        self.cdrom.as_deref()
    }

    /// Attaches a raw disk image as the primary master.
    pub fn attach_disk(&mut self, path: &str) -> std::io::Result<()> {
        let b = ide::FileBackend::open(path, false)?;
        self.ide.attach_disk(0, 0, Box::new(b));
        Ok(())
    }

    /// Writes a Radeon register, letting the card reach memory and raise INTA.
    pub(crate) fn radeon_write(&mut self, f: impl FnOnce(&mut radeon::Radeon, &mut CardBus)) {
        let aperture = self.aperture();
        let master = self.pci[DISPLAY].1.bus_master();
        if let Display::Radeon(r) = &mut self.display {
            let mut bus = CardBus { ram: &mut self.ram[..], watch: &mut self.code_watch, aperture, master };
            f(r, &mut bus);
        }
        self.sync_pci_irqs();
    }

    /// Level of the INTx line of `self.pci[function]`.
    fn intx(&self, function: usize) -> bool {
        if function == DISPLAY
            && let Display::Radeon(r) = &self.display
        {
            return r.irq();
        }
        if let Some((i, a)) = &self.ac97
            && *i == function
        {
            return a.irq();
        }
        match function {
            UHCI => self.uhci.irq(),
            _ => self.nic.as_ref().is_some_and(|(i, n)| *i == function && n.irq()),
        }
    }

    /// PCI interrupts and the SCI are level-triggered and may share an IRQ.
    fn sync_pci_irqs(&mut self) {
        let route = &self.pci[PIIX3_ISA].1.data[pci::PIRQRC..pci::PIRQRC + 4];
        let mut levels = (self.sci as u16) << pm::SCI_IRQ;
        let mut pirqs = 0u8;
        for (i, (bdf, cfg)) in self.pci.iter().enumerate() {
            let pin = cfg.data[pci::INTERRUPT_PIN];
            if pin == 0 || !self.intx(i) {
                continue;
            }
            // Behind the AGP bridge at slot 1, INTx swizzles by the device number.
            let (slot, pin) = if bdf.bus == 0 { (bdf.dev, pin) } else { (1, pin + bdf.dev) };
            let pirq = pci::pirq_for(slot, pin);
            pirqs |= 1 << pirq;
            if let Some(irq) = pci::pirq_route(route[pirq]) {
                levels |= 1 << irq;
            }
        }
        if self.lapic.is_some() {
            for i in 0..4 {
                self.ioapic.set_irq(PIRQ_PIN + i, pirqs & 1 << i != 0);
            }
            self.service_ioapic();
        }
        let changed = levels ^ self.pci_irqs;
        self.pci_irqs = levels;
        for irq in 0..16 {
            if changed & 1 << irq != 0 {
                self.set_irq(irq, levels & 1 << irq != 0);
            }
        }
    }

    fn sync_ide(&mut self) {
        self.set_irq(14, self.ide.irq(0));
        self.set_irq(15, self.ide.irq(1));
    }

    pub fn reset(&mut self) {
        self.a20 = true;
        self.kbc = i8042::I8042::default();
        for r in PAM0..PAM0 + 7 {
            self.pci[0].1.data[r] = 0;
        }
        for r in pci::PIRQRC..pci::PIRQRC + 4 {
            self.pci[PIIX3_ISA].1.data[r] = 0x80;
        }
        self.pic = pic::Pic::default();
        if let Some(l) = self.lapic.as_mut() {
            let stats = l.stats;
            *l = apic::LocalApic::default();
            l.stats = stats;
        }
        self.ioapic = ioapic::IoApic::default();
        self.pci_irqs = 0;
        if let Display::Radeon(r) = &mut self.display {
            r.reset();
        }
        self.uhci.reset();
        if let Some((_, a)) = self.ac97.as_mut() {
            a.reset();
        }
        if let Some((_, nic)) = self.nic.as_mut() {
            nic.reset();
        }
        self.pm.reset();
        self.power.reset();
        self.sci = false;
        self.pci[PM].1 = pm::config_space();
        self.pit = pit::Pit::default();
        self.fdc = fdc::Fdc::default();
        self.com1.reset();
        self.irq0_backlog = 0;
        self.reset_requested = false;
    }
}

/// The 440BX's AGP aperture and the GATT that maps it onto RAM.
#[derive(Clone, Copy)]
struct Aperture {
    base: u32,
    size: u32,
    attbase: u32,
}

impl Aperture {
    // GATT entries are a page address with bit 0 as the valid bit, as Linux's intel-agp writes them.
    fn translate(&self, ram: &[u8], addr: u32) -> Option<u32> {
        let off = addr.wrapping_sub(self.base);
        if off >= self.size {
            return None;
        }
        let entry_addr = self.attbase as usize + (off >> 12) as usize * 4;
        let entry = u32::from_le_bytes(ram.get(entry_addr..entry_addr + 4)?.try_into().unwrap());
        (entry & 1 != 0).then_some((entry & !0xfff) | (off & 0xfff))
    }
}

/// Guest memory as the Radeon's bus master sees it, through the aperture.
struct CardBus<'a> {
    ram: &'a mut [u8],
    watch: &'a mut CodeWatch,
    aperture: Option<Aperture>,
    master: bool,
}

impl CardBus<'_> {
    fn ram_addr(&self, addr: u32) -> Option<usize> {
        let phys = self.aperture.and_then(|a| a.translate(self.ram, addr)).unwrap_or(addr) as usize;
        (phys < self.ram.len() && self.master).then_some(phys)
    }
}

impl radeon::BusMemory for CardBus<'_> {
    fn read(&mut self, addr: u32, buf: &mut [u8]) {
        let mut done = 0;
        while done < buf.len() {
            let a = addr.wrapping_add(done as u32);
            let n = (0x1000 - (a & 0xfff) as usize).min(buf.len() - done);
            match self.ram_addr(a) {
                Some(p) if p + n <= self.ram.len() => buf[done..done + n].copy_from_slice(&self.ram[p..p + n]),
                _ => buf[done..done + n].fill(0xff),
            }
            done += n;
        }
    }

    fn write(&mut self, addr: u32, buf: &[u8]) {
        let mut done = 0;
        while done < buf.len() {
            let a = addr.wrapping_add(done as u32);
            let n = (0x1000 - (a & 0xfff) as usize).min(buf.len() - done);
            if let Some(p) = self.ram_addr(a).filter(|p| p + n <= self.ram.len()) {
                self.watch.note_write(p as u32);
                self.ram[p..p + n].copy_from_slice(&buf[done..done + n]);
            }
            done += n;
        }
    }
}

fn bytes_le(size: u8, mut byte: impl FnMut(usize) -> u8) -> u32 {
    (0..size as usize).fold(0, |v, i| v | (byte(i) as u32) << (8 * i))
}

struct RamDma<'a>(&'a mut [u8], &'a mut CodeWatch);

impl ide::DmaMemory for RamDma<'_> {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.0.get(addr as usize + i).copied().unwrap_or(0xff);
        }
    }

    fn write_phys(&mut self, addr: u32, buf: &[u8]) {
        let mut page = addr & !0xfff;
        while page < addr.saturating_add(buf.len() as u32) {
            self.1.note_write(page);
            page += 0x1000;
        }
        for (i, &b) in buf.iter().enumerate() {
            if let Some(slot) = self.0.get_mut(addr as usize + i) {
                *slot = b;
            }
        }
    }
}

impl ac97::DmaMemory for RamDma<'_> {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]) {
        ide::DmaMemory::read_phys(self, addr, buf)
    }

    fn write_phys(&mut self, addr: u32, buf: &[u8]) {
        ide::DmaMemory::write_phys(self, addr, buf)
    }
}

impl net::DmaMemory for RamDma<'_> {
    fn read_phys(&mut self, addr: u64, buf: &mut [u8]) {
        match u32::try_from(addr) {
            Ok(a) => ide::DmaMemory::read_phys(self, a, buf),
            Err(_) => buf.fill(0xff),
        }
    }

    fn write_phys(&mut self, addr: u64, buf: &[u8]) {
        if let Ok(a) = u32::try_from(addr) {
            ide::DmaMemory::write_phys(self, a, buf)
        }
    }
}

/// What a card sees while the guest has bus mastering turned off.
struct NoDma;

impl net::DmaMemory for NoDma {
    fn read_phys(&mut self, _addr: u64, buf: &mut [u8]) {
        buf.fill(0xff);
    }

    fn write_phys(&mut self, _addr: u64, _buf: &[u8]) {}
}

impl usb::DmaMemory for RamDma<'_> {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]) {
        ide::DmaMemory::read_phys(self, addr, buf)
    }

    fn write_phys(&mut self, addr: u32, buf: &[u8]) {
        ide::DmaMemory::write_phys(self, addr, buf)
    }
}

impl Bus for Board {
    #[inline]
    fn read8(&mut self, addr: u32) -> u8 {
        if self.plain_ram(addr, 1) {
            return self.ram[addr as usize];
        }
        self.read_slow(addr)
    }

    #[inline]
    fn write8(&mut self, addr: u32, value: u8) {
        if self.plain_ram(addr, 1) {
            self.code_watch.note_write(addr);
            self.ram[addr as usize] = value;
            return;
        }
        self.write_slow(addr, value)
    }

    #[inline]
    fn read16(&mut self, addr: u32) -> u16 {
        if self.plain_ram(addr, 2) {
            let a = addr as usize;
            return u16::from_le_bytes([self.ram[a], self.ram[a + 1]]);
        }
        if self.is_mmio(addr) {
            return self.mmio_read(addr, 2) as u16;
        }
        u16::from_le_bytes([self.read_slow(addr), self.read_slow(addr.wrapping_add(1))])
    }

    #[inline]
    fn read32(&mut self, addr: u32) -> u32 {
        if self.plain_ram(addr, 4) {
            let a = addr as usize;
            return u32::from_le_bytes(self.ram[a..a + 4].try_into().unwrap());
        }
        if addr & 3 == 0
            && let Some(v) = self.apic_read(addr)
        {
            return v;
        }
        if self.is_mmio(addr) {
            return self.mmio_read(addr, 4);
        }
        let b = [0, 1, 2, 3].map(|i| self.read_slow(addr.wrapping_add(i)));
        u32::from_le_bytes(b)
    }

    #[inline]
    fn write16(&mut self, addr: u32, value: u16) {
        if self.plain_ram(addr, 2) {
            self.code_watch.note_write(addr);
            self.code_watch.note_write(addr + 1);
            let a = addr as usize;
            self.ram[a..a + 2].copy_from_slice(&value.to_le_bytes());
            return;
        }
        if self.is_mmio(addr) {
            return self.mmio_write(addr, 2, value as u32);
        }
        for (i, b) in value.to_le_bytes().into_iter().enumerate() {
            self.write_slow(addr.wrapping_add(i as u32), b);
        }
    }

    #[inline]
    fn write32(&mut self, addr: u32, value: u32) {
        if self.plain_ram(addr, 4) {
            self.code_watch.note_write(addr);
            self.code_watch.note_write(addr + 3);
            let a = addr as usize;
            self.ram[a..a + 4].copy_from_slice(&value.to_le_bytes());
            return;
        }
        if self.apic_write(addr, value, 4) {
            return;
        }
        if self.is_mmio(addr) {
            return self.mmio_write(addr, 4, value);
        }
        for (i, b) in value.to_le_bytes().into_iter().enumerate() {
            self.write_slow(addr.wrapping_add(i as u32), b);
        }
    }

    fn io_read(&mut self, port: u16, size: u8) -> u32 {
        if matches!(port, 0x1ce..=0x1cf | 0x3b0..=0x3df) {
            let now = self.now_ns();
            let v = match &mut self.display {
                Display::Std(vga) => vga.io_read(port, size, now),
                Display::Radeon(r) => {
                    r.set_time(now);
                    r.vga_io_read(port, size)
                }
            };
            if let Some(v) = v {
                return v;
            }
        }
        if let Display::Radeon(r) = &mut self.display
            && let Some(base) = self.pci[DISPLAY].1.io_bar(1)
            && (base..base.wrapping_add(radeon::IO_SIZE as u16)).contains(&port)
        {
            r.set_time(self.clock_base + self.start.elapsed().as_nanos() as u64);
            return r.io_read(port - base, size);
        }
        if let Some(base) = self.pci[UHCI].1.io_bar(4)
            && (base..base + UHCI_PORTS).contains(&port)
        {
            return self.uhci.read(port - base, size);
        }
        if let Some(base) = self.ac97_bar(0)
            && (base..base + ac97::NAM_LEN).contains(&port)
        {
            return self.ac97.as_mut().unwrap().1.nam_read(port - base, size);
        }
        if let Some(base) = self.ac97_bar(1)
            && (base..base + ac97::NABM_LEN).contains(&port)
        {
            return self.ac97.as_mut().unwrap().1.nabm_read(port - base, size);
        }
        if let Some(base) = pm::io_base(&self.pci[PM].1)
            && (base..base + pm::IO_LEN).contains(&port)
        {
            let now = self.now_ns();
            return self.pm.read(port - base, size, now);
        }
        if let Some(base) = pm::smbus_base(&self.pci[PM].1)
            && (base..base + pm::SMBUS_LEN).contains(&port)
        {
            return (0..size as u16).fold(0, |v, i| v | (self.pm.smbus_read(port - base + i) as u32) << (8 * i));
        }
        if let Some(off) = self.nic_port(port) {
            return self.nic_read(off, size);
        }
        if (battery::BASE..battery::BASE + battery::LEN).contains(&port) {
            return self.power.read(port - battery::BASE, size);
        }
        self.ide.set_bus_master_base(self.pci[IDE].1.io_bar(4));
        if let Some(v) = self.ide.io_read(port, size) {
            self.sync_ide();
            return v;
        }
        match port {
            0xcf8 if size == 4 => return self.pci_addr,
            0xcfc..=0xcff => {
                let off = (self.pci_addr & 0xfc) as usize + (port - 0xcfc) as usize;
                return match self.pci_function() {
                    Some(f) => self.pci[f].1.read(off, size as usize),
                    None => u32::MAX >> (32 - 8 * size as u32),
                };
            }
            _ => {}
        }
        let mut v = 0;
        for i in 0..size as u16 {
            v |= (self.io_read8(port.wrapping_add(i)) as u32) << (8 * i);
        }
        v
    }

    fn io_write(&mut self, port: u16, size: u8, value: u32) {
        let ours = match &mut self.display {
            Display::Std(vga) => vga.io_write(port, size, value),
            Display::Radeon(r) => {
                if let Some(base) = self.pci[DISPLAY].1.io_bar(1)
                    && (base..base.wrapping_add(radeon::IO_SIZE as u16)).contains(&port)
                {
                    r.set_time(self.clock_base + self.start.elapsed().as_nanos() as u64);
                    return self.radeon_write(|r, bus| r.io_write(port - base, size, value, bus));
                }
                r.vga_io_write(port, size, value)
            }
        };
        if ours {
            return;
        }
        if let Some(base) = self.pci[UHCI].1.io_bar(4)
            && (base..base + UHCI_PORTS).contains(&port)
        {
            let now = self.now_ns();
            self.uhci.write(port - base, size, value, now);
            self.sync_pci_irqs();
            return;
        }
        if let Some(base) = self.ac97_bar(0)
            && (base..base + ac97::NAM_LEN).contains(&port)
        {
            self.ac97.as_mut().unwrap().1.nam_write(port - base, size, value);
            return;
        }
        if let Some(base) = self.ac97_bar(1)
            && (base..base + ac97::NABM_LEN).contains(&port)
        {
            let dma = &mut RamDma(&mut self.ram[..], &mut self.code_watch);
            self.ac97.as_mut().unwrap().1.nabm_write(port - base, size, value, dma);
            self.sync_pci_irqs();
            return;
        }
        if let Some(base) = pm::io_base(&self.pci[PM].1)
            && (base..base + pm::IO_LEN).contains(&port)
        {
            let now = self.now_ns();
            self.pm.write(port - base, size, value, now);
            self.sync_sci();
            return;
        }
        if let Some(base) = pm::smbus_base(&self.pci[PM].1)
            && (base..base + pm::SMBUS_LEN).contains(&port)
        {
            for i in 0..size as u16 {
                self.pm.smbus_write(port - base + i, (value >> (8 * i)) as u8);
            }
            return;
        }
        if let Some(off) = self.nic_port(port) {
            return self.nic_write(off, size, value);
        }
        if (battery::BASE..battery::BASE + battery::LEN).contains(&port) {
            self.power.write(port - battery::BASE, size, value);
            return;
        }
        self.ide.set_bus_master_base(self.pci[IDE].1.io_bar(4));
        if self.ide.io_write(port, size, value, &mut RamDma(&mut self.ram[..], &mut self.code_watch)) {
            self.sync_ide();
            return;
        }
        match port {
            0xcf8 if size == 4 => {
                self.pci_addr = value;
                return;
            }
            0xcfc..=0xcff => {
                let off = (self.pci_addr & 0xfc) as usize + (port - 0xcfc) as usize;
                if let Some(f) = self.pci_function() {
                    self.pci[f].1.write(off, size as usize, value);
                    if f == HOST && self.chipset == Chipset::I440bx {
                        pci::i440bx_update(&mut self.pci[HOST].1);
                    }
                    if f == DISPLAY {
                        self.sync_display_bars();
                    }
                }
                self.sync_pci_irqs();
                return;
            }
            fw_cfg::PORT_SELECTOR if size == 2 => {
                self.fw_cfg.write_selector(value as u16);
                return;
            }
            _ => {}
        }
        for i in 0..size as u16 {
            self.io_write8(port.wrapping_add(i), (value >> (8 * i)) as u8);
        }
    }

    fn intr(&mut self) -> bool {
        match &self.lapic {
            Some(l) if l.hw_enabled() => (l.accepts_pic() && self.pic.intr()) || l.pending().is_some(),
            _ => self.pic.intr(),
        }
    }

    fn rdmsr(&mut self, msr: u32) -> Option<u64> {
        let l = self.lapic.as_ref()?;
        (msr == apic::msr::APIC_BASE).then(|| l.msr())
    }

    fn wrmsr(&mut self, msr: u32, value: u64) -> bool {
        match self.lapic.as_mut() {
            Some(l) if msr == apic::msr::APIC_BASE => {
                l.set_msr(value);
                true
            }
            _ => false,
        }
    }

    fn apic_present(&self) -> bool {
        self.lapic.as_ref().is_some_and(|l| l.hw_enabled())
    }

    fn apic_page(&self) -> Option<u32> {
        self.lapic.as_ref().filter(|l| l.hw_enabled()).map(|l| l.base())
    }

    fn apic_tpr(&self) -> u8 {
        self.lapic.as_ref().map_or(0, |l| l.tpr())
    }

    fn set_apic_tpr(&mut self, tpr: u8) {
        if let Some(l) = self.lapic.as_mut() {
            l.set_tpr(tpr);
        }
    }

    fn tpr_threshold(&self) -> u8 {
        self.lapic.as_ref().map_or(0, |l| l.tpr_threshold())
    }

    fn watch_code_page(&mut self, addr: u32) {
        let page = (addr >> 12) as usize;
        if let Some(word) = self.code_watch.bits.get_mut(page >> 6) {
            *word |= 1 << (page & 63);
        }
    }

    fn take_written_code_pages(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.code_watch.written)
    }

    /// 8259 interrupts come first, as in QEMU's apic_get_interrupt: ExtINT
    /// ignores the APIC's priorities.
    fn inta(&mut self) -> u8 {
        if let Some(l) = self.lapic.as_mut()
            && l.hw_enabled()
            && !(l.accepts_pic() && self.pic.intr())
        {
            return l.ack();
        }
        self.pic.inta()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> Board {
        Board::new(Config { ram_bytes: 1 << 20, bios: vec![0; 0x2_0000], vgabios: None, apic: true, chipset: Chipset::I440fx, gpu: Gpu::StdVga, power: battery::Power::default() })
    }

    #[test]
    fn a_battery_change_raises_the_sci_through_gpe0_and_shows_in_the_window() {
        let mut b = board();
        b.pci[PM].1.write(pm::PMBA, 4, acpi::PM_BASE as u32 | 1);
        b.pci[PM].1.write(pm::PMREGMISC, 1, 1);
        let gpe0_en = acpi::PM_BASE + pm::reg::GPE0_EN;
        b.io_write(gpe0_en, 1, 1 << pm::GPE_BATTERY);
        b.set_power(battery::Power { ac_online: false, battery: Some(battery::Battery::fake(42, false)) });
        assert!(b.sci);
        let window = |r| battery::BASE + r;
        assert_eq!(b.io_read(window(battery::reg::STATUS), 4), battery::status::BATTERY_PRESENT);
        assert_eq!(b.io_read(window(battery::reg::EVENTS), 4), battery::event::AC | battery::event::BATTERY_STATUS | battery::event::BATTERY_INFO);
    }

    #[test]
    fn timer_through_io_apic_is_not_held_back_by_an_unreachable_pic() {
        let mut b = board();
        for (port, v) in [(0x20, 0x11), (0x21, 0x08), (0x21, 0x04), (0x21, 0x01), (0x21, 0x00)] {
            b.io_write(port, 1, v);
        }
        b.write32(apic::DEFAULT_BASE + apic::reg::SVR, 0x1ff);
        b.write32(ioapic::BASE, 0x10 + 2 * IRQ0_PIN as u32);
        b.write32(ioapic::BASE + 0x10, 0x30);
        b.pulse_irq(0);
        assert!(b.irq0_waiting());
        assert!(b.intr());
        assert_eq!(b.inta(), 0x30);
        assert!(b.pic.requested(0), "the 8259 latched IRQ 0 but LINT0 is masked");
        assert!(!b.irq0_waiting());
    }

    #[test]
    fn pic_reaches_the_cpu_through_lint0_as_extint() {
        let mut b = board();
        for (port, v) in [(0x20, 0x11), (0x21, 0x08), (0x21, 0x04), (0x21, 0x01), (0x21, 0x00)] {
            b.io_write(port, 1, v);
        }
        b.pulse_irq(1);
        assert!(!b.intr());
        b.write32(apic::DEFAULT_BASE + apic::reg::SVR, 0x1ff);
        b.write32(apic::DEFAULT_BASE + apic::reg::LVT_LINT0, 0x8700);
        assert!(b.intr());
        assert_eq!(b.inta(), 0x09);
    }

    fn pci_write(b: &mut Board, dev: u32, off: u32, v: u32) {
        b.io_write(0xcf8, 4, 0x8000_0000 | dev << 11 | off);
        b.io_write(0xcfc, 4, v);
    }

    /// A board whose AC'97 controller is playing a two-frame buffer marked
    /// for an interrupt, with PIRQD routed to IRQ 11. It finishes on the
    /// first tick after a millisecond.
    fn ac97_playing_short_buffer() -> Board {
        let mut b = board();
        b.add_ac97(Box::new(ac97::NullOut));
        pci_write(&mut b, 4, 0x10, 0xc000);
        pci_write(&mut b, 4, 0x14, 0xc100);
        pci_write(&mut b, 4, 0x04, 5);
        b.io_write(0xcf8, 4, 0x8000_0000 | 1 << 11 | pci::PIRQRC as u32);
        b.io_write(0xcfc + 3, 1, 11);
        b.write32(0x1000, 0x2000);
        b.write32(0x1004, ac97::BD_IOC | 4);
        b.io_write(0xc000 + ac97::codec::reg::MASTER_VOLUME as u16, 2, 0);
        b.io_write(0xc100 + ac97::PCM_OUT + ac97::bm::BDBAR, 4, 0x1000);
        b.io_write(0xc100 + ac97::PCM_OUT + ac97::bm::CR, 1, (ac97::cr::RPBM | ac97::cr::IOCE) as u32);
        b.tick();
        std::thread::sleep(std::time::Duration::from_millis(1));
        b
    }

    #[test]
    fn ac97_interrupt_reaches_the_8259_through_its_pirq() {
        let mut b = ac97_playing_short_buffer();
        let slave_irr = |b: &Board| b.pic.registers()[1].0;
        assert_eq!(slave_irr(&b), 0);
        b.tick();
        assert_eq!(slave_irr(&b), 1 << (11 - 8));
    }

    #[test]
    fn ac97_interrupt_reaches_the_io_apic_on_pirqd() {
        let mut b = ac97_playing_short_buffer();
        b.write32(apic::DEFAULT_BASE + apic::reg::SVR, 0x1ff);
        b.write32(ioapic::BASE, 0x10 + 2 * (PIRQ_PIN as u32 + 3));
        b.write32(ioapic::BASE + 0x10, 0x8000 | 0x45);
        b.tick();
        assert_eq!(b.inta(), 0x45);
    }

    #[test]
    fn board_has_no_sound_card_unless_one_is_added() {
        let mut b = board();
        b.io_write(0xcf8, 4, 0x8000_0000 | 4 << 11);
        assert_eq!(b.io_read(0xcfc, 4), u32::MAX);
        b.add_ac97(Box::new(ac97::NullOut));
        assert_eq!(b.io_read(0xcfc, 4), 0x2415_8086);
    }

    struct Silent;

    impl net::Backend for Silent {
        fn send(&mut self, _frame: &[u8]) {}
        fn recv(&mut self) -> Option<Vec<u8>> {
            None
        }
    }

    fn pci_write_sized(b: &mut Board, dev: u32, reg: u32, size: u8, v: u32) {
        b.io_write(0xcf8, 4, 0x8000_0000 | dev << 11 | (reg & !3));
        b.io_write(0xcfc + (reg & 3) as u16, size, v);
    }

    fn pci_read(b: &mut Board, dev: u32, reg: u32) -> u32 {
        b.io_write(0xcf8, 4, 0x8000_0000 | dev << 11 | reg);
        b.io_read(0xcfc, 4)
    }

    /// A board with the card's BARs at I/O 0xc100 and 0xfebf1000, as SeaBIOS
    /// would place them, and PIRQC routed to IRQ 11.
    fn board_with_nic() -> Board {
        let mut b = board();
        b.attach_nic(net::DEFAULT_MAC, Box::new(Silent));
        pci_write_sized(&mut b, 3, 0x10, 4, 0xc100);
        pci_write_sized(&mut b, 3, 0x14, 4, 0xfebf_1000);
        pci_write_sized(&mut b, 3, 0x04, 2, 0x07);
        pci_write_sized(&mut b, 1, pci::PIRQRC as u32 + 2, 1, 11);
        b
    }

    #[test]
    fn nic_is_absent_unless_attached() {
        let mut b = board();
        assert_eq!(pci_read(&mut b, 3, 0), 0xffff_ffff);
    }

    #[test]
    fn nic_sits_at_slot_3_with_an_io_and_a_memory_bar() {
        let mut b = board();
        b.attach_nic(net::DEFAULT_MAC, Box::new(Silent));
        assert_eq!(pci_read(&mut b, 3, 0), 0x8139_10ec);
        assert_eq!(pci_read(&mut b, 3, 0x08), 0x0200_0020);
        assert_eq!(pci_read(&mut b, 3, 0x3c) >> 8 & 0xff, 1, "INTA#");
        pci_write_sized(&mut b, 3, 0x10, 4, 0xffff_ffff);
        pci_write_sized(&mut b, 3, 0x14, 4, 0xffff_ffff);
        assert_eq!(pci_read(&mut b, 3, 0x10), 0xffff_ff01);
        assert_eq!(pci_read(&mut b, 3, 0x14), 0xffff_ff00);
    }

    #[test]
    fn nic_registers_answer_through_both_bars() {
        let mut b = board_with_nic();
        assert_eq!(b.io_read(0xc100, 4), u32::from_le_bytes([0x52, 0x54, 0x00, 0x12]));
        assert_eq!(b.read16(0xfebf_1004), 0x5634);
        b.write32(0xfebf_1030, 0x0012_3400);
        assert_eq!(b.io_read(0xc130, 4), 0x0012_3400, "a 32-bit MMIO write reaches RxBuf whole");
    }

    #[test]
    fn nic_interrupt_reaches_irq_11_through_pirqc() {
        let mut b = board_with_nic();
        for (port, v) in [(0x20, 0x11), (0x21, 0x08), (0x21, 0x04), (0x21, 0x01), (0x21, 0x00), (0xa0, 0x11), (0xa1, 0x70), (0xa1, 0x02), (0xa1, 0x01), (0xa1, 0x00)] {
            b.io_write(port, 1, v);
        }
        b.io_write(0x4d1, 1, 0x08);
        b.write32(apic::DEFAULT_BASE + apic::reg::SVR, 0x1ff);
        b.write32(apic::DEFAULT_BASE + apic::reg::LVT_LINT0, 0x8700);
        b.io_write(0xc137, 1, 0x0c);
        b.io_write(0xc13c, 2, 0x0004);
        b.io_write(0xc120, 4, 0x2000);
        b.io_write(0xc110, 4, 60);
        let irr11 = |b: &Board| b.pic.registers()[1].0 & 1 << 3 != 0;
        assert!(irr11(&b));
        assert_eq!(b.inta(), 0x73);
        b.io_write(0xc13e, 2, 0x0004);
        assert!(!irr11(&b));
    }

    #[test]
    fn nic_interrupt_reaches_io_apic_input_18() {
        let mut b = board_with_nic();
        b.write32(apic::DEFAULT_BASE + apic::reg::SVR, 0x1ff);
        b.write32(ioapic::BASE, 0x10 + 2 * (PIRQ_PIN as u32 + 2));
        b.write32(ioapic::BASE + 0x10, 0x8000 | 0x42);
        b.io_write(0xc137, 1, 0x0c);
        b.io_write(0xc13c, 2, 0x0004);
        b.io_write(0xc120, 4, 0x2000);
        b.io_write(0xc110, 4, 60);
        assert!(b.intr());
        assert_eq!(b.inta(), 0x42);
        b.io_write(0xc13e, 2, 0x0004);
        b.write32(apic::DEFAULT_BASE + apic::reg::EOI, 0);
        assert!(!b.intr());
    }

    #[test]
    fn level_triggered_pci_interrupt_repeats_after_eoi_while_asserted() {
        let mut b = board();
        b.write32(apic::DEFAULT_BASE + apic::reg::SVR, 0x1ff);
        b.write32(ioapic::BASE, 0x10 + 2 * 11);
        b.write32(ioapic::BASE + 0x10, 0x8000 | 0x41);
        b.set_irq(11, true);
        assert_eq!(b.inta(), 0x41);
        assert!(!b.intr());
        b.write32(apic::DEFAULT_BASE + apic::reg::EOI, 0);
        assert_eq!(b.inta(), 0x41);
        b.set_irq(11, false);
        b.write32(apic::DEFAULT_BASE + apic::reg::EOI, 0);
        assert!(!b.intr());
    }

    #[test]
    fn reset_keeps_apic_access_counters_running() {
        let mut b = board();
        b.write32(apic::DEFAULT_BASE + apic::reg::TPR, 0x20);
        b.reset();
        b.write32(apic::DEFAULT_BASE + apic::reg::TPR, 0x20);
        assert_eq!(b.lapic.as_ref().unwrap().stats.tpr_writes, 2);
    }

    fn bx_board() -> Board {
        Board::new(Config {
            ram_bytes: 64 << 20,
            bios: vec![0; 0x2_0000],
            vgabios: None,
            apic: true,
            power: battery::Power::default(),
            chipset: Chipset::I440bx,
            gpu: Gpu::Radeon { rom: vec![0x55, 0xaa], trace: None, metal: false },
        })
    }

    fn config_read(b: &mut Board, bus: u32, dev: u32, off: u32) -> u32 {
        b.io_write(0xcf8, 4, 0x8000_0000 | bus << 16 | dev << 11 | off);
        b.io_read(0xcfc, 4)
    }

    fn config_write(b: &mut Board, bus: u32, dev: u32, off: u32, v: u32) {
        b.io_write(0xcf8, 4, 0x8000_0000 | bus << 16 | dev << 11 | off);
        b.io_write(0xcfc, 4, v);
    }

    #[test]
    fn the_cdrom_drive_is_there_without_a_disc_and_takes_one_later() {
        let mut b = board();
        b.io_write(0x176, 1, 0xa0);
        assert_eq!((b.io_read(0x174, 1), b.io_read(0x175, 1)), (0x14, 0xeb), "guests only look for a drive at boot");
        assert_eq!(b.cdrom(), None);
        let iso = std::env::temp_dir().join(format!("slopper-cdrom-test-{}.iso", std::process::id()));
        std::fs::write(&iso, vec![0u8; 2048 * 16]).unwrap();
        let path = iso.to_str().unwrap();
        b.change_cdrom(Some(path)).unwrap();
        assert_eq!(b.cdrom(), Some(path));
        b.change_cdrom(None).unwrap();
        assert_eq!(b.cdrom(), None);
        assert!(b.change_cdrom(Some("/nonexistent/disc.iso")).is_err());
        std::fs::remove_file(iso).unwrap();
    }

    #[test]
    fn agp_bus_answers_at_the_bridge_secondary_number() {
        let mut b = bx_board();
        assert_eq!(config_read(&mut b, 0, 0, 0), 0x7190_8086);
        assert_eq!(config_read(&mut b, 0, 1, 0), 0x7191_8086);
        assert_eq!(config_read(&mut b, 0, 7, 0), 0x7000_8086);
        assert_eq!(config_read(&mut b, 1, 0, 0), u32::MAX);
        config_write(&mut b, 0, 1, 0x18, 0x0002_0200);
        assert_eq!(config_read(&mut b, 2, 0, 0), 0x4e44_1002);
        assert_eq!(config_read(&mut b, 1, 0, 0), u32::MAX);
    }

    #[test]
    fn radeon_mmio_takes_whole_dwords() {
        let mut b = bx_board();
        config_write(&mut b, 0, 1, 0x18, 0x0001_0100);
        config_write(&mut b, 1, 0, 0x10, 0xe000_0000);
        config_write(&mut b, 1, 0, 0x18, 0xfe00_0000);
        config_write(&mut b, 1, 0, 0x04, 0x0003);
        b.write32(0xfe00_0010, 0x1234_5678);
        assert_eq!(b.read32(0xfe00_0010), 0x1234_5678);
        assert_eq!(b.read32(0xfe00_0100), 0xe000_0000);
        assert_eq!(b.read16(0xfe00_0f02), 0x4e44);
        b.write32(0xe000_1000, 0xcafe_f00d);
        assert_eq!(b.read8(0xe000_1001), 0xf0);
    }

    #[test]
    fn gart_aperture_maps_pages_through_attbase() {
        let mut b = bx_board();
        config_write(&mut b, 0, 0, 0x10, 0xe000_0000);
        config_write(&mut b, 0, 0, pci::bx::ATTBASE as u32, 0x0010_0000);
        b.write32(0x0010_0004, 0x0020_0001);
        assert_eq!(b.read32(0xe000_1010), u32::MAX);
        config_write(&mut b, 0, 0, pci::bx::NBXCFG as u32, pci::bx::APERTURE_ENABLE);
        b.write32(0x0020_0010, 0xabcd_ef01);
        assert_eq!(b.read32(0xe000_1010), 0xabcd_ef01);
        b.write16(0xe000_1012, 0x5555);
        assert_eq!(b.read32(0x0020_0010), 0x5555_ef01);
        assert_eq!(b.read32(0xe000_2000), u32::MAX);
    }

    #[test]
    fn radeon_scratch_writeback_reaches_ram_through_the_aperture() {
        let mut b = bx_board();
        config_write(&mut b, 0, 0, 0x10, 0xd000_0000);
        config_write(&mut b, 0, 0, pci::bx::ATTBASE as u32, 0x0010_0000);
        config_write(&mut b, 0, 0, pci::bx::NBXCFG as u32, pci::bx::APERTURE_ENABLE);
        b.write32(0x0010_0000, 0x0030_0001);
        config_write(&mut b, 0, 1, 0x18, 0x0001_0100);
        config_write(&mut b, 1, 0, 0x18, 0xfe00_0000);
        config_write(&mut b, 1, 0, 0x04, 0x0002);
        b.write32(0xfe00_0000 + radeon::regs::MC_AGP_LOCATION, 0xf87f_f800);
        b.write32(0xfe00_0000 + radeon::regs::AGP_BASE, 0xd000_0000);
        b.write32(0xfe00_0000 + radeon::regs::SCRATCH_ADDR, 0xf800_0100);
        b.write32(0xfe00_0000 + radeon::regs::SCRATCH_UMSK, 1);
        b.write32(0xfe00_0000 + radeon::regs::SCRATCH_REG0, 0xdead_beef);
        assert_eq!(b.read32(0x0030_0100), 0, "bus mastering is off");
        config_write(&mut b, 1, 0, 0x04, 0x0006);
        b.write32(0xfe00_0000 + radeon::regs::SCRATCH_REG0, 0xdead_beef);
        assert_eq!(b.read32(0x0030_0100), 0xdead_beef);
    }
}

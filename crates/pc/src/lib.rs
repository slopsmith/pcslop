//! An i440FX + PIIX3 PC: memory map, chipset registers and legacy devices.

pub mod acpi;
pub mod apic;
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
pub mod uart;

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
    pub vga: vga::Vga,
    pub ide: ide::Ide,
    pub uhci: usb::Uhci,
    pub pm: pm::Pm,
    vgabios: Vec<u8>,
    pub debugcon: Box<dyn Write + Send>,
    start: Instant,
    pub reset_requested: bool,
    pub post_code: u8,
}

const LEGACY_START: u32 = 0xa_0000;
const PIIX3_ISA: usize = 1;
const IDE: usize = 2;
const DISPLAY: usize = 3;
const UHCI: usize = 4;
const PM: usize = 5;
const UHCI_PORTS: u16 = 32;
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
        let mut b = Board {
            ram: ram::Ram::new(ram_bytes as usize),
            code_watch: CodeWatch { bits: vec![0; (ram_bytes as usize >> 12).div_ceil(64)], written: Vec::new() },
            bios: cfg.bios,
            a20: true,
            pci_addr: 0,
            pci_irqs: 0,
            sci: false,
            pci: vec![
                (Bdf { dev: 0, func: 0 }, pci::host_bridge()),
                (Bdf { dev: 1, func: 0 }, pci::piix3_isa()),
                (Bdf { dev: 1, func: 1 }, pci::piix3_ide()),
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
            vga: vga::Vga::new(),
            ide: ide::Ide::new(),
            uhci: usb::Uhci::new(),
            pm: pm::Pm::default(),
            vgabios: Vec::new(),
            debugcon: Box::new(std::io::stdout()),
            start: Instant::now(),
            reset_requested: false,
            post_code: 0,
        };
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
        b.pci.push((Bdf { dev: 2, func: 0 }, display));
        b.pci.push((Bdf { dev: 1, func: 2 }, pci::piix3_uhci()));
        b.pci.push((Bdf { dev: 1, func: 3 }, pm::config_space()));
        b.uhci.attach(0, Box::new(usb::Tablet::new()));
        b.setup_cmos(ram_bytes);
        b.fw_cfg.set(fw_cfg::key::RAM_SIZE, (ram_bytes as u64).to_le_bytes().to_vec());
        let e820 = fw_cfg::e820(&[(0, ram_bytes as u64, 1)]);
        b.fw_cfg.add_file("etc/e820", e820);
        b.fw_cfg.set(fw_cfg::key::SMBIOS_ENTRIES, fw_cfg::smbios_fields(SMBIOS));
        let acpi = acpi::build(ram_bytes, cfg.apic);
        b.fw_cfg.add_file(acpi::TABLES_FILE, acpi.tables);
        b.fw_cfg.add_file(acpi::RSDP_FILE, acpi.rsdp);
        b.fw_cfg.add_file(acpi::LOADER_FILE, acpi.loader);
        if cfg.apic {
            b.fw_cfg.add_file("etc/irq0-override", 1u32.to_le_bytes().to_vec());
        }
        b
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
        self.start.elapsed().as_nanos() as u64
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
        self.pit.next_irq0_ns(now).unwrap_or(1_000_000).min(usb.unwrap_or(u64::MAX)).min(lapic.unwrap_or(u64::MAX)).min(1_000_000)
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

    pub fn mouse_motion(&mut self, dx: i32, dy: i32, buttons: u8) {
        self.kbc.mouse_motion(dx, dy, buttons);
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
                return self.vga.mem_read8(addr);
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
        self.mmio_read(addr)
    }

    fn display(&self) -> &ConfigSpace {
        &self.pci[DISPLAY].1
    }

    fn mmio_read(&mut self, addr: u32) -> u8 {
        if let Some(v) = self.apic_read(addr & !3) {
            return (v >> (8 * (addr & 3))) as u8;
        }
        if let Some(base) = self.display().mem_bar(0)
            && (base..base + vga::VRAM_SIZE as u32).contains(&addr)
        {
            return self.vga.vram()[(addr - base) as usize];
        }
        if let Some(base) = self.display().rom_bar()
            && (base..base + self.vgabios.len() as u32).contains(&addr)
        {
            return self.vgabios[(addr - base) as usize];
        }
        0xff
    }

    fn mmio_write(&mut self, addr: u32, v: u8) {
        if self.apic_write(addr, v as u32, 1) {
            return;
        }
        if let Some(base) = self.display().mem_bar(0)
            && (base..base + vga::VRAM_SIZE as u32).contains(&addr)
        {
            self.vga.vram_mut()[(addr - base) as usize] = v;
        }
    }

    fn write_slow(&mut self, addr: u32, v: u8) {
        let addr = self.mask_a20(addr);
        self.code_watch.note_write(addr);
        if (LEGACY_START..LEGACY_END).contains(&addr) {
            if addr < 0xc_0000 {
                self.vga.mem_write8(addr, v);
            } else if self.pam(addr) & 2 != 0 {
                self.ram[addr as usize] = v;
            }
            return;
        }
        if (addr as usize) < self.ram.len() {
            self.ram[addr as usize] = v;
        } else {
            self.mmio_write(addr, v);
        }
    }

    /// True when `[addr, addr + len)` is ordinary RAM with no special decoding.
    #[inline]
    fn plain_ram(&self, addr: u32, len: u32) -> bool {
        let end = addr as u64 + len as u64;
        self.a20_enabled() && end <= self.ram.len() as u64 && (end <= LEGACY_START as u64 || addr >= LEGACY_END)
    }

    fn pci_function(&mut self) -> Option<&mut ConfigSpace> {
        let a = self.pci_addr;
        if a & 0x8000_0000 == 0 || (a >> 16) & 0xff != 0 {
            return None;
        }
        let bdf = Bdf { dev: ((a >> 11) & 0x1f) as u8, func: ((a >> 8) & 7) as u8 };
        self.pci.iter_mut().find(|(b, _)| *b == bdf).map(|(_, c)| c)
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
        self.vga.render(out)
    }

    /// True if the picture may differ from the last time this was asked.
    pub fn display_changed(&mut self) -> bool {
        self.vga.take_changed()
    }

    /// The display's linear framebuffer: its guest physical base, host memory
    /// and size, while the guest has it decoded.
    pub fn framebuffer(&mut self) -> Option<(u32, *mut u8, usize)> {
        let base = self.display().mem_bar(0)?;
        Some((base, self.vga.vram_ptr(), vga::VRAM_SIZE))
    }

    /// Notes that the guest wrote the framebuffer through a direct mapping.
    pub fn framebuffer_written(&mut self) {
        self.vga.mark_changed();
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
        let b = ide::FileBackend::open(path, true)?;
        self.ide.attach_cdrom(1, 0, Some(Box::new(b)));
        Ok(())
    }

    /// Attaches a raw disk image as the primary master.
    pub fn attach_disk(&mut self, path: &str) -> std::io::Result<()> {
        let b = ide::FileBackend::open(path, false)?;
        self.ide.attach_disk(0, 0, Box::new(b));
        Ok(())
    }

    /// Level of the INTx line of `self.pci[function]`.
    fn intx(&self, function: usize) -> bool {
        function == UHCI && self.uhci.irq()
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
            let pirq = pci::pirq_for(bdf.dev, pin);
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
            *l = apic::LocalApic::default();
        }
        self.ioapic = ioapic::IoApic::default();
        self.pci_irqs = 0;
        self.uhci.reset();
        self.pm.reset();
        self.sci = false;
        self.pci[PM].1 = pm::config_space();
        self.pit = pit::Pit::default();
        self.fdc = fdc::Fdc::default();
        self.com1.reset();
        self.irq0_backlog = 0;
        self.reset_requested = false;
    }
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
        for (i, b) in value.to_le_bytes().into_iter().enumerate() {
            self.write_slow(addr.wrapping_add(i as u32), b);
        }
    }

    fn io_read(&mut self, port: u16, size: u8) -> u32 {
        if matches!(port, 0x1ce..=0x1cf | 0x3b0..=0x3df) {
            let now = self.now_ns();
            if let Some(v) = self.vga.io_read(port, size, now) {
                return v;
            }
        }
        if let Some(base) = self.pci[UHCI].1.io_bar(4)
            && (base..base + UHCI_PORTS).contains(&port)
        {
            return self.uhci.read(port - base, size);
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
                    Some(f) => f.read(off, size as usize),
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
        if self.vga.io_write(port, size, value) {
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
                    f.write(off, size as usize, value);
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
        Board::new(Config { ram_bytes: 1 << 20, bios: vec![0; 0x2_0000], vgabios: None, apic: true })
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
}

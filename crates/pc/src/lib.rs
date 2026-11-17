//! An i440FX + PIIX3 PC: memory map, chipset registers and legacy devices.

pub mod fdc;
pub mod fw_cfg;
pub mod i8042;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod ram;
pub mod rtc;

use std::io::Write;
use std::time::Instant;

use pci::{Bdf, ConfigSpace, PAM0};
use x86::bus::Bus;

pub struct Config {
    pub ram_bytes: u32,
    pub bios: Vec<u8>,
    pub vgabios: Option<Vec<u8>>,
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
    pub pic: pic::Pic,
    pub pit: pit::Pit,
    fdc: fdc::Fdc,
    /// Timer interrupts owed to the guest: Windows counts them to keep time,
    /// so ticks that arrive while one is still pending are delivered later.
    irq0_backlog: u32,
    pub rtc: rtc::Rtc,
    pub fw_cfg: fw_cfg::FwCfg,
    pub kbc: i8042::I8042,
    pub vga: vga::Vga,
    pub ide: ide::Ide,
    vgabios: Vec<u8>,
    pub debugcon: Box<dyn Write + Send>,
    start: Instant,
    pub reset_requested: bool,
    pub post_code: u8,
}

const LEGACY_START: u32 = 0xa_0000;
const IDE: usize = 2;
const DISPLAY: usize = 3;
const LEGACY_END: u32 = 0x10_0000;

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
            pci: vec![
                (Bdf { dev: 0, func: 0 }, pci::host_bridge()),
                (Bdf { dev: 1, func: 0 }, pci::piix3_isa()),
                (Bdf { dev: 1, func: 1 }, pci::piix3_ide()),
            ],
            pic: pic::Pic::default(),
            pit: pit::Pit::default(),
            fdc: fdc::Fdc::default(),
            irq0_backlog: 0,
            rtc: rtc::Rtc::default(),
            fw_cfg: fw_cfg::FwCfg::new(),
            kbc: i8042::I8042::default(),
            vga: vga::Vga::new(),
            ide: ide::Ide::new(),
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
        b.setup_cmos(ram_bytes);
        b.fw_cfg.set(fw_cfg::key::RAM_SIZE, (ram_bytes as u64).to_le_bytes().to_vec());
        let e820 = fw_cfg::e820(&[(0, ram_bytes as u64, 1)]);
        b.fw_cfg.add_file("etc/e820", e820);
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
        if self.irq0_backlog > 0 && !self.pic.requested(0) {
            self.irq0_backlog -= 1;
            self.pic.pulse_irq(0);
        }
        if self.rtc.poll(now) {
            self.pic.pulse_irq(8);
        }
    }

    /// Timer interrupts not yet delivered; it grows when the guest falls behind.
    pub fn timer_backlog(&self) -> u32 {
        self.irq0_backlog
    }

    /// Nanoseconds until a timer will next need attention.
    pub fn next_event_ns(&self) -> u64 {
        let now = self.now_ns();
        if self.irq0_backlog > 0 && !self.pic.requested(0) {
            return 0;
        }
        self.pit.next_irq0_ns(now).unwrap_or(1_000_000).min(1_000_000)
    }

    fn a20_enabled(&self) -> bool {
        self.a20 || self.kbc.a20()
    }

    fn mask_a20(&self, addr: u32) -> u32 {
        if self.a20_enabled() { addr } else { addr & !(1 << 20) }
    }

    fn sync_kbc(&mut self) {
        let (kbd, aux) = self.kbc.irqs();
        self.pic.set_irq(1, kbd);
        self.pic.set_irq(12, aux);
        if self.kbc.reset_requested {
            self.kbc.reset_requested = false;
            self.reset_requested = true;
        }
    }

    pub fn key(&mut self, code: u16, pressed: bool) {
        self.kbc.key(code, pressed);
        self.sync_kbc();
    }

    pub fn mouse_motion(&mut self, dx: i32, dy: i32, buttons: u8) {
        self.kbc.mouse_motion(dx, dy, buttons);
        self.sync_kbc();
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
                self.pic.set_irq(6, self.fdc.irq());
                v
            }
            0x92 => (self.a20 as u8) << 1,
            0x402 => 0xe9,
            0x510 => 0,
            0x511 => self.fw_cfg.read_data(),
            0x60 | 0x64 => {
                let v = self.kbc.read(port);
                if port == 0x60 {
                    // Reading the output buffer drops IRQ 1/12; dropping them here gives the
                    // PIC an edge for the next byte even when it is loaded straight away.
                    self.pic.set_irq(1, false);
                    self.pic.set_irq(12, false);
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
                self.pic.set_irq(6, self.fdc.irq());
            }
            0x80 => self.post_code = v,
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

    fn sync_ide(&mut self) {
        self.pic.set_irq(14, self.ide.irq(0));
        self.pic.set_irq(15, self.ide.irq(1));
    }

    pub fn reset(&mut self) {
        self.a20 = true;
        self.kbc = i8042::I8042::default();
        for r in PAM0..PAM0 + 7 {
            self.pci[0].1.data[r] = 0;
        }
        self.pic = pic::Pic::default();
        self.pit = pit::Pit::default();
        self.fdc = fdc::Fdc::default();
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
        self.pic.intr()
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

    fn inta(&mut self) -> u8 {
        self.pic.inta()
    }
}

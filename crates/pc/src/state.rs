//! The board's part of a save state: every device register, and a
//! description of the machine that a restore must match.

use std::time::Instant;

use snapshot::{Error, Reader, Result, Snap, Writer};

use crate::pci::ConfigSpace;
use crate::{Board, Chipset, Display};

/// What a snapshot's machine must have in common with the one restoring it.
#[derive(Snap, Default, Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub ram_bytes: u32,
    pub chipset: String,
    pub display: String,
    pub apic: bool,
    /// Bus, device and function of each PCI function, with its vendor and device IDs.
    pub pci: Vec<(u32, u32)>,
    pub bios_hash: u64,
    pub vgabios_hash: u64,
    pub sound: bool,
    pub nic: bool,
}

impl Machine {
    /// How `other` differs from `self`, one line per difference.
    pub fn differences(&self, other: &Machine) -> Vec<String> {
        let mut out = Vec::new();
        let mut check = |what: &str, a: String, b: String| {
            if a != b {
                out.push(format!("{what}: snapshot has {a}, this machine {b}"));
            }
        };
        check("RAM", format!("{} MiB", self.ram_bytes >> 20), format!("{} MiB", other.ram_bytes >> 20));
        check("chipset", self.chipset.clone(), other.chipset.clone());
        check("display", self.display.clone(), other.display.clone());
        check("APICs", self.apic.to_string(), other.apic.to_string());
        check("BIOS", format!("{:016x}", self.bios_hash), format!("{:016x}", other.bios_hash));
        check("VGA BIOS", format!("{:016x}", self.vgabios_hash), format!("{:016x}", other.vgabios_hash));
        check("sound card", self.sound.to_string(), other.sound.to_string());
        check("network card", self.nic.to_string(), other.nic.to_string());
        if self.pci != other.pci {
            check("PCI functions", format!("{:x?}", self.pci), format!("{:x?}", other.pci));
        }
        out
    }
}

/// The schema of everything `save_devices` writes.
const DEVICES_SCHEMA: u64 = snapshot::mix(
    snapshot::fnv(b"board devices v1"),
    &[
        ConfigSpace::SCHEMA,
        crate::pic::Pic::SCHEMA,
        crate::apic::LocalApic::SCHEMA,
        crate::ioapic::IoApic::SCHEMA,
        crate::pit::Pit::SCHEMA,
        crate::fdc::Fdc::SCHEMA,
        crate::rtc::Rtc::SCHEMA,
        crate::uart::Uart::SCHEMA,
        crate::fw_cfg::FwCfg::SCHEMA,
        crate::i8042::I8042::SCHEMA,
        vga::Vga::SCHEMA,
        radeon::Radeon::SCHEMA,
        ide::Ide::SCHEMA,
        usb::Uhci::SCHEMA,
        usb::Tablet::SCHEMA,
        crate::pm::Pm::SCHEMA,
        ac97::Ac97::SCHEMA,
        net::Rtl8139::SCHEMA,
        crate::battery::PowerDevice::SCHEMA,
    ],
);

fn put_opt<T: Snap>(w: &mut Writer, v: Option<&T>) {
    w.put(&v.is_some());
    if let Some(v) = v {
        w.put(v);
    }
}

fn get_opt<T: Snap>(r: &mut Reader<'_>, v: Option<&mut T>, what: &str) -> Result<()> {
    let mut present = false;
    r.get(&mut present)?;
    match (present, v) {
        (false, None) => Ok(()),
        (true, Some(v)) => r.get(v),
        (saved, _) => Err(Error::new(format!("the snapshot {} a {what}, this machine does not", if saved { "has" } else { "lacks" }))),
    }
}

impl Board {
    pub fn machine(&self) -> Machine {
        Machine {
            ram_bytes: self.ram.len() as u32,
            chipset: match self.chipset {
                Chipset::I440fx => "440FX".into(),
                Chipset::I440bx => "440BX".into(),
            },
            display: match self.display {
                Display::Std(_) => "stdvga".into(),
                Display::Radeon(_) => "radeon".into(),
            },
            apic: self.lapic.is_some(),
            pci: self.pci.iter().map(|(b, c)| ((b.bus as u32) << 16 | (b.dev as u32) << 8 | b.func as u32, c.read(0, 4))).collect(),
            bios_hash: snapshot::fnv(&self.bios),
            vgabios_hash: snapshot::fnv(&self.vgabios),
            sound: self.ac97.is_some(),
            nic: self.nic.is_some(),
        }
    }

    pub fn devices_schema() -> u64 {
        DEVICES_SCHEMA
    }

    /// Writes every device's state. Guest RAM and VRAM are left to the caller.
    pub fn save_devices(&mut self, w: &mut Writer) -> Result<()> {
        if let Display::Radeon(card) = &mut self.display {
            card.gpu_sync();
        }
        w.u64(self.now_ns());
        w.put(&self.a20);
        w.put(&self.pci_addr);
        w.u64(self.pci.len() as u64);
        for (_, c) in &self.pci {
            w.put(c);
        }
        w.put(&self.pci_irqs);
        w.put(&self.sci);
        w.put(&self.pic);
        put_opt(w, self.lapic.as_ref());
        w.put(&self.ioapic);
        w.put(&self.pit);
        w.put(&self.fdc);
        w.put(&self.irq0_backlog);
        w.put(&self.rtc);
        w.put(&self.com1);
        w.put(&self.fw_cfg);
        w.put(&self.kbc);
        match &self.display {
            Display::Std(v) => w.put(v.as_ref()),
            Display::Radeon(r) => w.put(r.as_ref()),
        }
        w.put(&self.ide);
        w.put(&self.uhci);
        put_opt(w, self.uhci.device::<usb::Tablet>(0));
        w.put(&self.pm);
        put_opt(w, self.ac97.as_ref().map(|(_, a)| a));
        put_opt(w, self.nic.as_ref().map(|(_, n)| n));
        w.put(&self.power);
        w.put(&self.post_code);
        Ok(())
    }

    /// Loads what `save_devices` wrote into a board built with the same
    /// `Machine`, with the same drives attached. The board's clock carries
    /// on from the snapshot's time, so timers neither fire early nor late.
    pub fn load_devices(&mut self, r: &mut Reader<'_>) -> Result<()> {
        let now = r.u64()?;
        r.get(&mut self.a20)?;
        r.get(&mut self.pci_addr)?;
        if r.u64()? != self.pci.len() as u64 {
            return Err(Error::new("the snapshot has a different set of PCI functions"));
        }
        for (_, c) in &mut self.pci {
            r.get(c)?;
        }
        r.get(&mut self.pci_irqs)?;
        r.get(&mut self.sci)?;
        r.get(&mut self.pic)?;
        get_opt(r, self.lapic.as_mut(), "local APIC")?;
        r.get(&mut self.ioapic)?;
        r.get(&mut self.pit)?;
        r.get(&mut self.fdc)?;
        r.get(&mut self.irq0_backlog)?;
        r.get(&mut self.rtc)?;
        r.get(&mut self.com1)?;
        r.get(&mut self.fw_cfg)?;
        r.get(&mut self.kbc)?;
        match &mut self.display {
            Display::Std(v) => {
                r.get(v.as_mut())?;
                v.mark_changed();
            }
            Display::Radeon(card) => {
                r.get(card.as_mut())?;
                card.vga().mark_changed();
            }
        }
        r.get(&mut self.ide)?;
        r.get(&mut self.uhci)?;
        get_opt(r, self.uhci.device_mut::<usb::Tablet>(0), "USB tablet")?;
        r.get(&mut self.pm)?;
        get_opt(r, self.ac97.as_mut().map(|(_, a)| a), "sound card")?;
        get_opt(r, self.nic.as_mut().map(|(_, n)| n), "network card")?;
        r.get(&mut self.power)?;
        r.get(&mut self.post_code)?;
        if !r.is_empty() {
            return Err(Error::new("device state has trailing data"));
        }
        self.code_watch.bits.fill(0);
        self.code_watch.written.clear();
        self.reset_requested = false;
        self.sync_display_bars();
        self.rebase_clock(now);
        Ok(())
    }

    /// The display's VRAM, which a snapshot stores beside the device state.
    pub fn vram_mut(&mut self) -> Option<&mut [u8]> {
        match &mut self.display {
            Display::Std(v) => Some(v.vram_mut()),
            Display::Radeon(r) => Some(r.vram_mut()),
        }
    }

    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    pub fn ram_mut(&mut self) -> &mut [u8] {
        &mut self.ram
    }

    /// Takes out the hard disk, for a snapshot that had none.
    pub fn remove_disk(&mut self) {
        self.ide.detach(0, 0);
    }

    /// Writes buffered disk data to the image files.
    pub fn flush_disks(&mut self) -> std::io::Result<()> {
        self.ide.flush()
    }

    /// Cables the network card to another backend, dropping the old one first.
    pub fn replace_nic_backend(&mut self, start: impl FnOnce() -> Option<Box<dyn net::Backend>>) {
        if let Some((_, nic)) = self.nic.as_mut() {
            nic.set_backend(Box::new(net::Unplugged));
            if let Some(b) = start() {
                nic.set_backend(b);
            }
        }
    }

    pub(crate) fn rebase_clock(&mut self, now_ns: u64) {
        self.clock_base = now_ns;
        self.start = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Gpu, battery};
    use x86::bus::Bus;

    struct Silent;

    impl net::Backend for Silent {
        fn send(&mut self, _frame: &[u8]) {}
        fn recv(&mut self) -> Option<Vec<u8>> {
            None
        }
    }

    fn board() -> Board {
        let mut b = Board::new(Config {
            ram_bytes: 4 << 20,
            bios: vec![0; 0x2_0000],
            vgabios: None,
            apic: true,
            chipset: Chipset::I440fx,
            gpu: Gpu::StdVga,
            power: battery::Power::default(),
        });
        b.add_ac97(Box::new(ac97::NullOut));
        b.attach_nic(net::DEFAULT_MAC, Box::new(Silent));
        b
    }

    /// A board with state in most devices: an initialised PIC, a running
    /// PIT, CMOS and VGA writes, PCI BARs, network card registers and
    /// tablet input.
    fn busy_board() -> Board {
        let mut b = board();
        for (port, v) in [(0x20, 0x11), (0x21, 0x08), (0x21, 0x04), (0x21, 0x01), (0x21, 0xfc), (0x43, 0x34), (0x40, 0x9c), (0x40, 0x2e)] {
            b.io_write(port, 1, v);
        }
        b.io_write(0x70, 1, 0x40);
        b.io_write(0x71, 1, 0x5a);
        b.io_write(0x3c8, 1, 3);
        for v in [10, 20, 30] {
            b.io_write(0x3c9, 1, v);
        }
        b.io_write(0xcf8, 4, 0x8000_0000 | 3 << 11 | 0x10);
        b.io_write(0xcfc, 4, 0xc100);
        b.io_write(0xcf8, 4, 0x8000_0000 | 3 << 11 | 0x04);
        b.io_write(0xcfc, 2, 0x07);
        b.io_write(0xc130, 4, 0x0012_3400);
        b.write32(crate::apic::DEFAULT_BASE + crate::apic::reg::SVR, 0x1ff);
        b.tablet(100, 200, 1, 0);
        b.write32(0x1234, 0xdead_beef);
        b
    }

    fn save(b: &mut Board) -> Vec<u8> {
        let mut w = Writer::new();
        b.save_devices(&mut w).unwrap();
        w.buf
    }

    #[test]
    fn devices_saved_after_a_restore_match_the_original() {
        let mut a = busy_board();
        let saved = save(&mut a);
        let mut b = board();
        b.load_devices(&mut Reader::new(&saved)).unwrap();
        let again = save(&mut b);
        assert_eq!(saved[8..], again[8..], "everything after the clock");
    }

    #[test]
    fn restored_registers_answer_like_the_original() {
        let mut a = busy_board();
        let saved = save(&mut a);
        let mut b = board();
        b.load_devices(&mut Reader::new(&saved)).unwrap();
        b.io_write(0x70, 1, 0x40);
        assert_eq!(b.io_read(0x71, 1), 0x5a);
        assert_eq!(b.io_read(0x21, 1), 0xfc);
        assert_eq!(b.io_read(0xc130, 4), 0x0012_3400);
    }

    #[test]
    fn the_board_clock_carries_on_from_the_snapshot() {
        let mut a = board();
        a.rebase_clock(5_000_000_000);
        let saved = save(&mut a);
        let mut b = board();
        b.load_devices(&mut Reader::new(&saved)).unwrap();
        let now = b.now_ns();
        assert!((5_000_000_000..6_000_000_000).contains(&now), "clock at {now}");
    }

    #[test]
    fn a_restore_into_a_board_without_the_network_card_is_refused() {
        let mut a = board();
        let saved = save(&mut a);
        let mut b = Board::new(Config {
            ram_bytes: 4 << 20,
            bios: vec![0; 0x2_0000],
            vgabios: None,
            apic: true,
            chipset: Chipset::I440fx,
            gpu: Gpu::StdVga,
            power: battery::Power::default(),
        });
        b.add_ac97(Box::new(ac97::NullOut));
        assert!(b.load_devices(&mut Reader::new(&saved)).is_err());
        assert!(!a.machine().differences(&b.machine()).is_empty());
    }

    #[test]
    fn machines_differ_by_ram_size_and_bios() {
        let a = board();
        let mut b = Board::new(Config {
            ram_bytes: 8 << 20,
            bios: vec![1; 0x2_0000],
            vgabios: None,
            apic: true,
            chipset: Chipset::I440fx,
            gpu: Gpu::StdVga,
            power: battery::Power::default(),
        });
        b.add_ac97(Box::new(ac97::NullOut));
        b.attach_nic(net::DEFAULT_MAC, Box::new(Silent));
        assert_eq!(a.machine().differences(&b.machine()).len(), 2);
        assert!(a.machine().differences(&board().machine()).is_empty());
    }

    #[test]
    fn the_radeon_registers_come_back_after_a_restore() {
        let radeon = || {
            Board::new(Config {
                ram_bytes: 64 << 20,
                bios: vec![0; 0x2_0000],
                vgabios: None,
                apic: true,
                power: battery::Power::default(),
                chipset: Chipset::I440bx,
                gpu: Gpu::Radeon { rom: vec![0x55, 0xaa], trace: None, metal: false },
            })
        };
        let mut a = radeon();
        let mut w = Writer::new();
        a.save_devices(&mut w).unwrap();
        let mut b = radeon();
        b.load_devices(&mut Reader::new(&w.buf)).unwrap();
        let mut w2 = Writer::new();
        b.save_devices(&mut w2).unwrap();
        assert_eq!(w.buf.len(), w2.buf.len());
    }
}

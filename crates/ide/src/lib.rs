//! PIIX3 IDE function (PCI 8086:7010): two legacy-mode channels with ATA
//! hard disks and ATAPI CD-ROMs, plus SFF-8038i bus master DMA.

mod ata;
mod atapi;
mod backend;
mod busmaster;
mod channel;

pub use backend::{BlockBackend, FileBackend, MemBackend};

use channel::Channel;

pub const PCI_VENDOR: u16 = 0x8086;
pub const PCI_DEVICE: u16 = 0x7010;
pub const PCI_CLASS: u8 = 0x01;
pub const PCI_SUBCLASS: u8 = 0x01;
/// Both channels in legacy mode, bus master capable.
pub const PCI_PROG_IF: u8 = 0x80;
/// Class code as `(class << 16) | (subclass << 8) | prog_if`.
pub const PCI_CLASS_CODE: u32 = (PCI_CLASS as u32) << 16 | (PCI_SUBCLASS as u32) << 8 | PCI_PROG_IF as u32;

/// Guest physical memory as seen by the bus master engine.
pub trait DmaMemory {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]);
    fn write_phys(&mut self, addr: u32, buf: &[u8]);
}

const CMD_BASE: [u16; 2] = [0x1f0, 0x170];
const CTL_PORT: [u16; 2] = [0x3f6, 0x376];

#[derive(snapshot::Snap)]
pub struct Ide {
    ch: [Channel; 2],
    bm_base: Option<u16>,
}

impl Default for Ide {
    fn default() -> Self {
        Self::new()
    }
}

impl Ide {
    pub fn new() -> Self {
        Ide { ch: [Channel::new(), Channel::new()], bm_base: None }
    }

    pub fn attach_disk(&mut self, channel: usize, slot: usize, b: Box<dyn BlockBackend>) {
        self.ch[channel].attach_disk(slot, b);
    }

    /// Leaves the slot without a drive.
    pub fn detach(&mut self, channel: usize, slot: usize) {
        self.ch[channel].detach(slot);
    }

    /// Attaches a CD-ROM drive, or changes the medium if the slot already
    /// holds one (`None` ejects it). A medium change raises a unit attention.
    pub fn attach_cdrom(&mut self, channel: usize, slot: usize, b: Option<Box<dyn BlockBackend>>) {
        self.ch[channel].attach_cdrom(slot, b);
    }

    /// Writes buffered data of every disk to its backend's storage.
    pub fn flush(&mut self) -> std::io::Result<()> {
        for ch in &mut self.ch {
            for d in &mut ch.dev {
                if let channel::Kind::Disk(disk) = &mut d.kind {
                    disk.backend.flush()?;
                }
            }
        }
        Ok(())
    }

    pub fn set_bus_master_base(&mut self, base: Option<u16>) {
        self.bm_base = base.map(|b| b & !0xf);
    }

    /// Mirrors the IDE Decode Enable bit of a channel's IDETIM register
    /// (config 0x41 bit 7 / 0x43 bit 7). Channels decode by default.
    pub fn set_channel_enabled(&mut self, channel: usize, enabled: bool) {
        self.ch[channel].enabled = enabled;
    }

    /// None if the port is not ours.
    pub fn io_read(&mut self, port: u16, size: u8) -> Option<u32> {
        for (c, ch) in self.ch.iter_mut().enumerate() {
            if !ch.enabled {
                continue;
            }
            let base = CMD_BASE[c];
            if port == base {
                return Some(ch.read_data(size));
            }
            if (base + 1..base + 8).contains(&port) {
                let mut v = 0;
                for i in 0..size.min(4) as u16 {
                    let p = port + i;
                    let b = if p < base + 8 { ch.read_reg(p - base) } else { 0xff };
                    v |= (b as u32) << (8 * i);
                }
                return Some(v);
            }
            if port == CTL_PORT[c] {
                return Some(ch.read_alt_status() as u32 | (u32::MAX << 8) & size_mask(size));
            }
        }
        let bm = self.bm_base?;
        if (bm..bm + 16).contains(&port) {
            let mut v = 0;
            for i in 0..size.min(4) as u16 {
                let off = (port - bm + i) as usize;
                let b = if off < 16 { self.ch[off / 8].bm_read(off % 8) } else { 0xff };
                v |= (b as u32) << (8 * i);
            }
            return Some(v);
        }
        None
    }

    /// false if the port is not ours. DMA may run during this call.
    pub fn io_write(&mut self, port: u16, size: u8, value: u32, mem: &mut dyn DmaMemory) -> bool {
        for (c, ch) in self.ch.iter_mut().enumerate() {
            if !ch.enabled {
                continue;
            }
            let base = CMD_BASE[c];
            if port == base {
                ch.write_data(size, value, mem);
                return true;
            }
            if (base + 1..base + 8).contains(&port) {
                for i in 0..size.min(4) as u16 {
                    if port + i < base + 8 {
                        ch.write_reg(port + i - base, (value >> (8 * i)) as u8, mem);
                    }
                }
                return true;
            }
            if port == CTL_PORT[c] {
                ch.write_devctl(value as u8);
                return true;
            }
        }
        let Some(bm) = self.bm_base else { return false };
        if (bm..bm + 16).contains(&port) {
            for i in 0..size.min(4) as u16 {
                let off = (port - bm + i) as usize;
                if off < 16 {
                    self.ch[off / 8].bm_write(off % 8, (value >> (8 * i)) as u8, mem);
                }
            }
            return true;
        }
        false
    }

    /// Current level of IRQ14 (channel 0) / IRQ15 (channel 1).
    pub fn irq(&self, channel: usize) -> bool {
        self.ch[channel].irq()
    }

    pub fn reset(&mut self) {
        for ch in &mut self.ch {
            ch.hard_reset();
        }
    }
}

fn size_mask(size: u8) -> u32 {
    match size {
        1 => 0xff,
        2 => 0xffff,
        _ => u32::MAX,
    }
}

//! One IDE channel: two device slots sharing a task file bus, the device
//! control register, the interrupt line and the bus master registers.

use std::io;
use std::mem;

use crate::atapi::{Cdrom, sense};
use crate::backend::BlockBackend;
use crate::busmaster::BusMaster;
use crate::DmaMemory;

pub const ST_BSY: u8 = 0x80;
pub const ST_DRDY: u8 = 0x40;
pub const ST_DSC: u8 = 0x10;
pub const ST_DRQ: u8 = 0x08;
pub const ST_ERR: u8 = 0x01;

pub const ERR_ABRT: u8 = 0x04;
pub const ERR_IDNF: u8 = 0x10;
pub const ERR_UNC: u8 = 0x40;

const CTL_NIEN: u8 = 0x02;
const CTL_SRST: u8 = 0x04;
const CTL_HOB: u8 = 0x80;

/// Bits 7 and 5 of the device register are obsolete and read back as one.
pub const DEV_ALWAYS: u8 = 0xa0;
pub const DEV_LBA: u8 = 0x40;

// DD7 pulled low as ATA-6 requires; absent-device reads follow 86Box
// src/disk/hdc_ide.c ide_status() (Miran Grca, Sarah Walker et al.).
const EMPTY_BUS: u8 = 0x7f;

#[derive(Default, Clone, Copy, snapshot::Snap)]
pub struct Taskfile {
    pub feature: u8,
    pub nsector: u8,
    pub sector: u8,
    pub lcyl: u8,
    pub hcyl: u8,
    pub hob_feature: u8,
    pub hob_nsector: u8,
    pub hob_sector: u8,
    pub hob_lcyl: u8,
    pub hob_hcyl: u8,
    pub select: u8,
}

impl Taskfile {
    fn write(&mut self, reg: u16, v: u8) {
        let (cur, hob) = match reg {
            1 => (&mut self.feature, &mut self.hob_feature),
            2 => (&mut self.nsector, &mut self.hob_nsector),
            3 => (&mut self.sector, &mut self.hob_sector),
            4 => (&mut self.lcyl, &mut self.hob_lcyl),
            5 => (&mut self.hcyl, &mut self.hob_hcyl),
            _ => {
                self.select = v | DEV_ALWAYS;
                return;
            }
        };
        *hob = *cur;
        *cur = v;
    }

    fn signature(&mut self, packet: bool) {
        *self = Taskfile { nsector: 1, sector: 1, select: DEV_ALWAYS, ..Default::default() };
        if packet {
            self.lcyl = 0x14;
            self.hcyl = 0xeb;
        }
    }
}

/// A source of bytes for a device-to-host transfer.
#[derive(snapshot::Snap)]
pub enum Stream {
    Buf(Vec<u8>),
    /// Contiguous bytes of the device's backend starting at this offset.
    Linear(u64),
    /// Synthesised raw CD sectors, see [`crate::atapi::cd_sector`].
    CdRaw { lba: u32, flags: u8, size: usize },
}

/// What happens as the host drains or fills the PIO buffer.
#[derive(snapshot::Snap)]
pub enum Pio {
    None,
    /// A single buffer (IDENTIFY); the command ends when it is drained.
    Single,
    AtaRead { lba: u64, left: u32, block: u32 },
    AtaWrite { lba: u64, left: u32, block: u32 },
    Packet,
    AtapiIn { stream: Stream, done: usize, total: usize },
    AtapiOut { done: usize, total: usize },
}

impl Default for Stream {
    fn default() -> Self {
        Stream::Buf(Vec::new())
    }
}

impl Default for DmaReq {
    fn default() -> Self {
        DmaReq::AtapiOut { total: 0 }
    }
}

impl Pio {
    fn host_writes(&self) -> bool {
        matches!(self, Pio::AtaWrite { .. } | Pio::Packet | Pio::AtapiOut { .. })
    }
}

#[derive(snapshot::Snap)]
pub enum DmaReq {
    AtaRead { lba: u64, count: u32 },
    AtaWrite { lba: u64, count: u32 },
    AtapiIn { stream: Stream, total: usize },
    AtapiOut { total: usize },
}

impl DmaReq {
    pub fn len(&self) -> usize {
        match self {
            DmaReq::AtaRead { count, .. } | DmaReq::AtaWrite { count, .. } => *count as usize * 512,
            DmaReq::AtapiIn { total, .. } | DmaReq::AtapiOut { total } => *total,
        }
    }

    pub fn to_memory(&self) -> bool {
        matches!(self, DmaReq::AtaRead { .. } | DmaReq::AtapiIn { .. })
    }
}

#[derive(snapshot::Snap)]
pub struct Disk {
    #[snap(skip)]
    pub backend: Box<dyn BlockBackend>,
    pub sectors: u64,
    pub cyls: u16,
    pub heads: u8,
    pub spt: u8,
}

pub enum Kind {
    None,
    Disk(Disk),
    Cdrom(Cdrom),
}

/// Loads in place, keeping the disk or medium attached here; a snapshot of
/// a different kind of device is refused.
impl snapshot::Snap for Kind {
    const SCHEMA: u64 = snapshot::mix(snapshot::fnv(b"Kind"), &[Disk::SCHEMA, Cdrom::SCHEMA]);

    fn save(&self, w: &mut snapshot::Writer) {
        match self {
            Kind::None => w.u32(0),
            Kind::Disk(d) => {
                w.u32(1);
                w.put(d);
            }
            Kind::Cdrom(c) => {
                w.u32(2);
                w.put(c);
            }
        }
    }

    fn load(&mut self, r: &mut snapshot::Reader<'_>) -> snapshot::Result<()> {
        match (r.u32()?, self) {
            (0, Kind::None) => Ok(()),
            (1, Kind::Disk(d)) => r.get(d),
            (2, Kind::Cdrom(c)) => r.get(c),
            (t, _) => Err(snapshot::Error::new(format!("IDE device kind {t} in the snapshot differs from this machine's"))),
        }
    }
}

#[derive(snapshot::Snap)]
pub struct Device {
    pub kind: Kind,
    pub tf: Taskfile,
    pub status: u8,
    pub error: u8,
    pub buf: Vec<u8>,
    pub pos: usize,
    pub end: usize,
    pub pio: Pio,
    pub lba48: bool,
    pub mult: u8,
    pub cur_heads: u8,
    pub cur_spt: u8,
    /// Sector count value of the last SET FEATURES "set transfer mode".
    pub xfer_mode: u8,
    pub wcache: bool,
    pub reverts: bool,
    pub serial: String,
}

pub const MAX_MULT: u8 = 16;

impl Device {
    fn new() -> Self {
        Device {
            kind: Kind::None,
            tf: Taskfile { select: DEV_ALWAYS, ..Default::default() },
            status: 0,
            error: 0,
            buf: Vec::new(),
            pos: 0,
            end: 0,
            pio: Pio::None,
            lba48: false,
            mult: MAX_MULT,
            cur_heads: 0,
            cur_spt: 0,
            xfer_mode: 0,
            wcache: true,
            reverts: false,
            serial: String::new(),
        }
    }

    pub fn present(&self) -> bool {
        !matches!(self.kind, Kind::None)
    }

    pub fn is_cdrom(&self) -> bool {
        matches!(self.kind, Kind::Cdrom(_))
    }

    pub fn backend(&mut self) -> Option<&mut Box<dyn BlockBackend>> {
        match &mut self.kind {
            Kind::None => None,
            Kind::Disk(d) => Some(&mut d.backend),
            Kind::Cdrom(c) => c.medium(),
        }
    }

    pub fn read_stream(&mut self, s: &Stream, pos: usize, out: &mut [u8]) -> io::Result<()> {
        match s {
            Stream::Buf(b) => {
                out.copy_from_slice(&b[pos..pos + out.len()]);
                Ok(())
            }
            Stream::Linear(base) => {
                let b = self.backend().ok_or(io::ErrorKind::NotFound)?;
                b.read_at(base + pos as u64, out)
            }
            Stream::CdRaw { lba, flags, size } => {
                let mut done = 0;
                while done < out.len() {
                    let p = pos + done;
                    let sector = crate::atapi::cd_sector(self, lba + (p / size) as u32, *flags)?;
                    let off = p % size;
                    let n = (size - off).min(out.len() - done);
                    out[done..done + n].copy_from_slice(&sector[off..off + n]);
                    done += n;
                }
                Ok(())
            }
        }
    }

    /// Puts the device in its post-reset state and loads the signature.
    fn soft_reset(&mut self) {
        let packet = self.is_cdrom();
        self.tf.signature(packet);
        self.status = if packet || !self.present() { 0 } else { ST_DRDY | ST_DSC };
        self.error = 0x01;
        self.pio = Pio::None;
        self.pos = 0;
        self.end = 0;
        self.lba48 = false;
        if self.reverts {
            self.power_on_defaults();
        }
        if let Kind::Cdrom(c) = &mut self.kind {
            c.sense = sense::NONE;
        }
    }

    fn power_on_defaults(&mut self) {
        self.mult = MAX_MULT;
        self.xfer_mode = 0;
        self.wcache = true;
        self.reverts = false;
        if let Kind::Disk(d) = &self.kind {
            self.cur_heads = d.heads;
            self.cur_spt = d.spt;
        }
    }

    fn hard_reset(&mut self) {
        self.power_on_defaults();
        self.soft_reset();
        if let Kind::Cdrom(c) = &mut self.kind {
            c.power_on();
        }
    }
}

#[derive(snapshot::Snap)]
pub struct Channel {
    pub dev: [Device; 2],
    pub sel: usize,
    devctl: u8,
    intrq: bool,
    line: bool,
    pub bm: BusMaster,
    pub dma: Option<(usize, DmaReq)>,
    pub enabled: bool,
}

impl Channel {
    pub fn new() -> Self {
        Channel {
            dev: [Device::new(), Device::new()],
            sel: 0,
            devctl: 0,
            intrq: false,
            line: false,
            bm: BusMaster::default(),
            dma: None,
            enabled: true,
        }
    }

    pub fn attach_disk(&mut self, slot: usize, backend: Box<dyn BlockBackend>) {
        let sectors = backend.len() / 512;
        let (cyls, heads, spt) = default_geometry(sectors);
        let d = &mut self.dev[slot];
        d.kind = Kind::Disk(Disk { backend, sectors, cyls, heads, spt });
        d.serial = format!("SLOPHD{:014}", slot + 1);
        d.hard_reset();
    }

    pub fn detach(&mut self, slot: usize) {
        self.dev[slot] = Device::new();
    }

    pub fn attach_cdrom(&mut self, slot: usize, medium: Option<Box<dyn BlockBackend>>) {
        let d = &mut self.dev[slot];
        if let Kind::Cdrom(c) = &mut d.kind {
            c.change_medium(medium);
            return;
        }
        d.kind = Kind::Cdrom(Cdrom::new(medium));
        d.serial = format!("SLOPCD{:014}", slot + 1);
        d.hard_reset();
    }

    pub fn hard_reset(&mut self) {
        for d in &mut self.dev {
            d.hard_reset();
        }
        self.sel = 0;
        self.devctl = 0;
        self.intrq = false;
        self.line = false;
        self.bm = BusMaster::default();
        self.dma = None;
    }

    fn empty(&self) -> bool {
        !self.dev[0].present() && !self.dev[1].present()
    }

    pub fn irq(&self) -> bool {
        self.line
    }

    pub fn raise_irq(&mut self) {
        self.intrq = true;
        self.update_line();
    }

    fn update_line(&mut self) {
        let level = self.intrq && self.devctl & CTL_NIEN == 0;
        if level && !self.line {
            self.bm.irq_edge();
        }
        self.line = level;
    }

    fn status_of_selected(&self) -> u8 {
        if self.empty() {
            EMPTY_BUS
        } else if !self.dev[self.sel].present() {
            0
        } else {
            self.dev[self.sel].status
        }
    }

    pub fn read_alt_status(&self) -> u8 {
        self.status_of_selected()
    }

    pub fn read_reg(&mut self, reg: u16) -> u8 {
        if reg == 7 {
            self.intrq = false;
            self.update_line();
            return self.status_of_selected();
        }
        if self.empty() {
            return EMPTY_BUS;
        }
        let hob = self.devctl & CTL_HOB != 0;
        let d = &self.dev[self.sel];
        let tf = &d.tf;
        match reg {
            1 if !d.present() => 0,
            1 => d.error,
            2 if hob => tf.hob_nsector,
            2 => tf.nsector,
            3 if hob => tf.hob_sector,
            3 => tf.sector,
            4 if hob => tf.hob_lcyl,
            4 => tf.lcyl,
            5 if hob => tf.hob_hcyl,
            5 => tf.hcyl,
            _ => tf.select,
        }
    }

    pub fn write_reg(&mut self, reg: u16, v: u8, mem: &mut dyn DmaMemory) {
        if reg == 7 {
            self.exec_command(v, mem);
            return;
        }
        if self.dev[self.sel].status & (ST_BSY | ST_DRQ) != 0 {
            return;
        }
        self.devctl &= !CTL_HOB;
        for d in &mut self.dev {
            d.tf.write(reg, v);
        }
        if reg == 6 {
            self.sel = (v as usize >> 4) & 1;
        }
    }

    pub fn write_devctl(&mut self, v: u8) {
        let old = self.devctl;
        self.devctl = v;
        if v & CTL_SRST != 0 && old & CTL_SRST == 0 {
            self.dma = None;
            self.intrq = false;
            for d in self.dev.iter_mut().filter(|d| d.present()) {
                d.pio = Pio::None;
                d.status = ST_BSY;
            }
        } else if v & CTL_SRST == 0 && old & CTL_SRST != 0 {
            for d in &mut self.dev {
                d.soft_reset();
            }
            self.sel = 0;
        }
        self.update_line();
    }

    pub fn read_data(&mut self, size: u8) -> u32 {
        if self.empty() {
            return u32::from_ne_bytes([EMPTY_BUS; 4]) & crate::size_mask(size);
        }
        let d = &mut self.dev[self.sel];
        if d.status & ST_DRQ == 0 || d.pio.host_writes() {
            return crate::size_mask(size);
        }
        // An 8-bit access still moves a whole word, as on real drives.
        let n = (size as usize).max(2);
        let mut v = 0u32;
        for i in 0..n {
            let b = d.buf.get(d.pos).copied().unwrap_or(0);
            d.pos += 1;
            if i < size as usize {
                v |= (b as u32) << (8 * i);
            }
        }
        if d.pos >= d.end {
            self.pio_block_done(&mut NoMem);
        }
        v
    }

    pub fn write_data(&mut self, size: u8, v: u32, mem: &mut dyn DmaMemory) {
        let d = &mut self.dev[self.sel];
        if !d.present() || d.status & ST_DRQ == 0 || !d.pio.host_writes() {
            return;
        }
        let n = (size as usize).max(2);
        for i in 0..n {
            if d.pos < d.buf.len() {
                d.buf[d.pos] = if i < 4 { (v >> (8 * i)) as u8 } else { 0 };
            }
            d.pos += 1;
        }
        if d.pos >= d.end {
            self.pio_block_done(mem);
        }
    }

    /// Starts a PIO data block of `len` bytes. `buf` must already hold the
    /// data for device-to-host transfers.
    pub fn start_pio(&mut self, len: usize, irq: bool) {
        let d = &mut self.dev[self.sel];
        d.buf.resize(len.next_multiple_of(2).max(d.buf.len()), 0);
        d.pos = 0;
        d.end = len.next_multiple_of(2);
        d.status = ST_DRDY | ST_DSC | ST_DRQ;
        if irq {
            self.raise_irq();
        }
    }

    fn pio_block_done(&mut self, mem: &mut dyn DmaMemory) {
        let d = &mut self.dev[self.sel];
        d.status &= !ST_DRQ;
        match mem::replace(&mut d.pio, Pio::None) {
            Pio::None => {}
            Pio::Single => {
                d.status = ST_DRDY | ST_DSC;
            }
            Pio::AtaRead { lba, left, block } => self.ata_read_block(lba, left, block),
            Pio::AtaWrite { lba, left, block } => self.ata_write_block_done(lba, left, block),
            Pio::Packet => {
                let mut cdb = [0u8; 12];
                cdb.copy_from_slice(&self.dev[self.sel].buf[..12]);
                self.atapi_packet(&cdb, mem);
            }
            Pio::AtapiIn { stream, done, total } => self.atapi_pio_in(stream, done, total),
            Pio::AtapiOut { done, total } => self.atapi_pio_out(done, total),
        }
    }

    /// Ends a command without error and raises INTRQ.
    pub fn complete(&mut self, extra: u8) {
        let d = &mut self.dev[self.sel];
        d.status = ST_DRDY | extra;
        d.error = 0;
        d.pio = Pio::None;
        self.raise_irq();
    }

    pub fn abort(&mut self) {
        self.fail(ERR_ABRT);
    }

    pub fn fail(&mut self, err: u8) {
        let d = &mut self.dev[self.sel];
        d.status = ST_DRDY | ST_ERR;
        d.error = err;
        d.pio = Pio::None;
        self.raise_irq();
    }

    fn exec_command(&mut self, cmd: u8, mem: &mut dyn DmaMemory) {
        let d = &self.dev[self.sel];
        if !d.present() {
            return;
        }
        if d.status & (ST_BSY | ST_DRQ) != 0 && !(cmd == 0x08 && d.is_cdrom()) {
            return;
        }
        self.devctl &= !CTL_HOB;
        self.intrq = false;
        self.update_line();
        if self.dma.as_ref().is_some_and(|(i, _)| *i == self.sel) {
            self.dma = None;
        }
        self.dev[self.sel].error = 0;
        if cmd == 0x90 {
            self.execute_diagnostic();
            return;
        }
        if self.dev[self.sel].is_cdrom() {
            self.atapi_ata_command(cmd);
        } else {
            self.ata_command(cmd, mem);
        }
    }

    /// EXECUTE DEVICE DIAGNOSTIC runs on both devices whichever is selected.
    fn execute_diagnostic(&mut self) {
        for d in &mut self.dev {
            let reverts = mem::replace(&mut d.reverts, false);
            d.soft_reset();
            d.reverts = reverts;
        }
        self.sel = 0;
        if self.dev[0].present() {
            self.raise_irq();
        }
    }

    /// IDENTIFY word 93 (hardware reset result) for device `slot`.
    pub fn reset_result(&self, slot: usize) -> u16 {
        const CBLID: u16 = 1 << 13;
        if slot == 0 {
            let base = 0x4000 | CBLID | 0x0001 | 0x0002 | 0x0008;
            if self.dev[1].present() { base | 0x0010 | 0x0020 } else { base | 0x0040 }
        } else {
            0x4000 | CBLID | 0x0100 | 0x0200 | 0x0800
        }
    }

    /// Queues a DMA request for the selected device and runs it at once if
    /// the bus master engine is already started.
    pub fn start_dma(&mut self, req: DmaReq, mem: &mut dyn DmaMemory) {
        self.dev[self.sel].status = ST_DRDY | ST_DSC | ST_DRQ;
        self.dma = Some((self.sel, req));
        if self.bm.started() {
            self.run_dma(mem);
        }
    }
}

/// Stands in for guest memory on data port reads, which never start DMA.
struct NoMem;

impl DmaMemory for NoMem {
    fn read_phys(&mut self, _: u32, buf: &mut [u8]) {
        buf.fill(0xff);
    }
    fn write_phys(&mut self, _: u32, _: &[u8]) {}
}

/// Default CHS translation: 16 heads and 63 sectors per track, capped at
/// 16383 cylinders as ATA-6 prescribes for IDENTIFY word 1.
fn default_geometry(sectors: u64) -> (u16, u8, u8) {
    if sectors < 16 * 63 {
        let spt = sectors.clamp(1, 63) as u8;
        let cyls = (sectors / spt as u64).max(1);
        return (cyls as u16, 1, spt);
    }
    let cyls = (sectors / (16 * 63)).min(16383);
    (cyls as u16, 16, 63)
}

#[cfg(test)]
mod tests {
    use super::default_geometry;

    #[test]
    fn geometry_caps_at_16383_cylinders() {
        assert_eq!(default_geometry(100_000_000), (16383, 16, 63));
        assert_eq!(default_geometry(16 * 63 * 100), (100, 16, 63));
        assert_eq!(default_geometry(10), (1, 1, 10));
    }
}

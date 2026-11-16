//! ATA hard disk command set.

use crate::DmaMemory;
use crate::busmaster::DmaOutcome;
use crate::channel::*;

/// Where a read or write command starts and how many sectors it covers.
struct Range {
    lba: u64,
    count: u32,
}

impl Channel {
    fn disk(&mut self) -> &mut Disk {
        match &mut self.dev[self.sel].kind {
            Kind::Disk(d) => d,
            _ => unreachable!(),
        }
    }

    pub fn ata_command(&mut self, cmd: u8, mem: &mut dyn DmaMemory) {
        match cmd {
            0x10..=0x1f => self.complete(ST_DSC),
            0x20 | 0x21 => self.ata_pio_read(false, 1),
            0x24 => self.ata_pio_read(true, 1),
            0xc4 | 0x29 => self.ata_multiple(cmd == 0x29, false),
            0x30 | 0x31 | 0x3c => self.ata_pio_write(false, 1),
            0x34 => self.ata_pio_write(true, 1),
            0xc5 | 0x39 => self.ata_multiple(cmd == 0x39, true),
            0xc8 | 0xc9 => self.ata_dma(false, false, mem),
            0x25 => self.ata_dma(true, false, mem),
            0xca | 0xcb => self.ata_dma(false, true, mem),
            0x35 => self.ata_dma(true, true, mem),
            0x40 | 0x41 => self.ata_verify(false),
            0x42 => self.ata_verify(true),
            0x70..=0x7f => self.ata_seek(),
            0x91 => self.ata_init_params(),
            0xc6 => self.ata_set_multiple(),
            0xe0..=0xe3 | 0xe6 | 0x94..=0x97 | 0x99 => self.complete(ST_DSC),
            0xe5 | 0x98 => {
                self.dev[self.sel].tf.nsector = 0xff;
                self.complete(ST_DSC);
            }
            0xe7 | 0xea => self.ata_flush(),
            0xec => self.ata_identify(),
            0xef => self.set_features(),
            0xf8 => self.ata_native_max(false),
            0x27 => self.ata_native_max(true),
            _ => self.abort(),
        }
    }

    fn command_range(&mut self, lba48: bool) -> Option<Range> {
        let d = &mut self.dev[self.sel];
        d.lba48 = lba48;
        let tf = d.tf;
        let (heads, spt) = (d.cur_heads as u64, d.cur_spt as u64);
        if lba48 {
            let count = match (tf.hob_nsector as u32) << 8 | tf.nsector as u32 {
                0 => 0x1_0000,
                n => n,
            };
            let lba = (tf.hob_hcyl as u64) << 40
                | (tf.hob_lcyl as u64) << 32
                | (tf.hob_sector as u64) << 24
                | (tf.hcyl as u64) << 16
                | (tf.lcyl as u64) << 8
                | tf.sector as u64;
            return Some(Range { lba, count });
        }
        let count = match tf.nsector {
            0 => 256,
            n => n as u32,
        };
        if tf.select & DEV_LBA != 0 {
            let lba = ((tf.select & 0xf) as u64) << 24 | (tf.hcyl as u64) << 16 | (tf.lcyl as u64) << 8 | tf.sector as u64;
            return Some(Range { lba, count });
        }
        let cyl = (tf.hcyl as u64) << 8 | tf.lcyl as u64;
        let head = (tf.select & 0xf) as u64;
        let sector = tf.sector as u64;
        if sector == 0 || sector > spt || head >= heads {
            return None;
        }
        Some(Range { lba: (cyl * heads + head) * spt + sector - 1, count })
    }

    /// Decodes the command's address and checks it against the disk size,
    /// failing the command with IDNF when it is out of range.
    fn checked_range(&mut self, lba48: bool) -> Option<Range> {
        let sectors = self.disk().sectors;
        match self.command_range(lba48) {
            Some(r) if r.lba + r.count as u64 <= sectors => Some(r),
            _ => {
                self.fail(ERR_IDNF);
                None
            }
        }
    }

    /// Writes `lba` back into the task file, as drives do after a transfer.
    fn set_sector(&mut self, lba: u64) {
        let d = &mut self.dev[self.sel];
        let tf = &mut d.tf;
        if d.lba48 {
            tf.sector = lba as u8;
            tf.lcyl = (lba >> 8) as u8;
            tf.hcyl = (lba >> 16) as u8;
            tf.hob_sector = (lba >> 24) as u8;
            tf.hob_lcyl = (lba >> 32) as u8;
            tf.hob_hcyl = (lba >> 40) as u8;
        } else if tf.select & DEV_LBA != 0 {
            tf.sector = lba as u8;
            tf.lcyl = (lba >> 8) as u8;
            tf.hcyl = (lba >> 16) as u8;
            tf.select = (tf.select & 0xf0) | ((lba >> 24) & 0xf) as u8;
        } else if d.cur_heads != 0 && d.cur_spt != 0 {
            let track = lba / d.cur_spt as u64;
            let cyl = track / d.cur_heads as u64;
            tf.sector = (lba % d.cur_spt as u64 + 1) as u8;
            tf.lcyl = cyl as u8;
            tf.hcyl = (cyl >> 8) as u8;
            tf.select = (tf.select & 0xf0) | (track % d.cur_heads as u64) as u8;
        }
    }

    fn ata_multiple(&mut self, lba48: bool, write: bool) {
        let mult = self.dev[self.sel].mult as u32;
        if mult == 0 {
            self.abort();
        } else if write {
            self.ata_pio_write(lba48, mult);
        } else {
            self.ata_pio_read(lba48, mult);
        }
    }

    fn ata_pio_read(&mut self, lba48: bool, block: u32) {
        if let Some(r) = self.checked_range(lba48) {
            self.ata_read_block(r.lba, r.count, block);
        }
    }

    /// Loads the next DRQ block of a PIO read, or ends the command when
    /// nothing is left.
    pub fn ata_read_block(&mut self, lba: u64, left: u32, block: u32) {
        if left == 0 {
            self.dev[self.sel].status = ST_DRDY | ST_DSC;
            return;
        }
        let n = left.min(block);
        let d = &mut self.dev[self.sel];
        d.buf.resize(n as usize * 512, 0);
        let mut buf = std::mem::take(&mut d.buf);
        let res = d.backend().unwrap().read_at(lba * 512, &mut buf);
        self.dev[self.sel].buf = buf;
        if res.is_err() {
            self.set_sector(lba);
            self.fail(ERR_UNC);
            return;
        }
        self.set_sector(lba + n as u64 - 1);
        self.dev[self.sel].pio = Pio::AtaRead { lba: lba + n as u64, left: left - n, block };
        self.start_pio(n as usize * 512, true);
    }

    fn ata_pio_write(&mut self, lba48: bool, block: u32) {
        if let Some(r) = self.checked_range(lba48) {
            let n = r.count.min(block);
            self.dev[self.sel].pio = Pio::AtaWrite { lba: r.lba, left: r.count, block };
            self.start_pio(n as usize * 512, false);
        }
    }

    /// Commits a filled PIO write block and asks for the next one.
    pub fn ata_write_block_done(&mut self, lba: u64, left: u32, block: u32) {
        let n = left.min(block);
        let d = &mut self.dev[self.sel];
        let buf = std::mem::take(&mut d.buf);
        let res = d.backend().unwrap().write_at(lba * 512, &buf[..n as usize * 512]);
        self.dev[self.sel].buf = buf;
        if res.is_err() {
            self.set_sector(lba);
            self.abort();
            return;
        }
        self.set_sector(lba + n as u64 - 1);
        let left = left - n;
        if left == 0 {
            self.complete(ST_DSC);
            return;
        }
        let next = left.min(block);
        self.dev[self.sel].pio = Pio::AtaWrite { lba: lba + n as u64, left, block };
        self.start_pio(next as usize * 512, true);
    }

    fn ata_dma(&mut self, lba48: bool, write: bool, mem: &mut dyn DmaMemory) {
        let Some(Range { lba, count }) = self.checked_range(lba48) else { return };
        if write && self.disk().backend.read_only() {
            self.abort();
            return;
        }
        let req = if write { DmaReq::AtaWrite { lba, count } } else { DmaReq::AtaRead { lba, count } };
        self.start_dma(req, mem);
    }

    pub fn ata_dma_finished(&mut self, lba: u64, done: usize, outcome: DmaOutcome) {
        let sectors = (done / 512) as u64;
        self.set_sector(lba + sectors.saturating_sub(1));
        match outcome {
            DmaOutcome::Done => self.complete(ST_DSC),
            DmaOutcome::ShortTable => self.abort(),
            DmaOutcome::Io => self.fail(ERR_UNC),
        }
    }

    fn ata_verify(&mut self, lba48: bool) {
        if let Some(r) = self.checked_range(lba48) {
            self.set_sector(r.lba + r.count as u64 - 1);
            self.complete(ST_DSC);
        }
    }

    fn ata_seek(&mut self) {
        let sectors = self.disk().sectors;
        match self.command_range(false) {
            Some(r) if r.lba < sectors => self.complete(ST_DSC),
            _ => self.fail(ERR_IDNF),
        }
    }

    fn ata_init_params(&mut self) {
        let d = &mut self.dev[self.sel];
        if d.tf.nsector == 0 {
            self.abort();
            return;
        }
        d.cur_heads = (d.tf.select & 0xf) + 1;
        d.cur_spt = d.tf.nsector;
        self.complete(ST_DSC);
    }

    fn ata_set_multiple(&mut self) {
        let n = self.dev[self.sel].tf.nsector;
        if n > MAX_MULT || !(n == 0 || n.is_power_of_two()) {
            self.abort();
            return;
        }
        self.dev[self.sel].mult = n;
        self.complete(ST_DSC);
    }

    fn ata_flush(&mut self) {
        if self.disk().backend.flush().is_err() {
            self.abort();
        } else {
            self.complete(ST_DSC);
        }
    }

    fn ata_native_max(&mut self, lba48: bool) {
        let max = self.disk().sectors.saturating_sub(1);
        let d = &mut self.dev[self.sel];
        d.lba48 = lba48;
        if !lba48 {
            d.tf.select |= DEV_LBA;
        }
        self.set_sector(if lba48 { max } else { max.min(0x0fff_ffff) });
        self.complete(ST_DSC);
    }

    /// SET FEATURES, shared with ATAPI devices.
    pub fn set_features(&mut self) {
        let d = &mut self.dev[self.sel];
        match d.tf.feature {
            0x03 => {
                let mode = d.tf.nsector;
                let ok = match mode >> 3 {
                    0 => mode <= 1,
                    1 => mode & 7 <= 4,
                    4 => mode & 7 <= 2,
                    8 => mode & 7 <= 5,
                    _ => false,
                };
                if !ok {
                    self.abort();
                    return;
                }
                d.xfer_mode = mode;
            }
            0x02 => d.wcache = true,
            0x82 => {
                d.wcache = false;
                if let Some(b) = d.backend() {
                    let _ = b.flush();
                }
            }
            0xcc => d.reverts = true,
            0x66 => d.reverts = false,
            0x55 | 0xaa | 0x05 | 0x85 | 0x42 | 0xc2 => {}
            _ => {
                self.abort();
                return;
            }
        }
        self.complete(ST_DSC);
    }

    fn ata_identify(&mut self) {
        let words = self.disk_identify();
        self.send_identify(&words);
    }

    pub fn send_identify(&mut self, words: &[u16; 256]) {
        let mut words = *words;
        finish_identify(&mut words);
        let d = &mut self.dev[self.sel];
        d.buf.clear();
        for w in words {
            d.buf.extend_from_slice(&w.to_le_bytes());
        }
        d.pio = Pio::Single;
        self.start_pio(512, true);
    }

    // Field values follow QEMU hw/ide/core.c ide_identify() (Fabrice Bellard
    // and the QEMU contributors).
    fn disk_identify(&mut self) -> [u16; 256] {
        let slot = self.sel;
        let reset_result = self.reset_result(slot);
        let d = &self.dev[slot];
        let Kind::Disk(disk) = &d.kind else { unreachable!() };
        let mut w = [0u16; 256];
        w[0] = 0x0040;
        w[1] = disk.cyls;
        w[3] = disk.heads as u16;
        w[4] = 512 * disk.spt as u16;
        w[5] = 512;
        w[6] = disk.spt as u16;
        put_string(&mut w[10..20], &d.serial);
        w[20] = 3;
        w[21] = 512;
        w[22] = 4;
        put_string(&mut w[23..27], "1.0");
        put_string(&mut w[27..47], "86slopper HARDDISK");
        w[47] = 0x8000 | MAX_MULT as u16;
        w[48] = 1;
        w[49] = (1 << 11) | (1 << 9) | (1 << 8);
        w[50] = 0x4000;
        w[51] = 0x200;
        w[52] = 0x200;
        w[53] = 0x7;
        let (h, s) = (d.cur_heads as u64, d.cur_spt as u64);
        let cur_cyls = disk.sectors.checked_div(h * s).unwrap_or(0).min(0xffff);
        let cur_cap = (cur_cyls * h * s).min(0xffff_ffff) as u32;
        w[54] = cur_cyls as u16;
        w[55] = h as u16;
        w[56] = s as u16;
        w[57] = cur_cap as u16;
        w[58] = (cur_cap >> 16) as u16;
        if d.mult != 0 {
            w[59] = 0x100 | d.mult as u16;
        }
        let lba28 = disk.sectors.min(0x0fff_ffff) as u32;
        w[60] = lba28 as u16;
        w[61] = (lba28 >> 16) as u16;
        dma_mode_words(&mut w, d.xfer_mode);
        w[64] = 0x03;
        w[65] = 120;
        w[66] = 120;
        w[67] = 120;
        w[68] = 120;
        w[80] = 0x007e;
        w[82] = (1 << 14) | (1 << 6) | (1 << 5);
        w[83] = (1 << 14) | (1 << 13) | (1 << 12) | (1 << 10);
        w[84] = 1 << 14;
        w[85] = (1 << 14) | (1 << 6) | if d.wcache { 1 << 5 } else { 0 };
        w[86] = (1 << 13) | (1 << 12) | (1 << 10);
        w[87] = 1 << 14;
        w[93] = reset_result;
        for i in 0..4 {
            w[100 + i] = (disk.sectors >> (16 * i)) as u16;
        }
        w
    }
}

/// Fills words 62, 63 and 88 from the transfer mode chosen with SET
/// FEATURES: MWDMA 0-2 and UDMA 0-5 are supported, the selected one flagged.
pub fn dma_mode_words(w: &mut [u16; 256], xfer_mode: u8) {
    let n = xfer_mode & 7;
    w[62] = 0x0007;
    w[63] = 0x0007;
    w[88] = 0x003f;
    match xfer_mode >> 3 {
        4 => w[63] |= 0x100 << n,
        8 => w[88] |= 0x100 << n,
        _ => {}
    }
}

/// ATA strings hold two characters per word, the first in the high byte.
pub fn put_string(words: &mut [u16], s: &str) {
    let mut bytes = s.bytes();
    for w in words {
        let hi = bytes.next().unwrap_or(b' ') as u16;
        let lo = bytes.next().unwrap_or(b' ') as u16;
        *w = hi << 8 | lo;
    }
}

/// Adds the integrity word (255): signature A5h and a checksum that makes
/// the 512 bytes sum to zero.
fn finish_identify(w: &mut [u16; 256]) {
    w[255] = 0x00a5;
    let sum = w.iter().fold(0u8, |s, &x| s.wrapping_add(x as u8).wrapping_add((x >> 8) as u8));
    w[255] |= (sum.wrapping_neg() as u16) << 8;
}

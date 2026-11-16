//! ATAPI CD-ROM: the PACKET protocol and the SCSI/MMC commands Windows XP
//! and SeaBIOS issue.

use std::io;

use crate::DmaMemory;
use crate::ata::{dma_mode_words, put_string};
use crate::backend::BlockBackend;
use crate::busmaster::DmaOutcome;
use crate::channel::*;

const SECTOR: usize = 2048;
/// Larger media are reported as DVD-ROM (80 minutes of CD audio frames).
const CD_MAX_SECTORS: u64 = 80 * 60 * 75;

const REASON_COD: u8 = 0x01;
const REASON_IO: u8 = 0x02;

pub mod sense {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Sense {
        pub key: u8,
        pub asc: u8,
        pub ascq: u8,
    }

    const fn s(key: u8, asc: u8, ascq: u8) -> Sense {
        Sense { key, asc, ascq }
    }

    pub const NONE: Sense = s(0, 0, 0);
    pub const NO_MEDIUM_TRAY_CLOSED: Sense = s(2, 0x3a, 0x01);
    pub const NO_MEDIUM_TRAY_OPEN: Sense = s(2, 0x3a, 0x02);
    pub const REMOVAL_PREVENTED: Sense = s(2, 0x53, 0x02);
    pub const INVALID_OPCODE: Sense = s(5, 0x20, 0x00);
    pub const LBA_OUT_OF_RANGE: Sense = s(5, 0x21, 0x00);
    pub const INVALID_FIELD: Sense = s(5, 0x24, 0x00);
    pub const SAVING_UNSUPPORTED: Sense = s(5, 0x39, 0x00);
    pub const INCOMPATIBLE_FORMAT: Sense = s(5, 0x30, 0x02);
    pub const ILLEGAL_MODE: Sense = s(5, 0x64, 0x00);
    pub const MEDIUM_CHANGED: Sense = s(6, 0x28, 0x00);
    pub const POWER_ON_RESET: Sense = s(6, 0x29, 0x00);
    pub const READ_ERROR: Sense = s(3, 0x11, 0x00);
}

use sense::Sense;

pub struct Cdrom {
    medium: Option<Box<dyn BlockBackend>>,
    tray_open: bool,
    locked: bool,
    /// Sense data of the last command, returned by REQUEST SENSE.
    pub sense: Sense,
    unit_attention: Option<Sense>,
    /// Pending GET EVENT STATUS NOTIFICATION media event code.
    media_event: u8,
    packet_dma: bool,
    bcl: u16,
}

impl Cdrom {
    pub fn new(medium: Option<Box<dyn BlockBackend>>) -> Self {
        Cdrom {
            medium,
            tray_open: false,
            locked: false,
            sense: sense::NONE,
            unit_attention: None,
            media_event: 0,
            packet_dma: false,
            bcl: 0,
        }
    }

    pub fn medium(&mut self) -> Option<&mut Box<dyn BlockBackend>> {
        if self.tray_open { None } else { self.medium.as_mut() }
    }

    fn present(&self) -> bool {
        !self.tray_open && self.medium.is_some()
    }

    fn sectors(&self) -> u64 {
        match &self.medium {
            Some(m) if !self.tray_open => m.len() / SECTOR as u64,
            _ => 0,
        }
    }

    fn is_dvd(&self) -> bool {
        self.sectors() > CD_MAX_SECTORS
    }

    pub fn power_on(&mut self) {
        self.locked = false;
        self.tray_open = false;
        self.unit_attention = self.medium.is_some().then_some(sense::POWER_ON_RESET);
        self.media_event = 0;
    }

    pub fn change_medium(&mut self, medium: Option<Box<dyn BlockBackend>>) {
        self.tray_open = false;
        self.media_event = if medium.is_some() { 2 } else { 3 };
        if medium.is_some() {
            self.unit_attention = Some(sense::MEDIUM_CHANGED);
        }
        self.medium = medium;
    }

    fn not_ready(&self) -> Sense {
        if self.tray_open { sense::NO_MEDIUM_TRAY_OPEN } else { sense::NO_MEDIUM_TRAY_CLOSED }
    }
}

/// Builds one raw sector of a Mode 1 data track for READ CD with the given
/// byte 9 flags (sync, header codes, user data, EDC/ECC).
pub fn cd_sector(dev: &mut Device, lba: u32, flags: u8) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(2352);
    if flags & 0x80 != 0 {
        out.push(0);
        out.extend_from_slice(&[0xff; 10]);
        out.push(0);
    }
    if flags & 0x20 != 0 {
        let msf = lba_to_msf(lba);
        out.extend(msf.iter().map(|&v| ((v / 10) << 4) | (v % 10)));
        out.push(0x01);
    }
    if flags & 0x10 != 0 {
        let start = out.len();
        out.resize(start + SECTOR, 0);
        let b = dev.backend().ok_or(io::ErrorKind::NotFound)?;
        b.read_at(lba as u64 * SECTOR as u64, &mut out[start..])?;
    }
    if flags & 0x08 != 0 {
        out.resize(out.len() + 288, 0);
    }
    Ok(out)
}

fn cd_sector_size(flags: u8) -> usize {
    let mut n = 0;
    if flags & 0x80 != 0 {
        n += 12;
    }
    if flags & 0x20 != 0 {
        n += 4;
    }
    if flags & 0x10 != 0 {
        n += SECTOR;
    }
    if flags & 0x08 != 0 {
        n += 288;
    }
    n
}

fn lba_to_msf(lba: u32) -> [u8; 3] {
    let f = lba + 150;
    [(f / 75 / 60) as u8, (f / 75 % 60) as u8, (f % 75) as u8]
}

/// Writes an address field of TOC-like data: 4 bytes, LBA or 0/M/S/F.
fn put_addr(out: &mut Vec<u8>, lba: u32, msf: bool) {
    if msf {
        out.push(0);
        out.extend_from_slice(&lba_to_msf(lba));
    } else {
        out.extend_from_slice(&lba.to_be_bytes());
    }
}

fn be16(b: &[u8]) -> usize {
    u16::from_be_bytes([b[0], b[1]]) as usize
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Channel {
    fn cd(&mut self) -> &mut Cdrom {
        match &mut self.dev[self.sel].kind {
            Kind::Cdrom(c) => c,
            _ => unreachable!(),
        }
    }

    /// ATA-level commands sent to the packet device.
    pub fn atapi_ata_command(&mut self, cmd: u8) {
        match cmd {
            0xa0 => self.packet_command(),
            0xa1 => {
                let w = self.packet_identify();
                self.send_identify(&w);
            }
            0x08 => {
                let d = &mut self.dev[self.sel];
                d.tf.signature_packet();
                d.status = 0;
                d.error = 0x01;
                d.pio = Pio::None;
            }
            0xef => self.set_features(),
            0xe5 | 0x98 => {
                self.dev[self.sel].tf.nsector = 0xff;
                self.complete(ST_DSC);
            }
            0xe0..=0xe3 | 0xe6 | 0xe7 | 0x94..=0x97 | 0x99 => self.complete(ST_DSC),
            0xec | 0x20 | 0x21 => {
                self.dev[self.sel].tf.signature_packet();
                self.abort();
            }
            _ => self.abort(),
        }
    }

    fn packet_command(&mut self) {
        let tf = self.dev[self.sel].tf;
        if tf.feature & 0x02 != 0 {
            self.abort();
            return;
        }
        let c = self.cd();
        c.packet_dma = tf.feature & 0x01 != 0;
        c.bcl = (tf.hcyl as u16) << 8 | tf.lcyl as u16;
        let d = &mut self.dev[self.sel];
        d.tf.nsector = (d.tf.nsector & !7) | REASON_COD;
        d.pio = Pio::Packet;
        d.buf.resize(12, 0);
        self.start_pio(12, false);
        self.dev[self.sel].status = ST_DRDY | ST_DRQ;
    }

    // Field values follow QEMU hw/ide/core.c ide_atapi_identify() (Fabrice
    // Bellard and the QEMU contributors).
    fn packet_identify(&mut self) -> [u16; 256] {
        let slot = self.sel;
        let reset_result = self.reset_result(slot);
        let d = &self.dev[slot];
        let mut w = [0u16; 256];
        w[0] = (2 << 14) | (5 << 8) | (1 << 7) | (2 << 5);
        put_string(&mut w[10..20], &d.serial);
        w[20] = 3;
        w[21] = 512;
        w[22] = 4;
        put_string(&mut w[23..27], "1.0");
        put_string(&mut w[27..47], "86slopper DVD-ROM");
        w[48] = 1;
        w[49] = (1 << 9) | (1 << 8);
        w[53] = 7;
        dma_mode_words(&mut w, d.xfer_mode);
        w[64] = 3;
        w[65] = 0xb4;
        w[66] = 0xb4;
        w[67] = 0x12c;
        w[68] = 0xb4;
        w[71] = 30;
        w[72] = 30;
        w[80] = 0x007e;
        w[82] = (1 << 14) | (1 << 9) | (1 << 4);
        w[85] = (1 << 14) | (1 << 9) | (1 << 4);
        w[93] = reset_result;
        w
    }

    pub fn atapi_ok(&mut self) {
        let d = &mut self.dev[self.sel];
        d.tf.nsector = (d.tf.nsector & !7) | REASON_IO | REASON_COD;
        self.complete(ST_DSC);
    }

    fn atapi_error(&mut self, s: Sense) {
        self.cd().sense = s;
        let d = &mut self.dev[self.sel];
        d.tf.nsector = (d.tf.nsector & !7) | REASON_IO | REASON_COD;
        d.status = ST_DRDY | ST_ERR;
        d.error = s.key << 4 | if s.key == 5 { ERR_ABRT } else { 0 };
        d.pio = Pio::None;
        self.raise_irq();
    }

    /// Byte count limit for one DRQ block: even unless the whole remainder
    /// fits, and 0xFFFF counts as 0xFFFE.
    fn block_len(&mut self, left: usize) -> usize {
        let bcl = self.cd().bcl as usize;
        if left <= bcl && bcl != 0xffff { left } else { left.min(bcl & !1) }
    }

    fn reply(&mut self, mut data: Vec<u8>, alloc: usize, mem: &mut dyn DmaMemory) {
        data.truncate(alloc);
        let total = data.len();
        self.data_in(Stream::Buf(data), total, mem);
    }

    fn data_in(&mut self, stream: Stream, total: usize, mem: &mut dyn DmaMemory) {
        if total == 0 {
            self.atapi_ok();
        } else if self.cd().packet_dma {
            self.start_dma(DmaReq::AtapiIn { stream, total }, mem);
        } else if self.cd().bcl == 0 {
            self.abort();
        } else {
            self.atapi_pio_in(stream, 0, total);
        }
    }

    fn data_out(&mut self, total: usize, mem: &mut dyn DmaMemory) {
        if total == 0 {
            self.atapi_ok();
        } else if self.cd().packet_dma {
            self.start_dma(DmaReq::AtapiOut { total }, mem);
        } else if self.cd().bcl == 0 {
            self.abort();
        } else {
            self.atapi_pio_out(0, total);
        }
    }

    /// Presents the next data-in DRQ block, or completes the command.
    pub fn atapi_pio_in(&mut self, stream: Stream, done: usize, total: usize) {
        if done >= total {
            self.atapi_ok();
            return;
        }
        let n = self.block_len(total - done);
        let d = &mut self.dev[self.sel];
        let mut buf = std::mem::take(&mut d.buf);
        buf.clear();
        buf.resize(n.next_multiple_of(2), 0);
        let res = d.read_stream(&stream, done, &mut buf[..n]);
        d.buf = buf;
        if res.is_err() {
            self.atapi_error(sense::READ_ERROR);
            return;
        }
        d.tf.lcyl = n as u8;
        d.tf.hcyl = (n >> 8) as u8;
        d.tf.nsector = (d.tf.nsector & !7) | REASON_IO;
        d.pio = Pio::AtapiIn { stream, done: done + n, total };
        self.start_pio(n, true);
    }

    pub fn atapi_pio_out(&mut self, done: usize, total: usize) {
        if done >= total {
            self.atapi_ok();
            return;
        }
        let n = self.block_len(total - done);
        let d = &mut self.dev[self.sel];
        d.buf.clear();
        d.buf.resize(n.next_multiple_of(2), 0);
        d.tf.lcyl = n as u8;
        d.tf.hcyl = (n >> 8) as u8;
        d.tf.nsector &= !7;
        d.pio = Pio::AtapiOut { done: done + n, total };
        self.start_pio(n, true);
    }

    pub fn atapi_dma_finished(&mut self, outcome: DmaOutcome) {
        match outcome {
            DmaOutcome::Done => self.atapi_ok(),
            DmaOutcome::ShortTable => self.abort(),
            DmaOutcome::Io => self.atapi_error(sense::READ_ERROR),
        }
    }

    // Unit attention and medium checks follow the flags of QEMU
    // hw/ide/atapi.c atapi_cmd_table (Fabrice Bellard et al.).
    pub fn atapi_packet(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let op = cdb[0];
        let allow_ua = matches!(op, 0x03 | 0x12 | 0x46 | 0x4a);
        let needs_medium =
            matches!(op, 0x00 | 0x25 | 0x28 | 0xa8 | 0x2b | 0x43 | 0x42 | 0x51 | 0x52 | 0xad | 0xbe | 0xb9);
        self.dev[self.sel].status = ST_DRDY | ST_BSY;
        let c = self.cd();
        if op != 0x03 {
            c.sense = sense::NONE;
        }
        if !allow_ua && let Some(ua) = c.unit_attention.take() {
            self.atapi_error(ua);
            return;
        }
        if needs_medium && !c.present() {
            let s = c.not_ready();
            self.atapi_error(s);
            return;
        }
        match op {
            0x00 => self.atapi_ok(),
            0x03 => self.request_sense(cdb, mem),
            0x12 => self.inquiry(cdb, mem),
            0x15 => self.data_out(cdb[4] as usize, mem),
            0x55 => self.data_out(be16(&cdb[7..]), mem),
            0x1a | 0x5a => self.mode_sense(cdb, mem),
            0x1b => self.start_stop(cdb),
            0x1e => {
                self.cd().locked = cdb[4] & 1 != 0;
                self.atapi_ok();
            }
            0x25 => {
                let last = (self.cd().sectors() - 1).min(u32::MAX as u64) as u32;
                let mut out = last.to_be_bytes().to_vec();
                out.extend_from_slice(&(SECTOR as u32).to_be_bytes());
                self.reply(out, 8, mem);
            }
            0x28 => self.read(be32(&cdb[2..]), be16(&cdb[7..]) as u32, 0x10, mem),
            0xa8 => self.read(be32(&cdb[2..]), be32(&cdb[6..]), 0x10, mem),
            0xbe => self.read_cd(cdb, mem),
            0xb9 => self.read_cd_msf(cdb, mem),
            0x2b => {
                if be32(&cdb[2..]) as u64 >= self.cd().sectors() {
                    self.atapi_error(sense::LBA_OUT_OF_RANGE);
                } else {
                    self.atapi_ok();
                }
            }
            0x42 => self.read_subchannel(cdb, mem),
            0x43 => self.read_toc(cdb, mem),
            0x46 => self.get_configuration(cdb, mem),
            0x4a => self.event_status(cdb, mem),
            0x51 => self.disc_information(cdb, mem),
            0x52 => self.track_information(cdb, mem),
            0xad => self.dvd_structure(cdb, mem),
            0xbb => self.atapi_ok(),
            0xbd => {
                let out = vec![0, 0, 0, 0, 0, 1, 0, 0];
                self.reply(out, be16(&cdb[8..]), mem);
            }
            _ => self.atapi_error(sense::INVALID_OPCODE),
        }
    }

    fn request_sense(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let c = self.cd();
        let s = match c.sense {
            sense::NONE => c.unit_attention.take().unwrap_or(sense::NONE),
            s => s,
        };
        c.sense = sense::NONE;
        let mut out = vec![0u8; 18];
        out[0] = 0x70;
        out[2] = s.key;
        out[7] = 10;
        out[12] = s.asc;
        out[13] = s.ascq;
        self.reply(out, cdb[4] as usize, mem);
    }

    fn inquiry(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let alloc = cdb[4] as usize;
        if cdb[1] & 1 != 0 {
            if cdb[2] != 0 {
                self.atapi_error(sense::INVALID_FIELD);
                return;
            }
            self.reply(vec![0x05, 0x00, 0x00, 0x01, 0x00], alloc, mem);
            return;
        }
        let mut out = vec![0x05, 0x80, 0x00, 0x21, 31, 0, 0, 0];
        out.extend_from_slice(b"86SLOPPR");
        out.extend_from_slice(b"DVD-ROM         ");
        out.extend_from_slice(b"1.0 ");
        self.reply(out, alloc, mem);
    }

    fn start_stop(&mut self, cdb: &[u8; 12]) {
        let start = cdb[4] & 1 != 0;
        let loej = cdb[4] & 2 != 0;
        if cdb[4] & 0xf0 != 0 || !loej {
            self.atapi_ok();
            return;
        }
        let c = self.cd();
        if !start && c.locked {
            self.atapi_error(sense::REMOVAL_PREVENTED);
            return;
        }
        if c.tray_open == start {
            c.tray_open = !start;
            c.media_event = if start { 2 } else { 3 };
        }
        self.atapi_ok();
    }

    fn read(&mut self, lba: u32, count: u32, flags: u8, mem: &mut dyn DmaMemory) {
        if count == 0 {
            self.atapi_ok();
            return;
        }
        if lba as u64 + count as u64 > self.cd().sectors() {
            self.atapi_error(sense::LBA_OUT_OF_RANGE);
            return;
        }
        let size = cd_sector_size(flags);
        let total = count as usize * size;
        let stream = if flags == 0x10 {
            Stream::Linear(lba as u64 * SECTOR as u64)
        } else {
            Stream::CdRaw { lba, flags, size }
        };
        self.data_in(stream, total, mem);
    }

    fn read_cd_common(&mut self, cdb: &[u8; 12], lba: u32, count: u32, mem: &mut dyn DmaMemory) {
        let sector_type = (cdb[1] >> 2) & 7;
        if sector_type != 0 && sector_type != 2 {
            self.atapi_error(sense::ILLEGAL_MODE);
            return;
        }
        if cdb[9] & 0x06 != 0 || cdb[10] & 7 != 0 {
            self.atapi_error(sense::INVALID_FIELD);
            return;
        }
        // A Mode 1 sector has no sub-header, so header code 2 adds nothing.
        let flags = cdb[9] & 0xf8 & !0x40;
        if flags == 0 {
            self.atapi_ok();
            return;
        }
        self.read(lba, count, flags, mem);
    }

    fn read_cd(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let lba = be32(&cdb[2..]);
        let count = (cdb[6] as u32) << 16 | (cdb[7] as u32) << 8 | cdb[8] as u32;
        self.read_cd_common(cdb, lba, count, mem);
    }

    fn read_cd_msf(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let msf = |b: &[u8]| (b[0] as u32 * 60 + b[1] as u32) * 75 + b[2] as u32;
        let (start, end) = (msf(&cdb[3..6]), msf(&cdb[6..9]));
        if start < 150 || end < start {
            self.atapi_error(sense::INVALID_FIELD);
            return;
        }
        self.read_cd_common(cdb, start - 150, end - start, mem);
    }

    // Formats 0 and 2 follow QEMU hw/block/cdrom.c cdrom_read_toc() and
    // cdrom_read_toc_raw() (Fabrice Bellard), a single-track data disc.
    fn read_toc(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let alloc = be16(&cdb[7..]);
        let msf = cdb[1] & 2 != 0;
        let format = match cdb[2] & 0xf {
            0 => cdb[9] >> 6,
            f => f,
        };
        let start_track = cdb[6];
        let leadout = self.cd().sectors() as u32;
        let mut out = vec![0, 0, 1, 1];
        match format {
            0 => {
                if start_track > 1 && start_track != 0xaa {
                    self.atapi_error(sense::INVALID_FIELD);
                    return;
                }
                if start_track <= 1 {
                    out.extend_from_slice(&[0, 0x14, 1, 0]);
                    put_addr(&mut out, 0, msf);
                }
                out.extend_from_slice(&[0, 0x16, 0xaa, 0]);
                put_addr(&mut out, leadout, msf);
            }
            1 => {
                out.extend_from_slice(&[0, 0x14, 1, 0]);
                put_addr(&mut out, 0, msf);
            }
            2 => {
                out.extend_from_slice(&[1, 0x14, 0, 0xa0, 0, 0, 0, 0, 1, 0x00, 0x00]);
                out.extend_from_slice(&[1, 0x14, 0, 0xa1, 0, 0, 0, 0, 1, 0x00, 0x00]);
                out.extend_from_slice(&[1, 0x14, 0, 0xa2, 0, 0, 0]);
                put_addr(&mut out, leadout, msf);
                out.extend_from_slice(&[1, 0x14, 0, 1, 0, 0, 0]);
                put_addr(&mut out, 0, msf);
            }
            _ => {
                self.atapi_error(sense::INVALID_FIELD);
                return;
            }
        }
        let len = (out.len() - 2) as u16;
        out[0..2].copy_from_slice(&len.to_be_bytes());
        self.reply(out, alloc, mem);
    }

    fn read_subchannel(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let alloc = be16(&cdb[7..]);
        let msf = cdb[1] & 2 != 0;
        let mut out = vec![0, 0x15, 0, 0];
        if cdb[2] & 0x40 != 0 {
            match cdb[3] {
                1 => {
                    out.extend_from_slice(&[1, 0x14, 1, 1]);
                    put_addr(&mut out, 0, msf);
                    if msf {
                        out.extend_from_slice(&[0, 0, 0, 0]);
                    } else {
                        put_addr(&mut out, 0, false);
                    }
                }
                2 | 3 => {
                    out.push(cdb[3]);
                    out.resize(4 + 20, 0);
                }
                _ => {
                    self.atapi_error(sense::INVALID_FIELD);
                    return;
                }
            }
        }
        let len = (out.len() - 4) as u16;
        out[2..4].copy_from_slice(&len.to_be_bytes());
        self.reply(out, alloc, mem);
    }

    fn mode_page(&mut self, code: u8) -> Option<Vec<u8>> {
        let locked = self.cd().locked;
        Some(match code {
            0x01 => vec![0x01, 0x06, 0x00, 0x05, 0, 0, 0, 0],
            0x0d => vec![0x0d, 0x06, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x4b],
            0x0e => vec![0x0e, 0x0e, 0x04, 0, 0, 0, 0, 0, 0x01, 0xff, 0x02, 0xff, 0, 0, 0, 0],
            // Capabilities values follow QEMU hw/ide/atapi.c cmd_mode_sense()
            // (Fabrice Bellard and the QEMU contributors).
            0x2a => {
                let mut p = vec![0u8; 22];
                p[0] = 0x2a;
                p[1] = 20;
                p[2] = 0x3b;
                p[4] = 0x71;
                p[5] = 0x60;
                p[6] = 0x01 | 0x08 | 0x20 | if locked { 0x02 } else { 0 };
                p[8..10].copy_from_slice(&704u16.to_be_bytes());
                p[10..12].copy_from_slice(&2u16.to_be_bytes());
                p[12..14].copy_from_slice(&512u16.to_be_bytes());
                p[14..16].copy_from_slice(&704u16.to_be_bytes());
                p
            }
            _ => return None,
        })
    }

    fn mode_sense(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let ten = cdb[0] == 0x5a;
        let alloc = if ten { be16(&cdb[7..]) } else { cdb[4] as usize };
        let control = cdb[2] >> 6;
        let code = cdb[2] & 0x3f;
        if control == 3 {
            self.atapi_error(sense::SAVING_UNSUPPORTED);
            return;
        }
        let mut pages = Vec::new();
        let codes: &[u8] = if code == 0x3f { &[0x01, 0x0d, 0x0e, 0x2a] } else { std::slice::from_ref(&code) };
        for &c in codes {
            let Some(mut p) = self.mode_page(c) else {
                self.atapi_error(sense::INVALID_FIELD);
                return;
            };
            if control == 1 {
                p[2..].fill(0);
            }
            pages.extend_from_slice(&p);
        }
        let c = self.cd();
        let medium = if !c.present() {
            0x70
        } else if c.is_dvd() {
            0x00
        } else {
            0x01
        };
        let mut out = if ten {
            let len = (pages.len() + 6) as u16;
            let mut h = len.to_be_bytes().to_vec();
            h.extend_from_slice(&[medium, 0, 0, 0, 0, 0]);
            h
        } else {
            vec![(pages.len() + 3) as u8, medium, 0, 0]
        };
        out.extend_from_slice(&pages);
        self.reply(out, alloc, mem);
    }

    fn get_configuration(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let rt = cdb[1] & 3;
        let start = be16(&cdb[2..]) as u16;
        let alloc = be16(&cdb[7..]);
        if rt == 3 {
            self.atapi_error(sense::INVALID_FIELD);
            return;
        }
        let c = self.cd();
        let present = c.present();
        let dvd = c.is_dvd();
        let profile: u16 = match (present, dvd) {
            (false, _) => 0,
            (true, true) => 0x0010,
            (true, false) => 0x0008,
        };
        let cur = |on: bool| if on { 0x01 } else { 0x00 };
        let mut features: Vec<(u16, bool, Vec<u8>)> = vec![
            (
                0x0000,
                true,
                vec![0x03, 8, 0x00, 0x10, cur(profile == 0x10), 0, 0x00, 0x08, cur(profile == 0x08), 0],
            ),
            (0x0001, true, vec![0x03, 4, 0, 0, 0, 2]),
            (0x0002, true, vec![0x07, 4, 0, 0, 0, 0]),
            (0x0003, true, vec![0x03, 4, 0x29, 0, 0, 0]),
            (0x0010, present, {
                let mut v = vec![cur(present), 8];
                v.extend_from_slice(&(SECTOR as u32).to_be_bytes());
                v.extend_from_slice(&(if dvd { 16u16 } else { 1 }).to_be_bytes());
                v.extend_from_slice(&[0, 0]);
                v
            }),
            (0x001e, present && !dvd, vec![0x08 | cur(present && !dvd), 4, 0, 0, 0, 0]),
            (0x001f, dvd, vec![cur(dvd), 0]),
            (0x0100, true, vec![0x03, 0]),
        ];
        features.retain(|(code, current, _)| match rt {
            0 => *code >= start,
            1 => *code >= start && *current,
            _ => *code == start,
        });
        let mut out = vec![0, 0, 0, 0, 0, 0];
        out.extend_from_slice(&profile.to_be_bytes());
        for (code, _, body) in features {
            out.extend_from_slice(&code.to_be_bytes());
            out.extend_from_slice(&body);
        }
        let len = (out.len() - 4) as u32;
        out[0..4].copy_from_slice(&len.to_be_bytes());
        self.reply(out, alloc, mem);
    }

    // Polled media events only, as in QEMU hw/ide/atapi.c
    // cmd_get_event_status_notification() (the QEMU contributors).
    fn event_status(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        if cdb[1] & 1 == 0 {
            self.atapi_error(sense::INVALID_FIELD);
            return;
        }
        let alloc = be16(&cdb[7..]);
        let c = self.cd();
        let out = if cdb[4] & 0x10 != 0 {
            let event = std::mem::take(&mut c.media_event);
            let status = (c.present() as u8) << 1 | c.tray_open as u8;
            vec![0, 6, 0x04, 0x10, event, status, 0, 0]
        } else {
            vec![0, 2, 0x80, 0x10]
        };
        self.reply(out, alloc, mem);
    }

    // Field values follow QEMU hw/ide/atapi.c cmd_read_disc_information()
    // (Fabrice Bellard and the QEMU contributors).
    fn disc_information(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        if cdb[1] & 7 != 0 {
            self.atapi_error(sense::INVALID_FIELD);
            return;
        }
        let mut out = vec![0u8; 34];
        out[1] = 32;
        out[2] = 0x0e;
        out[3] = 1;
        out[4] = 1;
        out[5] = 1;
        out[6] = 1;
        out[7] = 0x20;
        self.reply(out, be16(&cdb[7..]), mem);
    }

    fn track_information(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let sectors = self.cd().sectors() as u32;
        let n = be32(&cdb[2..]);
        let ok = match cdb[1] & 3 {
            0 => n < sectors,
            1 | 2 => n == 1,
            _ => false,
        };
        if !ok {
            self.atapi_error(sense::INVALID_FIELD);
            return;
        }
        let mut out = vec![0u8; 36];
        out[1] = 34;
        out[2] = 1;
        out[3] = 1;
        out[5] = 0x04;
        out[6] = 0x01;
        out[24..28].copy_from_slice(&sectors.to_be_bytes());
        out[28..32].copy_from_slice(&sectors.saturating_sub(1).to_be_bytes());
        self.reply(out, be16(&cdb[7..]), mem);
    }

    fn dvd_structure(&mut self, cdb: &[u8; 12], mem: &mut dyn DmaMemory) {
        let alloc = be16(&cdb[8..]);
        let c = self.cd();
        if !c.is_dvd() {
            self.atapi_error(sense::INCOMPATIBLE_FORMAT);
            return;
        }
        let sectors = c.sectors() as u32;
        let out = match cdb[7] {
            0x00 => {
                let mut v = vec![0u8; 2052];
                v[0..2].copy_from_slice(&2050u16.to_be_bytes());
                v[4] = 0x01;
                v[5] = 0x0f;
                v[6] = 0x01;
                v[8..12].copy_from_slice(&0x30000u32.to_be_bytes());
                v[12..16].copy_from_slice(&(0x30000 + sectors - 1).to_be_bytes());
                v
            }
            0x01 => vec![0, 6, 0, 0, 0, 0, 0, 0],
            _ => {
                self.atapi_error(sense::INVALID_FIELD);
                return;
            }
        };
        self.reply(out, alloc, mem);
    }
}

impl Taskfile {
    pub fn signature_packet(&mut self) {
        self.nsector = 1;
        self.sector = 1;
        self.lcyl = 0x14;
        self.hcyl = 0xeb;
        self.select &= 0x10 | DEV_ALWAYS;
    }
}

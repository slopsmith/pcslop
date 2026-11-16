#![allow(dead_code)]

use std::collections::HashMap;
use std::io;

use ide::{BlockBackend, DmaMemory, Ide, MemBackend};

pub const PRI: u16 = 0x1f0;
pub const SEC: u16 = 0x170;
pub const PRI_CTL: u16 = 0x3f6;
pub const SEC_CTL: u16 = 0x376;
pub const BM: u16 = 0xc000;

pub struct Ram(pub Vec<u8>);

impl DmaMemory for Ram {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]) {
        let a = addr as usize;
        buf.copy_from_slice(&self.0[a..a + buf.len()]);
    }
    fn write_phys(&mut self, addr: u32, buf: &[u8]) {
        let a = addr as usize;
        self.0[a..a + buf.len()].copy_from_slice(buf);
    }
}

/// Byte at `off` of a [`pattern_disk`].
pub fn pattern(off: u64) -> u8 {
    let lba = off / 512;
    (lba as u8).wrapping_mul(7) ^ (off % 512) as u8 ^ (lba >> 8) as u8
}

pub fn pattern_disk(sectors: usize) -> Box<dyn BlockBackend> {
    let data = (0..sectors as u64 * 512).map(pattern).collect();
    Box::new(MemBackend::new(data, false))
}

/// A huge disk whose unwritten bytes follow [`pattern`].
pub struct SparseDisk {
    pub len: u64,
    pub written: HashMap<u64, u8>,
}

impl BlockBackend for SparseDisk {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        for (i, b) in buf.iter_mut().enumerate() {
            let o = off + i as u64;
            *b = self.written.get(&o).copied().unwrap_or_else(|| pattern(o));
        }
        Ok(())
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        for (i, b) in buf.iter().enumerate() {
            self.written.insert(off + i as u64, *b);
        }
        Ok(())
    }
    fn read_only(&self) -> bool {
        false
    }
}

/// A CD image whose sector `n` starts with the bytes of `n` (little endian)
/// followed by a fill of `n as u8`.
pub fn pattern_cd(sectors: u32) -> Box<dyn BlockBackend> {
    let mut data = vec![0u8; sectors as usize * 2048];
    for n in 0..sectors {
        let s = &mut data[n as usize * 2048..][..2048];
        s.fill(n as u8);
        s[..4].copy_from_slice(&n.to_le_bytes());
    }
    Box::new(MemBackend::new(data, true))
}

pub struct Pc {
    pub ide: Ide,
    pub ram: Ram,
}

impl Pc {
    pub fn new() -> Self {
        let mut ide = Ide::new();
        ide.set_bus_master_base(Some(BM));
        Pc { ide, ram: Ram(vec![0; 4 << 20]) }
    }

    pub fn inb(&mut self, port: u16) -> u8 {
        self.ide.io_read(port, 1).expect("port not decoded") as u8
    }
    pub fn inw(&mut self, port: u16) -> u16 {
        self.ide.io_read(port, 2).expect("port not decoded") as u16
    }
    pub fn inl(&mut self, port: u16) -> u32 {
        self.ide.io_read(port, 4).expect("port not decoded")
    }
    pub fn outb(&mut self, port: u16, v: u8) {
        assert!(self.ide.io_write(port, 1, v as u32, &mut self.ram));
    }
    pub fn outw(&mut self, port: u16, v: u16) {
        assert!(self.ide.io_write(port, 2, v as u32, &mut self.ram));
    }
    pub fn outl(&mut self, port: u16, v: u32) {
        assert!(self.ide.io_write(port, 4, v, &mut self.ram));
    }

    pub fn select(&mut self, base: u16, slave: bool) {
        self.outb(base + 6, if slave { 0xb0 } else { 0xa0 });
    }

    /// Writes LBA28 registers and a command for device `dev` (0 or 1).
    pub fn lba28(&mut self, base: u16, dev: u8, lba: u32, count: u8, cmd: u8) {
        self.outb(base + 6, 0xe0 | dev << 4 | ((lba >> 24) & 0xf) as u8);
        self.outb(base + 2, count);
        self.outb(base + 3, lba as u8);
        self.outb(base + 4, (lba >> 8) as u8);
        self.outb(base + 5, (lba >> 16) as u8);
        self.outb(base + 7, cmd);
    }

    pub fn lba48(&mut self, base: u16, lba: u64, count: u16, cmd: u8) {
        self.outb(base + 6, 0x40);
        self.outb(base + 2, (count >> 8) as u8);
        self.outb(base + 3, (lba >> 24) as u8);
        self.outb(base + 4, (lba >> 32) as u8);
        self.outb(base + 5, (lba >> 40) as u8);
        self.outb(base + 2, count as u8);
        self.outb(base + 3, lba as u8);
        self.outb(base + 4, (lba >> 8) as u8);
        self.outb(base + 5, (lba >> 16) as u8);
        self.outb(base + 7, cmd);
    }

    /// Reads `bytes` from the data port with `width`-byte accesses.
    pub fn read_pio(&mut self, base: u16, bytes: usize, width: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes);
        while out.len() < bytes {
            match width {
                2 => out.extend_from_slice(&self.inw(base).to_le_bytes()),
                4 => out.extend_from_slice(&self.inl(base).to_le_bytes()),
                _ => unreachable!(),
            }
        }
        out
    }

    pub fn write_pio(&mut self, base: u16, data: &[u8], width: u8) {
        for c in data.chunks(width as usize) {
            match width {
                2 => self.outw(base, u16::from_le_bytes([c[0], c[1]])),
                4 => self.outl(base, u32::from_le_bytes(c.try_into().unwrap())),
                _ => unreachable!(),
            }
        }
    }

    pub fn status(&mut self, base: u16) -> u8 {
        self.inb(base + 7)
    }

    pub fn identify(&mut self, base: u16, dev: u8, cmd: u8) -> Vec<u16> {
        self.outb(base + 6, 0xa0 | dev << 4);
        self.outb(base + 7, cmd);
        assert_eq!(self.status(base) & 0x09, 0x08, "IDENTIFY did not raise DRQ");
        self.read_pio(base, 512, 2).chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
    }

    /// Sends PACKET with a byte count limit and returns the data-in blocks.
    /// Ends with the status register value after the final interrupt.
    pub fn packet(&mut self, base: u16, cdb: &[u8], bcl: u16) -> (Vec<Vec<u8>>, u8) {
        self.outb(base + 1, 0);
        self.outb(base + 4, bcl as u8);
        self.outb(base + 5, (bcl >> 8) as u8);
        self.outb(base + 7, 0xa0);
        let st = self.status(base);
        assert_eq!(st & 0x88, 0x08, "PACKET did not ask for the CDB");
        assert_eq!(self.inb(base + 2) & 3, 1, "interrupt reason is not CoD");
        let mut cdb12 = [0u8; 12];
        cdb12[..cdb.len()].copy_from_slice(cdb);
        self.write_pio(base, &cdb12, 2);
        let mut blocks = Vec::new();
        loop {
            let st = self.status(base);
            if st & 0x08 == 0 {
                return (blocks, st);
            }
            assert_eq!(self.inb(base + 2) & 3, 2, "data-in block without IO reason");
            let n = self.inb(base + 4) as usize | (self.inb(base + 5) as usize) << 8;
            let mut b = self.read_pio(base, n.next_multiple_of(2), 2);
            b.truncate(n);
            blocks.push(b);
        }
    }

    /// PACKET whose data is expected to be returned in full; panics on error.
    pub fn packet_data(&mut self, base: u16, cdb: &[u8], alloc_bcl: u16) -> Vec<u8> {
        let (blocks, st) = self.packet(base, cdb, alloc_bcl);
        assert_eq!(st & 0x01, 0, "packet {:#04x} failed, error {:#04x}", cdb[0], self.inb(base + 1));
        blocks.concat()
    }

    /// Returns (sense key, ASC, ASCQ) from REQUEST SENSE.
    pub fn request_sense(&mut self, base: u16) -> (u8, u8, u8) {
        let s = self.packet_data(base, &[0x03, 0, 0, 0, 18, 0], 18);
        (s[2] & 0xf, s[12], s[13])
    }

    /// Clears the power-on unit attention of a CD-ROM.
    pub fn clear_ua(&mut self, base: u16) {
        self.request_sense(base);
    }

    pub fn write_prd(&mut self, table: u32, entries: &[(u32, u32, bool)]) {
        for (i, &(addr, len, eot)) in entries.iter().enumerate() {
            let a = table as usize + i * 8;
            self.ram.0[a..a + 4].copy_from_slice(&addr.to_le_bytes());
            let flags = (len & 0xffff) | if eot { 1 << 31 } else { 0 };
            self.ram.0[a + 4..a + 8].copy_from_slice(&flags.to_le_bytes());
        }
    }
}

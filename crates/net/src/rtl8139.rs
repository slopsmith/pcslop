//! Realtek RTL8139C+ Fast Ethernet controller (PCI 10EC:8139, revision 0x20).
//!
//! A port of QEMU's hw/net/rtl8139.c (Copyright (c) 2006 Igor Kovalenko, with
//! changes by Mark Malakanov, Juergen Lock, Frediano Ziglio, Benjamin Poirier
//! and others; MIT license). The register file, the EEPROM, the receive ring,
//! the four transmit descriptors and the C+ descriptor rings with their
//! checksum, segmentation and VLAN offloads follow it closely.

use crate::{Backend, DmaMemory, Eeprom93c46};

pub const PCI_VENDOR: u16 = 0x10ec;
pub const PCI_DEVICE: u16 = 0x8139;
/// Revision 0x20 and up is an 8139C+, which Linux's 8139cp also drives.
pub const PCI_REVISION: u8 = 0x20;
/// Network controller, Ethernet.
pub const PCI_CLASS_CODE: u32 = 0x02_00_00;
/// Size of the I/O BAR and of the memory BAR, which decode the same registers.
pub const REGS_SIZE: u32 = 0x100;

mod reg {
    pub const MAR0: u8 = 0x08;
    pub const TX_STATUS0: u8 = 0x10;
    pub const TX_ADDR0: u8 = 0x20;
    pub const RX_BUF: u8 = 0x30;
    pub const CHIP_CMD: u8 = 0x37;
    pub const RX_BUF_PTR: u8 = 0x38;
    pub const RX_BUF_ADDR: u8 = 0x3a;
    pub const INTR_MASK: u8 = 0x3c;
    pub const INTR_STATUS: u8 = 0x3e;
    pub const TX_CONFIG: u8 = 0x40;
    pub const RX_CONFIG: u8 = 0x44;
    pub const TIMER: u8 = 0x48;
    pub const RX_MISSED: u8 = 0x4c;
    pub const CFG9346: u8 = 0x50;
    pub const CONFIG0: u8 = 0x51;
    pub const CONFIG1: u8 = 0x52;
    /// TimerInt on the 8139C and later.
    pub const FLASH_REG: u8 = 0x54;
    pub const MEDIA_STATUS: u8 = 0x58;
    pub const CONFIG3: u8 = 0x59;
    pub const CONFIG4: u8 = 0x5a;
    pub const HLT_CLK: u8 = 0x5b;
    pub const MULTI_INTR: u8 = 0x5c;
    pub const PCI_REVISION_ID: u8 = 0x5e;
    pub const TX_SUMMARY: u8 = 0x60;
    pub const BASIC_MODE_CTRL: u8 = 0x62;
    pub const BASIC_MODE_STATUS: u8 = 0x64;
    pub const NWAY_ADVERT: u8 = 0x66;
    pub const NWAY_LPAR: u8 = 0x68;
    pub const NWAY_EXPANSION: u8 = 0x6a;
    pub const CSCR: u8 = 0x74;
    pub const CONFIG5: u8 = 0xd8;
    pub const TX_POLL: u8 = 0xd9;
    pub const CP_CMD: u8 = 0xe0;
    pub const INTR_MITIGATE: u8 = 0xe2;
    pub const RX_RING_ADDR_LO: u8 = 0xe4;
    pub const RX_RING_ADDR_HI: u8 = 0xe8;
    pub const TX_THRESH: u8 = 0xec;
}

/// Interrupt status and mask bits.
pub mod intr {
    pub const PCS_TIMEOUT: u16 = 0x4000;
    pub const RX_UNDERRUN: u16 = 0x20;
    pub const RX_OVERFLOW: u16 = 0x10;
    pub const TX_OK: u16 = 0x04;
    pub const RX_OK: u16 = 0x01;
}

const CMD_RESET: u8 = 0x10;
const CMD_RX_ENB: u8 = 0x08;
const CMD_TX_ENB: u8 = 0x04;
const RX_BUF_EMPTY: u8 = 0x01;

const CP_RX_VLAN: u16 = 0x40;
const CP_RX_ENB: u16 = 0x02;
const CP_TX_ENB: u16 = 0x01;

/// C+ descriptors: four little-endian dwords, flags and length first.
const CP_OWN: u32 = 1 << 31;
const CP_EOR: u32 = 1 << 30;
const CP_FS: u32 = 1 << 29;
const CP_LS: u32 = 1 << 28;
const CP_TX_LGSEN: u32 = 1 << 27;
const CP_TX_IPCS: u32 = 1 << 18;
const CP_TX_UDPCS: u32 = 1 << 17;
const CP_TX_TCPCS: u32 = 1 << 16;
/// The bits a sent descriptor keeps. The rest report errors and, in bits
/// 16-19, collisions, where the driver had put its offload flags.
const CP_TX_DONE_KEEP: u32 = CP_EOR | CP_FS | CP_LS | 0xffff;
const CP_TX_TAGC: u32 = 1 << 17;
const CP_RX_TAVA: u32 = 1 << 16;
const CP_RX_MAR: u32 = 1 << 26;
const CP_RX_PAM: u32 = 1 << 25;
const CP_RX_BAR: u32 = 1 << 24;
const CP_RX_SIZE_MASK: u32 = (1 << 13) - 1;
const CP_RING_LEN: u32 = 64;
const CP_TX_BUFFER_SIZE: usize = 1 << 16;
/// Dump Tally Counters, in TxStatus0 (DTCCR) while in C+ mode.
const DTCCR_CMD: u32 = 0x08;

const ETH_HLEN: usize = 14;
const ETH_P_IP: u16 = 0x0800;
const ETH_P_VLAN: u16 = 0x8100;
const IP_PROTO_TCP: u8 = 6;
const IP_PROTO_UDP: u8 = 17;
const TCP_PSH: u8 = 0x08;
const TCP_FIN: u8 = 0x01;

const TX_HOST_OWNS: u32 = 0x2000;
const TX_UNDERRUN: u32 = 0x4000;
const TX_STAT_OK: u32 = 0x8000;
const TX_ABORTED: u32 = 0x4000_0000;

const RX_MULTICAST: u32 = 0x8000;
const RX_PHYSICAL: u32 = 0x4000;
const RX_BROADCAST: u32 = 0x2000;
const RX_STATUS_OK: u32 = 0x0001;

const ACCEPT_BROADCAST: u32 = 0x08;
const ACCEPT_MULTICAST: u32 = 0x04;
const ACCEPT_MY_PHYS: u32 = 0x02;
const ACCEPT_ALL_PHYS: u32 = 0x01;
const RX_NO_WRAP: u32 = 1 << 7;

const TX_LOOPBACK: u32 = 3 << 17;
const TX_VERSION_MASK: u32 = 0x7c80_0000;
/// The hardware version bits of an RTL8139C+.
const TX_VERSION_8139CPLUS: u32 = 1 << 30 | 1 << 29 | 1 << 28 | 1 << 26 | 1 << 23;

const CFG9346_OP_MASK: u8 = 0xc0;
const CFG9346_EEPROM: u8 = 0x80;
const CFG9346_RESET: u8 = 0x40;
const CFG9346_CONFIG_WRITE: u8 = 0xc0;

const BMSR_LINK_UP: u16 = 0x04;
const CSCR_F_LINK_100: u16 = 1 << 6;
const CSCR_HEART_BIT: u16 = 1 << 8;
const CSCR_LD: u16 = 1 << 9;

/// The 33 MHz PCI clock that drives the Timer register.
const PCI_PERIOD_NS: u64 = 30;
const MIN_FRAME: usize = 60;

/// Register traffic and frames, for working out VM exits per packet.
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub rx_dropped: u64,
}

/// The C+ tally counters, in the layout the dump command writes.
#[derive(Clone, Copy, Default, Debug, snapshot::Snap)]
struct Tally {
    tx_ok: u64,
    rx_ok: u64,
    rx_err: u32,
    miss_pkt: u16,
    rx_ok_phy: u64,
    rx_ok_brd: u64,
    rx_ok_mul: u32,
}

impl Tally {
    fn bytes(&self) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[0..8].copy_from_slice(&self.tx_ok.to_le_bytes());
        b[8..16].copy_from_slice(&self.rx_ok.to_le_bytes());
        b[24..28].copy_from_slice(&self.rx_err.to_le_bytes());
        b[28..30].copy_from_slice(&self.miss_pkt.to_le_bytes());
        b[40..48].copy_from_slice(&self.rx_ok_phy.to_le_bytes());
        b[48..56].copy_from_slice(&self.rx_ok_brd.to_le_bytes());
        b[56..60].copy_from_slice(&self.rx_ok_mul.to_le_bytes());
        b
    }
}

enum Rx {
    Consumed,
    /// No room yet: keep the frame and offer it again later.
    Retry,
}

#[derive(snapshot::Snap)]
pub struct Rtl8139 {
    mac: [u8; 6],
    phys: [u8; 6],
    mult: [u8; 8],
    tx_status: [u32; 4],
    tx_addr: [u32; 4],
    rx_buf: u32,
    rx_buf_size: u32,
    /// CAPR plus 16: how far the guest has read.
    rx_buf_ptr: u32,
    /// CBR: where the card writes next.
    rx_buf_addr: u32,
    intr_status: u16,
    intr_mask: u16,
    tx_config: u32,
    rx_config: u32,
    rx_missed: u32,
    cscr: u16,
    cfg9346: u8,
    config0: u8,
    config1: u8,
    config3: u8,
    config4: u8,
    config5: u8,
    clock_enabled: bool,
    chip_cmd: u8,
    multi_intr: u16,
    bmcr: u16,
    bmsr: u16,
    nway_advert: u16,
    nway_lpar: u16,
    nway_expansion: u16,
    cp_cmd: u16,
    tx_thresh: u8,
    curr_tx_desc: usize,
    cplus_enabled: bool,
    rx_ring_lo: u32,
    rx_ring_hi: u32,
    curr_cplus_rx_desc: u32,
    curr_cplus_tx_desc: u32,
    cplus_tx_buf: Vec<u8>,
    tally: Tally,
    eeprom: Eeprom93c46,
    timer_base_ns: u64,
    timer_int: u32,
    timer_deadline: Option<u64>,
    #[snap(skip)]
    backend: Box<dyn Backend>,
    #[snap(skip)]
    pending_rx: Option<Vec<u8>>,
    #[snap(skip)]
    pub stats: Stats,
}

fn set_masked(input: u32, mask: u32, current: u32) -> u32 {
    (input & !mask) | (current & mask)
}

fn addr64(lo: u32, hi: u32) -> u64 {
    lo as u64 | (hi as u64) << 32
}

fn read_u32(dma: &mut dyn DmaMemory, addr: u64) -> u32 {
    let mut b = [0; 4];
    dma.read_phys(addr, &mut b);
    u32::from_le_bytes(b)
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// The Internet checksum of `data`, stored little-endian like QEMU's
/// `ip_checksum`, which puts the bytes in network order.
fn ip_checksum(data: &[u8]) -> [u8; 2] {
    let mut sum: u32 = data.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32).sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    (!(sum as u16)).to_le_bytes()
}

/// The multicast filter hash, as QEMU's `net_crc32` computes it.
fn multicast_hash(mac: &[u8]) -> usize {
    let mut crc = 0xffff_ffffu32;
    for &byte in mac {
        let mut b = byte;
        for _ in 0..8 {
            let carry = (crc >> 31) ^ (b & 1) as u32;
            crc <<= 1;
            b >>= 1;
            if carry != 0 {
                crc = (crc ^ 0x04c1_1db6) | carry;
            }
        }
    }
    (crc >> 26) as usize
}

impl Rtl8139 {
    /// Plugs the card into another network. A frame the old one delivered
    /// but the guest has not taken yet is dropped.
    pub fn set_backend(&mut self, backend: Box<dyn Backend>) {
        self.backend = backend;
        self.pending_rx = None;
    }

    pub fn new(mac: [u8; 6], backend: Box<dyn Backend>) -> Self {
        let mut contents = [0u16; crate::eeprom::WORDS];
        contents[0] = 0x8129;
        contents[1] = PCI_VENDOR;
        contents[2] = PCI_DEVICE;
        for i in 0..3 {
            contents[7 + i] = u16::from_le_bytes([mac[2 * i], mac[2 * i + 1]]);
        }
        let mut nic = Rtl8139 {
            mac,
            phys: mac,
            mult: [0; 8],
            tx_status: [0; 4],
            tx_addr: [0; 4],
            rx_buf: 0,
            rx_buf_size: 8192,
            rx_buf_ptr: 0,
            rx_buf_addr: 0,
            intr_status: 0,
            intr_mask: 0,
            tx_config: 0,
            rx_config: 0,
            rx_missed: 0,
            cscr: 0,
            cfg9346: 0,
            config0: 0,
            config1: 0,
            config3: 0,
            config4: 0,
            config5: 0,
            clock_enabled: true,
            chip_cmd: 0,
            multi_intr: 0,
            bmcr: 0,
            bmsr: 0,
            nway_advert: 0,
            nway_lpar: 0,
            nway_expansion: 0,
            cp_cmd: 0,
            tx_thresh: 0,
            curr_tx_desc: 0,
            cplus_enabled: false,
            rx_ring_lo: 0,
            rx_ring_hi: 0,
            curr_cplus_rx_desc: 0,
            curr_cplus_tx_desc: 0,
            cplus_tx_buf: Vec::new(),
            tally: Tally::default(),
            eeprom: Eeprom93c46::new(contents),
            timer_base_ns: 0,
            timer_int: 0,
            timer_deadline: None,
            backend,
            pending_rx: None,
            stats: Stats::default(),
        };
        nic.reset();
        nic
    }

    /// The INTA# level.
    pub fn irq(&self) -> bool {
        self.intr_status & self.intr_mask != 0
    }

    /// Software reset, which is also what PCI reset does. Like QEMU, it
    /// leaves RxConfig, the multicast filter and Cfg9346 alone.
    pub fn reset(&mut self) {
        self.phys = self.mac;
        self.intr_status = 0;
        self.intr_mask = 0;
        self.tx_status = [TX_HOST_OWNS; 4];
        self.curr_tx_desc = 0;
        self.curr_cplus_rx_desc = 0;
        self.curr_cplus_tx_desc = 0;
        self.rx_ring_lo = 0;
        self.rx_ring_hi = 0;
        self.rx_buf = 0;
        self.reset_rx_ring(8192);
        self.tx_config = TX_VERSION_8139CPLUS;
        self.clock_enabled = true;
        self.chip_cmd = CMD_RESET;
        self.config0 = 0;
        self.config1 = 0x0c;
        self.config3 = 0x01;
        self.config5 = 0;
        self.cp_cmd = 0;
        self.cplus_enabled = false;
        self.bmcr = 0x1000;
        self.reset_phy();
        self.timer_base_ns = 0;
        self.timer_int = 0;
        self.timer_deadline = None;
        self.tally = Tally::default();
    }

    fn reset_phy(&mut self) {
        self.bmsr = 0x7809 | 0x0020 | BMSR_LINK_UP;
        self.nway_advert = 0x05e1;
        self.nway_lpar = 0x05e1;
        self.nway_expansion = 0x0001;
        self.cscr = CSCR_F_LINK_100 | CSCR_HEART_BIT | CSCR_LD;
    }

    fn reset_rx_ring(&mut self, size: u32) {
        self.rx_buf_size = size;
        self.rx_buf_ptr = 0;
        self.rx_buf_addr = 0;
    }

    fn receiver_enabled(&self) -> bool {
        self.chip_cmd & CMD_RX_ENB != 0
    }

    fn transmitter_enabled(&self) -> bool {
        self.chip_cmd & CMD_TX_ENB != 0
    }

    fn cp_receiver_enabled(&self) -> bool {
        self.cp_cmd & CP_RX_ENB != 0
    }

    /// Linux enables the C+ receiver before it has set up the ring.
    fn cp_rx_valid(&self) -> bool {
        self.rx_ring_lo != 0 || self.rx_ring_hi != 0
    }

    fn config_writable(&self) -> bool {
        self.cfg9346 & CFG9346_OP_MASK == CFG9346_CONFIG_WRITE
    }

    /// When the timer interrupt next needs `poll`.
    pub fn next_event_ns(&self) -> Option<u64> {
        self.timer_deadline
    }

    /// Runs the timer and hands the guest whatever frames the backend has.
    pub fn poll(&mut self, now: u64, dma: &mut dyn DmaMemory) {
        if self.timer_deadline.is_some_and(|t| now >= t) {
            if self.clock_enabled {
                self.intr_status |= intr::PCS_TIMEOUT;
                self.set_next_timer(now);
            } else {
                self.timer_deadline = None;
            }
        }
        self.receive_pending(dma);
    }

    fn receive_pending(&mut self, dma: &mut dyn DmaMemory) {
        loop {
            let Some(frame) = self.pending_rx.take().or_else(|| self.backend.recv()) else { return };
            if !self.can_receive() {
                self.pending_rx = Some(frame);
                return;
            }
            if let Rx::Retry = self.receive(&frame, dma) {
                self.pending_rx = Some(frame);
                return;
            }
        }
    }

    fn can_receive(&self) -> bool {
        if !self.clock_enabled || !self.receiver_enabled() {
            return true;
        }
        if self.cp_receiver_enabled() && self.cp_rx_valid() {
            return true;
        }
        let avail = (self.rx_buf_size + self.rx_buf_ptr - self.rx_buf_addr) % self.rx_buf_size;
        avail == 0 || avail >= 1514 || self.intr_mask & intr::RX_OVERFLOW != 0
    }

    /// The receive status bits a frame earns, or None if the address filter drops it.
    fn address_filter(&self, frame: &[u8]) -> Option<u32> {
        if self.rx_config & ACCEPT_ALL_PHYS != 0 {
            return Some(0);
        }
        if frame[..6] == [0xff; 6] {
            return (self.rx_config & ACCEPT_BROADCAST != 0).then_some(RX_BROADCAST);
        }
        if frame[0] & 1 != 0 {
            let h = multicast_hash(&frame[..6]);
            let hit = self.rx_config & ACCEPT_MULTICAST != 0 && self.mult[h >> 3] & 1 << (h & 7) != 0;
            return hit.then_some(RX_MULTICAST);
        }
        if frame[..6] == self.phys {
            return (self.rx_config & ACCEPT_MY_PHYS != 0).then_some(RX_PHYSICAL);
        }
        None
    }

    fn receive(&mut self, frame: &[u8], dma: &mut dyn DmaMemory) -> Rx {
        if !self.clock_enabled || !self.receiver_enabled() || frame.len() < 6 {
            self.stats.rx_dropped += 1;
            return Rx::Consumed;
        }
        let mut padded;
        let frame = if frame.len() < MIN_FRAME {
            padded = frame.to_vec();
            padded.resize(MIN_FRAME, 0);
            &padded[..]
        } else {
            frame
        };
        let Some(header) = self.address_filter(frame) else {
            self.tally.rx_err += 1;
            self.stats.rx_dropped += 1;
            return Rx::Consumed;
        };
        match header {
            RX_BROADCAST => self.tally.rx_ok_brd += 1,
            RX_MULTICAST => self.tally.rx_ok_mul += 1,
            RX_PHYSICAL => self.tally.rx_ok_phy += 1,
            _ => {}
        }
        let result = if self.cp_receiver_enabled() {
            self.receive_cplus(frame, header, dma)
        } else {
            self.receive_ring(frame, header, dma)
        };
        if let Rx::Consumed = result {
            self.stats.rx_frames += 1;
        }
        result
    }

    fn receive_ring(&mut self, frame: &[u8], header: u32, dma: &mut dyn DmaMemory) -> Rx {
        let size = frame.len() as u32;
        let avail = (self.rx_buf_size + self.rx_buf_ptr - self.rx_buf_addr) % self.rx_buf_size;
        if avail != 0 && (size + 8 + 3) & !3 >= avail {
            self.intr_status |= intr::RX_OVERFLOW;
            self.rx_missed += 1;
            return Rx::Retry;
        }
        let header = header | RX_STATUS_OK | (size + 4) << 16;
        self.write_ring(dma, &header.to_le_bytes());
        self.write_ring(dma, frame);
        self.write_ring(dma, &crc32fast::hash(frame).to_le_bytes());
        self.rx_buf_addr = ((self.rx_buf_addr + 3) & !3) % self.rx_buf_size;
        self.intr_status |= intr::RX_OK;
        Rx::Consumed
    }

    fn receive_cplus(&mut self, frame: &[u8], header: u32, dma: &mut dyn DmaMemory) -> Rx {
        if !self.cp_rx_valid() {
            return Rx::Consumed;
        }
        let desc = addr64(self.rx_ring_lo, self.rx_ring_hi) + 16 * self.curr_cplus_rx_desc as u64;
        let mut dw0 = read_u32(dma, desc);
        let mut dw1 = read_u32(dma, desc + 4);
        let buf = addr64(read_u32(dma, desc + 8), read_u32(dma, desc + 12));
        // QEMU drops the frame here. Holding it until the driver hands
        // descriptors back spares TCP a retransmit timeout.
        if dw0 & CP_OWN == 0 {
            return Rx::Retry;
        }
        let tagged = self.cp_cmd & CP_RX_VLAN != 0 && be16(frame, 12) == ETH_P_VLAN;
        let untagged;
        let data = if tagged {
            dw1 = (dw1 & !0xffff) | CP_RX_TAVA | u16::from_le_bytes([frame[14], frame[15]]) as u32;
            untagged = [&frame[..12], &frame[16..]].concat();
            &untagged[..]
        } else {
            dw1 &= !CP_RX_TAVA;
            frame
        };
        let size = data.len() as u32;
        if size + 4 > dw0 & CP_RX_SIZE_MASK {
            self.intr_status |= intr::RX_OVERFLOW;
            self.rx_missed += 1;
            self.tally.rx_err += 1;
            self.tally.miss_pkt = self.tally.miss_pkt.wrapping_add(1);
            self.stats.rx_dropped += 1;
            return Rx::Consumed;
        }
        dma.write_phys(buf, data);
        dma.write_phys(buf + size as u64, &crc32fast::hash(frame).to_le_bytes());
        dw0 &= !(CP_OWN | CP_RX_SIZE_MASK);
        dw0 |= CP_FS | CP_LS | (size + 4);
        if header & RX_BROADCAST != 0 {
            dw0 |= CP_RX_BAR;
        }
        if header & RX_MULTICAST != 0 {
            dw0 |= CP_RX_MAR;
        }
        if header & RX_PHYSICAL != 0 {
            dw0 |= CP_RX_PAM;
        }
        dma.write_phys(desc, &dw0.to_le_bytes());
        dma.write_phys(desc + 4, &dw1.to_le_bytes());
        self.tally.rx_ok += 1;
        self.curr_cplus_rx_desc = if dw0 & CP_EOR != 0 { 0 } else { (self.curr_cplus_rx_desc + 1) % CP_RING_LEN };
        self.intr_status |= intr::RX_OK;
        Rx::Consumed
    }

    /// With RxNoWrap the guest leaves 1.5 KiB of slack after the ring.
    fn write_ring(&mut self, dma: &mut dyn DmaMemory, buf: &[u8]) {
        let size = buf.len() as u32;
        let base = self.rx_buf as u64;
        if self.rx_buf_addr + size > self.rx_buf_size {
            let wrapped = (self.rx_buf_addr + size) % self.rx_buf_size;
            let no_wrap = self.rx_buf_size < 65536 && self.rx_config & RX_NO_WRAP != 0;
            if wrapped != 0 && !no_wrap {
                let first = (size - wrapped) as usize;
                if first > 0 {
                    dma.write_phys(base + self.rx_buf_addr as u64, &buf[..first]);
                }
                dma.write_phys(base, &buf[first..]);
                self.rx_buf_addr = wrapped;
                return;
            }
        }
        dma.write_phys(base + self.rx_buf_addr as u64, buf);
        self.rx_buf_addr += size;
    }

    fn rx_buffer_empty(&self) -> bool {
        (self.rx_buf_size + self.rx_buf_addr - self.rx_buf_ptr) % self.rx_buf_size == 0
    }

    /// Sends a frame, or loops it back to our own receiver in loopback mode.
    fn transfer_frame(&mut self, frame: &[u8], dma: &mut dyn DmaMemory) {
        if frame.is_empty() {
            return;
        }
        self.stats.tx_frames += 1;
        if self.tx_config & TX_LOOPBACK == TX_LOOPBACK {
            let _ = self.receive(frame, dma);
        } else {
            self.backend.send(frame);
        }
    }

    fn transmit_one(&mut self, descriptor: usize, dma: &mut dyn DmaMemory) -> bool {
        if !self.transmitter_enabled() || self.tx_status[descriptor] & TX_HOST_OWNS != 0 {
            return false;
        }
        let size = (self.tx_status[descriptor] & 0x1fff) as usize;
        let mut frame = vec![0; size];
        dma.read_phys(self.tx_addr[descriptor] as u64, &mut frame);
        self.tx_status[descriptor] |= TX_HOST_OWNS | TX_STAT_OK;
        self.transfer_frame(&frame, dma);
        self.intr_status |= intr::TX_OK;
        true
    }

    /// Like QEMU, only the descriptor the card expects next is looked at.
    fn transmit(&mut self, dma: &mut dyn DmaMemory) {
        if self.transmit_one(self.curr_tx_desc, dma) {
            self.curr_tx_desc = (self.curr_tx_desc + 1) % 4;
        }
    }

    fn cplus_transmit(&mut self, dma: &mut dyn DmaMemory) {
        let mut sent = 0;
        while sent < CP_RING_LEN && self.cplus_transmit_one(dma) {
            sent += 1;
        }
        if sent > 0 {
            self.intr_status |= intr::TX_OK;
        }
    }

    /// Takes one descriptor from the normal-priority ring, whose base is in
    /// TxAddr0 and TxAddr1, and sends the frame once its last segment is in.
    fn cplus_transmit_one(&mut self, dma: &mut dyn DmaMemory) -> bool {
        if !self.transmitter_enabled() || self.cp_cmd & CP_TX_ENB == 0 {
            return false;
        }
        let desc = addr64(self.tx_addr[0], self.tx_addr[1]) + 16 * self.curr_cplus_tx_desc as u64;
        let dw0 = read_u32(dma, desc);
        let dw1 = read_u32(dma, desc + 4);
        let buf = addr64(read_u32(dma, desc + 8), read_u32(dma, desc + 12));
        if dw0 & CP_OWN == 0 {
            return false;
        }
        if dw0 & CP_FS != 0 {
            self.cplus_tx_buf.clear();
        }
        let len = ((dw0 & 0xffff) as usize).min(CP_TX_BUFFER_SIZE - self.cplus_tx_buf.len());
        let start = self.cplus_tx_buf.len();
        self.cplus_tx_buf.resize(start + len, 0);
        dma.read_phys(buf, &mut self.cplus_tx_buf[start..]);
        self.curr_cplus_tx_desc = if dw0 & CP_EOR != 0 { 0 } else { (self.curr_cplus_tx_desc + 1) % CP_RING_LEN };
        dma.write_phys(desc, &(dw0 & CP_TX_DONE_KEEP).to_le_bytes());
        if dw0 & CP_LS != 0 {
            let mut frame = std::mem::take(&mut self.cplus_tx_buf);
            let tag = (dw1 & CP_TX_TAGC != 0).then(|| dw1 as u16);
            self.tally.tx_ok += 1;
            if !self.offload(&mut frame, dw0, tag, dma) {
                self.transfer_tagged(&frame, tag, dma);
            }
            frame.clear();
            self.cplus_tx_buf = frame;
        }
        true
    }

    /// Sends `frame` with an 802.1Q tag if the descriptor asked for one.
    /// The descriptor holds the tag control field in network byte order.
    fn transfer_tagged(&mut self, frame: &[u8], tag: Option<u16>, dma: &mut dyn DmaMemory) {
        match tag {
            Some(tci) if frame.len() >= 12 => {
                let mut tagged = frame[..12].to_vec();
                tagged.extend_from_slice(&ETH_P_VLAN.to_be_bytes());
                tagged.extend_from_slice(&tci.to_le_bytes());
                tagged.extend_from_slice(&frame[12..]);
                self.transfer_frame(&tagged, dma);
            }
            _ => self.transfer_frame(frame, dma),
        }
    }

    /// Fills in IPv4, TCP and UDP checksums, or splits a large TCP send into
    /// MSS-sized segments and sends them. Returns true if it sent the frame.
    fn offload(&mut self, frame: &mut [u8], dw0: u32, tag: Option<u16>, dma: &mut dyn DmaMemory) -> bool {
        if dw0 & (CP_TX_IPCS | CP_TX_UDPCS | CP_TX_TCPCS | CP_TX_LGSEN) == 0
            || frame.len() < ETH_HLEN + 20
            || be16(frame, 12) != ETH_P_IP
            || frame[ETH_HLEN] >> 4 != 4
        {
            return false;
        }
        let payload_len = frame.len() - ETH_HLEN;
        let hlen = 4 * (frame[ETH_HLEN] & 0xf) as usize;
        let ip_len = be16(frame, ETH_HLEN + 2) as usize;
        if hlen < 20 || hlen > payload_len || ip_len < hlen || ip_len > payload_len {
            return false;
        }
        let protocol = frame[ETH_HLEN + 9];
        let data_len = ip_len - hlen;
        let ip = ETH_HLEN;
        let l4 = ip + hlen;
        // The checksums cover a pseudo-header, built in the 12 bytes before
        // the TCP or UDP header and then replaced by the saved IP header.
        let pseudo = l4 - 12;
        let saved_ip: Vec<u8> = frame[ip..l4].to_vec();
        let set_ip_checksum = |frame: &mut [u8]| {
            frame[ip + 10..ip + 12].fill(0);
            let sum = ip_checksum(&frame[ip..l4]);
            frame[ip + 10..ip + 12].copy_from_slice(&sum);
        };
        let set_pseudo = |frame: &mut [u8], proto: u8, len: usize| {
            frame[pseudo..pseudo + 8].copy_from_slice(&saved_ip[12..20]);
            frame[pseudo + 8] = 0;
            frame[pseudo + 9] = proto;
            frame[pseudo + 10..pseudo + 12].copy_from_slice(&(len as u16).to_be_bytes());
        };
        if dw0 & CP_TX_LGSEN == 0 {
            if dw0 & CP_TX_IPCS != 0 {
                set_ip_checksum(frame);
            }
            let sum_at = match protocol {
                IP_PROTO_TCP if dw0 & CP_TX_TCPCS != 0 && data_len >= 20 => Some(16),
                IP_PROTO_UDP if dw0 & CP_TX_UDPCS != 0 && data_len >= 8 => Some(6),
                _ => None,
            };
            if let Some(at) = sum_at {
                let saved = frame[ip..l4].to_vec();
                set_pseudo(frame, protocol, data_len);
                frame[l4 + at..l4 + at + 2].fill(0);
                let sum = ip_checksum(&frame[pseudo..l4 + data_len]);
                frame[l4 + at..l4 + at + 2].copy_from_slice(&sum);
                frame[ip..l4].copy_from_slice(&saved);
            }
            return false;
        }
        let mss = ((dw0 >> 16) & 0x7ff) as usize;
        if protocol != IP_PROTO_TCP || data_len < 20 || mss == 0 {
            return false;
        }
        let tcp_hlen = 4 * (frame[l4 + 12] >> 4) as usize;
        if tcp_hlen < 20 || tcp_hlen > data_len {
            return false;
        }
        let tcp_data_len = data_len - tcp_hlen;
        let flags = frame[l4 + 13];
        let base_id = be16(&saved_ip, 4);
        let mut offset = 0;
        while offset < tcp_data_len {
            let last = offset + mss >= tcp_data_len;
            let chunk = if last { tcp_data_len - offset } else { mss };
            let body = l4 + tcp_hlen;
            if offset > 0 {
                frame.copy_within(body + offset..body + offset + chunk, body);
            }
            // QEMU clears PSH and FIN for good on the first segment, so its
            // last segment loses them too; they belong on the last one.
            frame[l4 + 13] = if last { flags } else { flags & !(TCP_PSH | TCP_FIN) };
            set_pseudo(frame, IP_PROTO_TCP, tcp_hlen + chunk);
            frame[l4 + 16..l4 + 18].fill(0);
            let sum = ip_checksum(&frame[pseudo..body + chunk]);
            frame[l4 + 16..l4 + 18].copy_from_slice(&sum);
            frame[ip..l4].copy_from_slice(&saved_ip);
            frame[ip + 2..ip + 4].copy_from_slice(&((hlen + tcp_hlen + chunk) as u16).to_be_bytes());
            frame[ip + 4..ip + 6].copy_from_slice(&base_id.wrapping_add((offset / mss) as u16).to_be_bytes());
            set_ip_checksum(frame);
            let segment = frame[..body + chunk].to_vec();
            self.transfer_tagged(&segment, tag, dma);
            let seq = u32::from_be_bytes(frame[l4 + 4..l4 + 8].try_into().unwrap());
            frame[l4 + 4..l4 + 8].copy_from_slice(&seq.wrapping_add(chunk as u32).to_be_bytes());
            offset += mss;
        }
        true
    }

    fn write_tx_status(&mut self, offset: u8, val: u32, dma: &mut dyn DmaMemory) {
        let descriptor = (offset / 4) as usize;
        if self.cplus_enabled {
            self.tx_status[descriptor] = val;
            if descriptor == 0 && val & DTCCR_CMD != 0 {
                let addr = addr64(self.tx_status[0] & !0x3f, self.tx_status[1]);
                dma.write_phys(addr, &self.tally.bytes());
                self.tx_status[0] &= !DTCCR_CMD;
            }
            return;
        }
        let val = set_masked(val & !0xff00_c000, 0x00c0_0000, self.tx_status[descriptor]);
        self.tx_status[descriptor] = val;
        self.transmit(dma);
    }

    fn tsad(&self) -> u16 {
        let mut v = 0;
        for (i, &s) in self.tx_status.iter().enumerate() {
            let bit = |flag: u32, base: u32| if s & flag != 0 { 1 << (base + i as u32) } else { 0 };
            v |= bit(TX_STAT_OK, 12) | bit(TX_UNDERRUN, 8) | bit(TX_ABORTED, 4) | bit(TX_HOST_OWNS, 0);
        }
        v as u16
    }

    fn set_next_timer(&mut self, now: u64) {
        let period = PCI_PERIOD_NS << 32;
        while self.timer_base_ns + period <= now {
            self.timer_base_ns += period;
        }
        self.timer_deadline = (self.timer_int != 0).then(|| {
            let mut delta = self.timer_int as u64 * PCI_PERIOD_NS;
            if self.timer_base_ns + delta <= now {
                delta += period;
            }
            self.timer_base_ns + delta
        });
    }

    /// Reads a register. `size` is 1, 2 or 4.
    pub fn read(&mut self, addr: u8, size: u8, now: u64) -> u32 {
        self.stats.reads += 1;
        match size {
            1 => self.readb(addr) as u32,
            2 => self.readw(addr),
            _ => self.readl(addr, now),
        }
    }

    /// Writes a register. `size` is 1, 2 or 4.
    pub fn write(&mut self, addr: u8, size: u8, val: u32, now: u64, dma: &mut dyn DmaMemory) {
        self.stats.writes += 1;
        match size {
            1 => self.writeb(addr, val as u8, dma),
            2 => self.writew(addr, val as u16, now, dma),
            _ => self.writel(addr, val, now, dma),
        }
    }

    fn tx_reg_read(regs: &[u32; 4], base: u8, addr: u8, size: u32) -> u32 {
        if addr as u32 & (size - 1) != 0 {
            return 0;
        }
        let v = regs[((addr - base) / 4) as usize] >> (8 * (addr & 3));
        if size == 4 { v } else { v & ((1 << (8 * size)) - 1) }
    }

    fn readb(&mut self, addr: u8) -> u8 {
        match addr {
            0x00..=0x05 => self.phys[addr as usize],
            0x06..=0x07 => 0,
            0x08..=0x0f => self.mult[(addr - reg::MAR0) as usize],
            0x10..=0x1f => Self::tx_reg_read(&self.tx_status, reg::TX_STATUS0, addr, 1) as u8,
            reg::CHIP_CMD => self.chip_cmd | if self.rx_buffer_empty() { RX_BUF_EMPTY } else { 0 },
            reg::CFG9346 => {
                let mut v = self.cfg9346;
                if v & CFG9346_OP_MASK == CFG9346_EEPROM {
                    v = (v & !1) | self.eeprom.data_out() as u8;
                }
                v
            }
            reg::CONFIG0 => self.config0,
            reg::CONFIG1 => self.config1,
            reg::CONFIG3 => self.config3,
            reg::CONFIG4 => self.config4,
            reg::CONFIG5 => self.config5,
            reg::MEDIA_STATUS => 0xd0 | (!self.bmsr & BMSR_LINK_UP) as u8,
            reg::HLT_CLK => self.clock_enabled as u8,
            reg::PCI_REVISION_ID => PCI_REVISION,
            reg::TX_THRESH => self.tx_thresh,
            0x43 => (self.tx_config >> 24) as u8,
            _ => 0,
        }
    }

    fn readw(&mut self, addr: u8) -> u32 {
        (match addr {
            0x20..=0x2f => Self::tx_reg_read(&self.tx_addr, reg::TX_ADDR0, addr, 2) as u16,
            reg::INTR_MASK => self.intr_mask,
            reg::INTR_STATUS => self.intr_status,
            reg::MULTI_INTR => self.multi_intr,
            reg::RX_BUF_PTR => self.rx_buf_ptr.wrapping_sub(0x10) as u16,
            reg::RX_BUF_ADDR => self.rx_buf_addr as u16,
            reg::BASIC_MODE_CTRL => self.bmcr,
            reg::BASIC_MODE_STATUS => self.bmsr,
            reg::NWAY_ADVERT => self.nway_advert,
            reg::NWAY_LPAR => self.nway_lpar,
            reg::NWAY_EXPANSION => self.nway_expansion,
            reg::CP_CMD => self.cp_cmd,
            reg::INTR_MITIGATE => 0,
            reg::TX_SUMMARY => self.tsad(),
            reg::CSCR => self.cscr,
            _ => u16::from_le_bytes([self.readb(addr), self.readb(addr.wrapping_add(1))]),
        }) as u32
    }

    fn readl(&mut self, addr: u8, now: u64) -> u32 {
        match addr {
            reg::RX_MISSED => self.rx_missed,
            reg::TX_CONFIG => self.tx_config,
            reg::RX_CONFIG => self.rx_config,
            0x10..=0x1f => Self::tx_reg_read(&self.tx_status, reg::TX_STATUS0, addr, 4),
            0x20..=0x2f => self.tx_addr[((addr - reg::TX_ADDR0) / 4) as usize],
            reg::RX_BUF => self.rx_buf,
            reg::RX_RING_ADDR_LO => self.rx_ring_lo,
            reg::RX_RING_ADDR_HI => self.rx_ring_hi,
            reg::TIMER => (now.saturating_sub(self.timer_base_ns) / PCI_PERIOD_NS) as u32,
            reg::FLASH_REG => self.timer_int,
            _ => u32::from_le_bytes([0, 1, 2, 3].map(|i| self.readb(addr.wrapping_add(i)))),
        }
    }

    fn writeb(&mut self, addr: u8, val: u8, dma: &mut dyn DmaMemory) {
        let masked = |mask: u32, current: u8| set_masked(val as u32, mask, current as u32) as u8;
        match addr {
            0x00..=0x05 => self.phys[addr as usize] = val,
            0x08..=0x0f => self.mult[(addr - reg::MAR0) as usize] = val,
            reg::CHIP_CMD => self.write_chip_cmd(val),
            reg::CFG9346 => self.write_cfg9346(val),
            reg::TX_CONFIG => self.write_tx_config((self.tx_config & !0xff) | val as u32),
            reg::CONFIG0 if self.config_writable() => self.config0 = masked(0xf8, self.config0),
            reg::CONFIG1 if self.config_writable() => self.config1 = masked(0x0c, self.config1),
            reg::CONFIG3 if self.config_writable() => self.config3 = masked(0x8f, self.config3),
            reg::CONFIG4 if self.config_writable() => self.config4 = masked(0x0a, self.config4),
            reg::CONFIG5 => self.config5 = masked(0x80, self.config5),
            reg::HLT_CLK => match val {
                b'R' => self.clock_enabled = true,
                b'H' => self.clock_enabled = false,
                _ => {}
            },
            reg::TX_THRESH => self.tx_thresh = val,
            // Bit 6 polls the normal-priority ring; QEMU ignores the high-priority one.
            reg::TX_POLL if val & 0x40 != 0 => self.cplus_transmit(dma),
            reg::RX_CONFIG => self.write_rx_config((self.rx_config & !0xff) | val as u32),
            _ => {}
        }
    }

    fn writew(&mut self, addr: u8, val: u16, now: u64, dma: &mut dyn DmaMemory) {
        let masked = |mask: u32, current: u16| set_masked(val as u32, mask, current as u32) as u16;
        match addr {
            reg::INTR_MASK => self.intr_mask = masked(0x1e00, self.intr_mask),
            reg::INTR_STATUS => {
                let status = self.intr_status & !val;
                self.intr_status = set_masked(status as u32, 0x1e00, self.intr_status as u32) as u16;
                self.set_next_timer(now);
            }
            reg::MULTI_INTR => self.multi_intr = masked(0xf000, self.multi_intr),
            reg::RX_BUF_PTR => {
                self.rx_buf_ptr = (val as u32 + 0x10) % self.rx_buf_size;
                self.receive_pending(dma);
            }
            reg::BASIC_MODE_CTRL => {
                if val & 0x8000 != 0 {
                    self.reset_phy();
                }
                // Speed, duplex and autonegotiation stay read-only, as in QEMU.
                self.bmcr = masked(0xccff | 0x3000 | 0x0100, self.bmcr);
            }
            reg::BASIC_MODE_STATUS => self.bmsr = masked(0xff3f, self.bmsr),
            reg::NWAY_ADVERT => self.nway_advert = val,
            reg::NWAY_LPAR | reg::INTR_MITIGATE => {}
            reg::NWAY_EXPANSION => self.nway_expansion = val,
            reg::CP_CMD => {
                self.cplus_enabled = true;
                self.cp_cmd = masked(0xff84, self.cp_cmd);
            }
            _ => {
                self.writeb(addr, val as u8, dma);
                self.writeb(addr.wrapping_add(1), (val >> 8) as u8, dma);
            }
        }
    }

    fn writel(&mut self, addr: u8, val: u32, now: u64, dma: &mut dyn DmaMemory) {
        match addr {
            reg::RX_MISSED => self.rx_missed = 0,
            reg::TX_CONFIG => self.write_tx_config(val),
            reg::RX_CONFIG => self.write_rx_config(val),
            0x10..=0x1f => self.write_tx_status(addr - reg::TX_STATUS0, val, dma),
            0x20..=0x2f => self.tx_addr[((addr - reg::TX_ADDR0) / 4) as usize] = val,
            reg::RX_BUF => self.rx_buf = val,
            reg::RX_RING_ADDR_LO => self.rx_ring_lo = val,
            reg::RX_RING_ADDR_HI => self.rx_ring_hi = val,
            reg::TIMER => {
                self.timer_base_ns = now;
                self.set_next_timer(now);
            }
            reg::FLASH_REG => {
                if self.timer_int != val {
                    self.timer_int = val;
                    self.set_next_timer(now);
                }
            }
            _ => {
                for (i, b) in val.to_le_bytes().into_iter().enumerate() {
                    self.writeb(addr.wrapping_add(i as u8), b, dma);
                }
            }
        }
    }

    fn write_chip_cmd(&mut self, val: u8) {
        if val & CMD_RESET != 0 {
            self.reset();
        }
        self.chip_cmd = set_masked(val as u32, 0xe3, self.chip_cmd as u32) as u8 & !CMD_RESET;
    }

    fn write_cfg9346(&mut self, val: u8) {
        let mut val = set_masked(val as u32, 0x31, self.cfg9346 as u32) as u8;
        match val & CFG9346_OP_MASK {
            CFG9346_EEPROM => self.eeprom.set_wires(val & 0x08 != 0, val & 0x04 != 0, val & 0x02 != 0),
            CFG9346_RESET => {
                val = 0;
                self.reset();
            }
            _ => {}
        }
        self.cfg9346 = val;
    }

    fn write_tx_config(&mut self, val: u32) {
        if self.transmitter_enabled() {
            self.tx_config = set_masked(val, TX_VERSION_MASK | 0x8070_f80f, self.tx_config);
        }
    }

    fn write_rx_config(&mut self, val: u32) {
        self.rx_config = set_masked(val, 0xf0fc_0040, self.rx_config);
        self.reset_rx_ring(8192 << ((self.rx_config >> 11) & 3));
    }
}

#[cfg(test)]
mod tests;

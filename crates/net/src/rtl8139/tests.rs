use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::*;
use crate::DEFAULT_MAC;

#[derive(Default)]
struct Wire {
    sent: Vec<Vec<u8>>,
    incoming: VecDeque<Vec<u8>>,
}

#[derive(Clone, Default)]
struct FakeBackend(Arc<Mutex<Wire>>);

impl Backend for FakeBackend {
    fn send(&mut self, frame: &[u8]) {
        self.0.lock().unwrap().sent.push(frame.to_vec());
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        self.0.lock().unwrap().incoming.pop_front()
    }
}

struct Ram(Vec<u8>);

impl DmaMemory for Ram {
    fn read_phys(&mut self, addr: u64, buf: &mut [u8]) {
        let a = addr as usize;
        buf.copy_from_slice(&self.0[a..a + buf.len()]);
    }

    fn write_phys(&mut self, addr: u64, buf: &[u8]) {
        let a = addr as usize;
        self.0[a..a + buf.len()].copy_from_slice(buf);
    }
}

struct Rig {
    nic: Rtl8139,
    ram: Ram,
    wire: FakeBackend,
    now: u64,
}

const RX_RING: u32 = 0x1_0000;
const OTHER_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc];

impl Rig {
    fn new() -> Rig {
        let wire = FakeBackend::default();
        Rig { nic: Rtl8139::new(DEFAULT_MAC, Box::new(wire.clone())), ram: Ram(vec![0; 1 << 20]), wire, now: 0 }
    }

    fn r8(&mut self, addr: u8) -> u8 {
        self.nic.read(addr, 1, self.now) as u8
    }

    fn r16(&mut self, addr: u8) -> u16 {
        self.nic.read(addr, 2, self.now) as u16
    }

    fn r32(&mut self, addr: u8) -> u32 {
        self.nic.read(addr, 4, self.now)
    }

    fn w8(&mut self, addr: u8, v: u8) {
        self.nic.write(addr, 1, v as u32, self.now, &mut self.ram);
    }

    fn w16(&mut self, addr: u8, v: u16) {
        self.nic.write(addr, 2, v as u32, self.now, &mut self.ram);
    }

    fn w32(&mut self, addr: u8, v: u32) {
        self.nic.write(addr, 4, v, self.now, &mut self.ram);
    }

    /// Sets the card up as Linux's 8139too does: a 32 KiB ring with
    /// RxNoWrap, our address and broadcasts.
    fn start_ring_rx(&mut self, rcr: u32) {
        self.w8(reg::CHIP_CMD, CMD_RESET);
        self.w32(reg::RX_BUF, RX_RING);
        self.w8(reg::CHIP_CMD, CMD_RX_ENB | CMD_TX_ENB);
        self.w32(reg::RX_CONFIG, rcr);
        self.w16(reg::INTR_MASK, 0xffff);
    }

    fn deliver(&mut self, frame: Vec<u8>) {
        self.wire.0.lock().unwrap().incoming.push_back(frame);
        self.nic.poll(self.now, &mut self.ram);
    }

    fn ring(&self, off: u32, len: usize) -> &[u8] {
        let a = (RX_RING + off) as usize;
        &self.ram.0[a..a + len]
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        self.wire.0.lock().unwrap().sent.clone()
    }
}

fn frame_to(dst: [u8; 6], len: usize) -> Vec<u8> {
    let mut f = dst.to_vec();
    f.extend_from_slice(&OTHER_MAC);
    f.extend_from_slice(&[0x08, 0x00]);
    f.extend((0..len - 14).map(|i| i as u8));
    f
}

const RCR_32K: u32 = 2 << 11;
const RCR_ACCEPT: u32 = ACCEPT_MY_PHYS | ACCEPT_BROADCAST;

#[test]
fn id_registers_hold_the_mac_after_reset() {
    let mut r = Rig::new();
    r.w8(0, 0x02);
    r.w8(reg::CHIP_CMD, CMD_RESET);
    let mac: Vec<u8> = (0..6).map(|i| r.r8(i)).collect();
    assert_eq!(mac, DEFAULT_MAC);
}

#[test]
fn identifies_as_an_8139c_plus() {
    let mut r = Rig::new();
    assert_eq!(r.r8(reg::PCI_REVISION_ID), 0x20);
    assert_eq!(r.r32(reg::TX_CONFIG) & TX_VERSION_MASK, 0x7480_0000);
}

#[test]
fn reset_bit_clears_itself() {
    let mut r = Rig::new();
    r.w8(reg::CHIP_CMD, CMD_RESET);
    assert_eq!(r.r8(reg::CHIP_CMD) & CMD_RESET, 0);
    assert_eq!(r.r8(reg::CHIP_CMD) & RX_BUF_EMPTY, RX_BUF_EMPTY);
}

#[test]
fn eeprom_reads_through_cfg9346_return_the_mac() {
    let mut r = Rig::new();
    let mut read_word = |addr: u16| {
        let cmd = addr | 6 << 6;
        r.w8(reg::CFG9346, 0x80);
        r.w8(reg::CFG9346, 0x88);
        for i in (0..=10).rev() {
            let di = if cmd & 1 << i != 0 { 0x02 } else { 0 };
            r.w8(reg::CFG9346, 0x88 | di);
            r.w8(reg::CFG9346, 0x88 | di | 0x04);
        }
        r.w8(reg::CFG9346, 0x88);
        let mut v = 0u16;
        for _ in 0..16 {
            r.w8(reg::CFG9346, 0x88 | 0x04);
            v = v << 1 | (r.r8(reg::CFG9346) & 1) as u16;
            r.w8(reg::CFG9346, 0x88);
        }
        r.w8(reg::CFG9346, 0);
        v
    };
    assert_eq!(read_word(0), 0x8129);
    assert_eq!(read_word(1), PCI_VENDOR);
    let mac: Vec<u8> = (7..10).flat_map(|a| read_word(a).to_le_bytes()).collect();
    assert_eq!(mac, DEFAULT_MAC);
}

#[test]
fn config_registers_need_the_unlock_in_cfg9346() {
    let mut r = Rig::new();
    r.w8(reg::CONFIG1, 0x20);
    assert_eq!(r.r8(reg::CONFIG1), 0x0c);
    r.w8(reg::CFG9346, 0xc0);
    r.w8(reg::CONFIG1, 0x20);
    assert_eq!(r.r8(reg::CONFIG1), 0x2c);
}

#[test]
fn interrupt_line_follows_status_and_mask() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    r.w16(reg::INTR_MASK, intr::TX_OK);
    r.deliver(frame_to(DEFAULT_MAC, 64));
    assert_eq!(r.r16(reg::INTR_STATUS), intr::RX_OK);
    assert!(!r.nic.irq(), "RxOK is masked");
    r.w16(reg::INTR_MASK, intr::RX_OK);
    assert!(r.nic.irq());
}

#[test]
fn writing_ones_to_intr_status_acknowledges_only_those_bits() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    r.deliver(frame_to(DEFAULT_MAC, 64));
    r.w32(reg::TX_ADDR0, 0x2000);
    r.w32(reg::TX_STATUS0, 64);
    assert_eq!(r.r16(reg::INTR_STATUS), intr::RX_OK | intr::TX_OK);
    r.w16(reg::INTR_STATUS, intr::TX_OK);
    assert_eq!(r.r16(reg::INTR_STATUS), intr::RX_OK);
    r.w16(reg::INTR_STATUS, intr::RX_OK);
    assert!(!r.nic.irq());
}

#[test]
fn transmits_a_descriptor_and_hands_it_back() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    let frame = frame_to(OTHER_MAC, 100);
    r.ram.0[0x2000..0x2000 + 100].copy_from_slice(&frame);
    r.w32(reg::TX_ADDR0, 0x2000);
    assert_eq!(r.r16(reg::TX_SUMMARY) & 1, 1, "descriptor 0 starts out owned by the driver");
    r.w32(reg::TX_STATUS0, 100);
    assert_eq!(r.sent(), vec![frame]);
    let status = r.r32(reg::TX_STATUS0);
    assert_eq!(status & (TX_HOST_OWNS | TX_STAT_OK), TX_HOST_OWNS | TX_STAT_OK);
    assert_eq!(r.r16(reg::TX_SUMMARY) & (1 << 12 | 1), 1 << 12 | 1);
    assert_eq!(r.r16(reg::INTR_STATUS) & intr::TX_OK, intr::TX_OK);
}

#[test]
fn transmit_descriptors_are_used_in_turn() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    for d in 0..4u8 {
        let base = 0x2000 + 0x800 * d as usize;
        r.ram.0[base..base + 60].copy_from_slice(&frame_to([d; 6], 60));
        r.w32(reg::TX_ADDR0 + 4 * d, base as u32);
    }
    r.w32(reg::TX_STATUS0 + 4, 60);
    assert!(r.sent().is_empty(), "the card waits for descriptor 0");
    r.w32(reg::TX_STATUS0, 60);
    r.w32(reg::TX_STATUS0 + 4, 60);
    r.w32(reg::TX_STATUS0 + 8, 60);
    let firsts: Vec<u8> = r.sent().iter().map(|f| f[0]).collect();
    assert_eq!(firsts, [0, 1, 2]);
}

#[test]
fn transmitter_off_sends_nothing() {
    let mut r = Rig::new();
    r.w32(reg::TX_ADDR0, 0x2000);
    r.w32(reg::TX_STATUS0, 60);
    assert!(r.sent().is_empty());
}

#[test]
fn received_frame_lands_in_the_ring_with_header_and_crc() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    let frame = frame_to(DEFAULT_MAC, 100);
    r.deliver(frame.clone());
    let header = u32::from_le_bytes(r.ring(0, 4).try_into().unwrap());
    assert_eq!(header & 0xffff, RX_STATUS_OK | RX_PHYSICAL);
    assert_eq!(header >> 16, 104);
    assert_eq!(r.ring(4, 100), &frame[..]);
    assert_eq!(r.ring(104, 4), crc32fast::hash(&frame).to_le_bytes());
    assert_eq!(r.r16(reg::RX_BUF_ADDR), 108);
    assert_eq!(r.r8(reg::CHIP_CMD) & RX_BUF_EMPTY, 0);
    assert_eq!(r.nic.stats.rx_frames, 1);
}

#[test]
fn ring_stays_empty_after_the_guest_catches_up() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    r.deliver(frame_to(DEFAULT_MAC, 101));
    let cbr = r.r16(reg::RX_BUF_ADDR);
    assert_eq!(cbr % 4, 0, "the write pointer stays dword aligned");
    r.w16(reg::RX_BUF_PTR, cbr.wrapping_sub(0x10));
    assert_eq!(r.r8(reg::CHIP_CMD) & RX_BUF_EMPTY, RX_BUF_EMPTY);
    assert_eq!(r.r16(reg::RX_BUF_PTR), cbr.wrapping_sub(0x10));
}

#[test]
fn short_frames_are_padded_to_the_ethernet_minimum() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    r.deliver(frame_to(DEFAULT_MAC, 42));
    let header = u32::from_le_bytes(r.ring(0, 4).try_into().unwrap());
    assert_eq!(header >> 16, 64);
}

#[test]
fn address_filter_drops_frames_for_other_cards() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    r.deliver(frame_to(OTHER_MAC, 64));
    assert_eq!(r.r16(reg::RX_BUF_ADDR), 0);
    assert_eq!(r.r16(reg::INTR_STATUS), 0);
}

#[test]
fn broadcasts_are_flagged_as_such() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    r.deliver(frame_to([0xff; 6], 64));
    let header = u32::from_le_bytes(r.ring(0, 4).try_into().unwrap());
    assert_eq!(header & 0xffff, RX_STATUS_OK | RX_BROADCAST);
}

#[test]
fn promiscuous_mode_takes_everything() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | ACCEPT_ALL_PHYS);
    r.deliver(frame_to(OTHER_MAC, 64));
    assert_ne!(r.r16(reg::RX_BUF_ADDR), 0);
}

#[test]
fn multicast_needs_its_hash_bit() {
    let group = [0x01, 0x00, 0x5e, 0x00, 0x00, 0x01];
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT | ACCEPT_MULTICAST);
    r.deliver(frame_to(group, 64));
    assert_eq!(r.r16(reg::RX_BUF_ADDR), 0);
    let h = multicast_hash(&group);
    r.w8(reg::MAR0 + (h >> 3) as u8, 1 << (h & 7));
    r.deliver(frame_to(group, 64));
    let header = u32::from_le_bytes(r.ring(0, 4).try_into().unwrap());
    assert_eq!(header & 0xffff, RX_STATUS_OK | RX_MULTICAST);
}

#[test]
fn frames_wrap_to_the_start_of_the_ring() {
    let mut r = Rig::new();
    r.start_ring_rx(0);
    r.w32(reg::RX_CONFIG, RCR_ACCEPT);
    let frame = frame_to(DEFAULT_MAC, 1000);
    for _ in 0..9 {
        r.deliver(frame.clone());
        let cbr = r.r16(reg::RX_BUF_ADDR);
        r.w16(reg::RX_BUF_PTR, cbr.wrapping_sub(0x10));
    }
    let start = 8 * 1008 % 8192;
    assert_eq!(start, 8064);
    let header = u32::from_le_bytes(r.ring(start, 4).try_into().unwrap());
    assert_eq!(header >> 16, 1004);
    let mut copy = r.ring(start + 4, (8192 - start - 4) as usize).to_vec();
    copy.extend_from_slice(r.ring(0, 1000 - copy.len()));
    assert_eq!(copy, frame);
}

#[test]
fn a_full_ring_holds_frames_back_until_the_guest_reads() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_ACCEPT);
    r.w16(reg::INTR_MASK, intr::RX_OK);
    let frame = frame_to(DEFAULT_MAC, 1500);
    for _ in 0..6 {
        r.deliver(frame.clone());
    }
    assert_eq!(r.nic.stats.rx_frames, 5);
    let cbr = r.r16(reg::RX_BUF_ADDR);
    r.w16(reg::RX_BUF_PTR, cbr.wrapping_sub(0x10));
    assert_eq!(r.nic.stats.rx_frames, 6, "the held frame goes in as soon as there is room");
    assert_eq!(r.nic.stats.rx_dropped, 0);
}

#[test]
fn loopback_mode_receives_its_own_frames() {
    let mut r = Rig::new();
    r.start_ring_rx(RCR_32K | RCR_ACCEPT);
    r.w32(reg::TX_CONFIG, TX_LOOPBACK);
    let frame = frame_to(DEFAULT_MAC, 80);
    r.ram.0[0x2000..0x2000 + 80].copy_from_slice(&frame);
    r.w32(reg::TX_ADDR0, 0x2000);
    r.w32(reg::TX_STATUS0, 80);
    assert!(r.sent().is_empty());
    assert_eq!(r.ring(4, 80), &frame[..]);
}

#[test]
fn timer_counts_the_pci_clock_and_interrupts_at_timer_int() {
    let mut r = Rig::new();
    r.w16(reg::INTR_MASK, intr::PCS_TIMEOUT);
    r.now = 1_000;
    r.w32(reg::TIMER, 0);
    r.now = 1_000 + 300;
    assert_eq!(r.r32(reg::TIMER), 10);
    r.w32(reg::FLASH_REG, 100);
    assert_eq!(r.nic.next_event_ns(), Some(1_000 + 3_000));
    r.now = 3_999;
    r.nic.poll(r.now, &mut r.ram);
    assert!(!r.nic.irq());
    r.now = 4_000;
    r.nic.poll(r.now, &mut r.ram);
    assert!(r.nic.irq());
}

#[test]
fn link_is_up_at_100_mbit_full_duplex() {
    let mut r = Rig::new();
    assert_eq!(r.r8(reg::MEDIA_STATUS) & 0x04, 0, "LINKB is active low");
    assert_eq!(r.r16(reg::BASIC_MODE_STATUS) & BMSR_LINK_UP, BMSR_LINK_UP);
    assert_eq!(r.r16(reg::NWAY_LPAR), 0x05e1);
}

const TX_RING: u32 = 0x3000;
const RX_DESCS: u32 = 0x4000;

impl Rig {
    /// Sets up C+ mode the way Linux's 8139cp does.
    fn start_cplus(&mut self) {
        self.w8(reg::CHIP_CMD, CMD_RESET);
        self.w16(reg::CP_CMD, CP_RX_ENB | CP_TX_ENB);
        self.w32(reg::RX_RING_ADDR_LO, RX_DESCS);
        self.w32(reg::TX_ADDR0, TX_RING);
        self.w32(reg::TX_ADDR0 + 4, 0);
        self.w8(reg::CHIP_CMD, CMD_RX_ENB | CMD_TX_ENB);
        self.w32(reg::RX_CONFIG, RCR_ACCEPT);
        self.w16(reg::INTR_MASK, 0xffff);
    }

    fn put_desc(&mut self, at: u32, dw0: u32, dw1: u32, buf: u32) {
        let a = at as usize;
        for (i, v) in [dw0, dw1, buf, 0].into_iter().enumerate() {
            self.ram.0[a + 4 * i..a + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
    }

    fn desc_dw(&self, at: u32, i: usize) -> u32 {
        let a = at as usize + 4 * i;
        u32::from_le_bytes(self.ram.0[a..a + 4].try_into().unwrap())
    }

    fn queue_tx(&mut self, index: u32, flags: u32, dw1: u32, data: &[u8]) {
        let buf = 0x8000 + 0x1_0000 * index;
        self.ram.0[buf as usize..buf as usize + data.len()].copy_from_slice(data);
        self.put_desc(TX_RING + 16 * index, CP_OWN | flags | data.len() as u32, dw1, buf);
    }
}

fn ones_sum(data: &[u8]) -> u16 {
    let mut sum: u32 = data.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32).sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

/// The TCP or UDP checksum, folded with the pseudo-header: 0xffff when valid.
fn l4_sum(packet: &[u8]) -> u16 {
    let ip = &packet[14..];
    let hlen = 4 * (ip[0] & 0xf) as usize;
    let len = u16::from_be_bytes([ip[2], ip[3]]) as usize - hlen;
    let mut pseudo = ip[12..20].to_vec();
    pseudo.extend_from_slice(&[0, ip[9]]);
    pseudo.extend_from_slice(&(len as u16).to_be_bytes());
    pseudo.extend_from_slice(&ip[hlen..hlen + len]);
    ones_sum(&pseudo)
}

fn ipv4_packet(protocol: u8, l4: &[u8]) -> Vec<u8> {
    let mut f = frame_to(OTHER_MAC, 14);
    let total = 20 + l4.len();
    f.extend_from_slice(&[0x45, 0, (total >> 8) as u8, total as u8, 0x12, 0x34, 0x40, 0, 64, protocol, 0, 0]);
    f.extend_from_slice(&[10, 0, 2, 15, 93, 184, 216, 34]);
    f.extend_from_slice(l4);
    f
}

fn tcp_segment(payload: usize, flags: u8) -> Vec<u8> {
    let mut tcp = vec![0x04, 0x00, 0x00, 0x50, 0, 0, 0x10, 0, 0, 0, 0, 0, 0x50, flags, 0xff, 0xff, 0, 0, 0, 0];
    tcp.extend((0..payload).map(|i| (i % 251) as u8));
    ipv4_packet(IP_PROTO_TCP, &tcp)
}

#[test]
fn cplus_sends_a_frame_from_one_descriptor() {
    let mut r = Rig::new();
    r.start_cplus();
    let frame = frame_to(OTHER_MAC, 200);
    r.queue_tx(0, CP_FS | CP_LS, 0, &frame);
    r.w8(reg::TX_POLL, 0x40);
    assert_eq!(r.sent(), vec![frame]);
    assert_eq!(r.desc_dw(TX_RING, 0) & CP_OWN, 0, "the descriptor goes back to the driver");
    assert_eq!(r.r16(reg::INTR_STATUS) & intr::TX_OK, intr::TX_OK);
}

#[test]
fn cplus_sent_descriptor_reports_no_collisions() {
    let mut r = Rig::new();
    r.start_cplus();
    r.queue_tx(0, CP_FS | CP_LS | CP_TX_IPCS | CP_TX_TCPCS, 0, &tcp_segment(10, 0x18));
    r.w8(reg::TX_POLL, 0x40);
    assert_eq!(r.desc_dw(TX_RING, 0) >> 16 & 0xf, 0, "8139cp adds these bits to its collision count");
}

#[test]
fn cplus_gathers_a_frame_from_several_descriptors() {
    let mut r = Rig::new();
    r.start_cplus();
    let frame = frame_to(OTHER_MAC, 300);
    r.queue_tx(0, CP_FS, 0, &frame[..100]);
    r.queue_tx(1, 0, 0, &frame[100..250]);
    r.queue_tx(2, CP_LS, 0, &frame[250..]);
    r.w8(reg::TX_POLL, 0x40);
    assert_eq!(r.sent(), vec![frame]);
}

#[test]
fn cplus_transmit_stops_at_a_descriptor_the_driver_owns() {
    let mut r = Rig::new();
    r.start_cplus();
    r.queue_tx(0, CP_FS | CP_LS, 0, &frame_to(OTHER_MAC, 60));
    r.w8(reg::TX_POLL, 0x40);
    r.queue_tx(1, CP_FS | CP_LS | CP_EOR, 0, &frame_to(OTHER_MAC, 61));
    r.queue_tx(0, CP_FS | CP_LS, 0, &frame_to(OTHER_MAC, 62));
    r.w8(reg::TX_POLL, 0x40);
    let lens: Vec<usize> = r.sent().iter().map(Vec::len).collect();
    assert_eq!(lens, [60, 61, 62], "end of ring goes back to descriptor 0");
}

#[test]
fn cplus_receive_fills_the_next_descriptor() {
    let mut r = Rig::new();
    r.start_cplus();
    r.put_desc(RX_DESCS, CP_OWN | 1536, 0, 0x2_0000);
    r.put_desc(RX_DESCS + 16, CP_OWN | CP_EOR | 1536, 0, 0x2_1000);
    for len in [100, 200, 300] {
        r.deliver(frame_to(DEFAULT_MAC, len));
        if len == 200 {
            r.put_desc(RX_DESCS, CP_OWN | 1536, 0, 0x2_0000);
        }
    }
    let dw0 = r.desc_dw(RX_DESCS + 16, 0);
    assert_eq!(dw0 & (CP_OWN | CP_FS | CP_LS | CP_RX_PAM | CP_EOR), CP_FS | CP_LS | CP_RX_PAM | CP_EOR);
    assert_eq!(dw0 & CP_RX_SIZE_MASK, 204);
    assert_eq!(&r.ram.0[0x2_1000..0x2_1000 + 200], &frame_to(DEFAULT_MAC, 200)[..]);
    assert_eq!(r.desc_dw(RX_DESCS, 0) & CP_RX_SIZE_MASK, 304, "end of ring wraps to descriptor 0");
    assert_eq!(r.r16(reg::INTR_STATUS) & intr::RX_OK, intr::RX_OK);
}

#[test]
fn cplus_receive_waits_for_the_driver_to_return_a_descriptor() {
    let mut r = Rig::new();
    r.start_cplus();
    r.put_desc(RX_DESCS, CP_EOR | 1536, 0, 0x2_0000);
    r.deliver(frame_to(DEFAULT_MAC, 100));
    assert_eq!(r.nic.stats.rx_frames, 0);
    r.put_desc(RX_DESCS, CP_OWN | CP_EOR | 1536, 0, 0x2_0000);
    r.nic.poll(r.now, &mut r.ram);
    assert_eq!(r.nic.stats.rx_frames, 1);
    assert_eq!(r.desc_dw(RX_DESCS, 0) & CP_RX_SIZE_MASK, 104);
}

#[test]
fn cplus_drops_frames_too_big_for_the_buffer() {
    let mut r = Rig::new();
    r.start_cplus();
    r.put_desc(RX_DESCS, CP_OWN | CP_EOR | 100, 0, 0x2_0000);
    r.deliver(frame_to(DEFAULT_MAC, 100));
    assert_eq!(r.desc_dw(RX_DESCS, 0) & CP_OWN, CP_OWN);
    assert_eq!(r.r16(reg::INTR_STATUS) & intr::RX_OVERFLOW, intr::RX_OVERFLOW);
    assert_eq!(r.r32(reg::RX_MISSED), 1);
}

#[test]
fn checksum_offload_fills_in_ip_and_udp_checksums() {
    let mut r = Rig::new();
    r.start_cplus();
    let udp = [0x00, 0x44, 0x00, 0x43, 0x00, 0x0c, 0, 0, 1, 2, 3, 4];
    let packet = ipv4_packet(IP_PROTO_UDP, &udp);
    r.queue_tx(0, CP_FS | CP_LS | CP_TX_IPCS | CP_TX_UDPCS, 0, &packet);
    r.w8(reg::TX_POLL, 0x40);
    let sent = &r.sent()[0];
    assert_eq!(ones_sum(&sent[14..34]), 0xffff);
    assert_eq!(l4_sum(sent), 0xffff);
    assert_eq!(sent[..24], packet[..24], "only the checksum fields change");
    assert_eq!(sent[26..40], packet[26..40]);
}

#[test]
fn checksum_offload_fills_in_the_tcp_checksum() {
    let mut r = Rig::new();
    r.start_cplus();
    r.queue_tx(0, CP_FS | CP_LS | CP_TX_TCPCS, 0, &tcp_segment(33, 0x18));
    r.w8(reg::TX_POLL, 0x40);
    assert_eq!(l4_sum(&r.sent()[0]), 0xffff);
}

#[test]
fn large_send_splits_tcp_into_mss_segments() {
    let mut r = Rig::new();
    r.start_cplus();
    let big = tcp_segment(2500, 0x18 | 0x01);
    r.queue_tx(0, CP_FS | CP_LS | CP_TX_LGSEN | 1000 << 16, 0, &big);
    r.w8(reg::TX_POLL, 0x40);
    let sent = r.sent();
    assert_eq!(sent.iter().map(Vec::len).collect::<Vec<_>>(), [1054, 1054, 554]);
    for (i, s) in sent.iter().enumerate() {
        assert_eq!(ones_sum(&s[14..34]), 0xffff, "IP checksum of segment {i}");
        assert_eq!(l4_sum(s), 0xffff, "TCP checksum of segment {i}");
        assert_eq!(u16::from_be_bytes([s[18], s[19]]), 0x1234 + i as u16);
        assert_eq!(u32::from_be_bytes(s[38..42].try_into().unwrap()), 0x1000 + 1000 * i as u32);
        assert_eq!(&s[54..], &big[54 + 1000 * i..54 + 1000 * i + s.len() - 54]);
    }
    let flags: Vec<u8> = sent.iter().map(|s| s[47]).collect();
    assert_eq!(flags, [0x10, 0x10, 0x19], "PSH and FIN only on the last segment");
}

#[test]
fn vlan_tag_is_inserted_on_send_and_stripped_on_receive() {
    let mut r = Rig::new();
    r.start_cplus();
    let frame = frame_to(OTHER_MAC, 64);
    r.queue_tx(0, CP_FS | CP_LS, CP_TX_TAGC | 0x2a00, &frame);
    r.w8(reg::TX_POLL, 0x40);
    let sent = &r.sent()[0];
    assert_eq!(sent[12..16], [0x81, 0x00, 0x00, 0x2a]);
    assert_eq!(sent[16..], frame[12..]);

    r.w16(reg::CP_CMD, CP_RX_ENB | CP_TX_ENB | CP_RX_VLAN);
    r.put_desc(RX_DESCS, CP_OWN | CP_EOR | 1536, 0, 0x2_0000);
    let mut tagged = frame_to(DEFAULT_MAC, 64);
    tagged.splice(12..12, [0x81, 0x00, 0x00, 0x2a]);
    r.deliver(tagged);
    assert_eq!(r.desc_dw(RX_DESCS, 1), CP_RX_TAVA | 0x2a00);
    assert_eq!(&r.ram.0[0x2_0000..0x2_0000 + 64], &frame_to(DEFAULT_MAC, 64)[..]);
}

#[test]
fn tally_counters_are_dumped_on_request() {
    let mut r = Rig::new();
    r.start_cplus();
    r.put_desc(RX_DESCS, CP_OWN | CP_EOR | 1536, 0, 0x2_0000);
    r.deliver(frame_to(DEFAULT_MAC, 100));
    r.queue_tx(0, CP_FS | CP_LS, 0, &frame_to(OTHER_MAC, 60));
    r.w8(reg::TX_POLL, 0x40);
    r.w32(reg::TX_STATUS0 + 4, 0);
    r.w32(reg::TX_STATUS0, 0x5000 | DTCCR_CMD);
    assert_eq!(r.r32(reg::TX_STATUS0) & DTCCR_CMD, 0, "the command bit clears when the dump is done");
    let at = |off: usize| u64::from_le_bytes(r.ram.0[0x5000 + off..0x5000 + off + 8].try_into().unwrap());
    assert_eq!((at(0), at(8), at(40)), (1, 1, 1));
}

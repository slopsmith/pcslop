mod common;

use common::*;
use ide::{Ide, MemBackend};

fn pc_with_disk(sectors: usize) -> Pc {
    let mut pc = Pc::new();
    pc.ide.attach_disk(0, 0, pattern_disk(sectors));
    pc
}

fn expected(lba: u64, sectors: u64) -> Vec<u8> {
    (lba * 512..(lba + sectors) * 512).map(pattern).collect()
}

fn ascii(words: &[u16]) -> String {
    words.iter().flat_map(|w| w.to_be_bytes()).map(char::from).collect::<String>().trim_end().to_string()
}

#[test]
fn ports_outside_the_controller_are_not_claimed() {
    let mut ide = Ide::new();
    assert_eq!(ide.io_read(0x3f7, 1), None);
    assert_eq!(ide.io_read(0x1f8, 1), None);
    assert_eq!(ide.io_read(0xc000, 1), None);
}

#[test]
fn empty_channel_reads_as_pulled_down_bus() {
    let mut pc = Pc::new();
    for port in SEC + 1..SEC + 8 {
        assert_eq!(pc.inb(port), 0x7f, "port {port:#x}");
    }
    assert_eq!(pc.inb(SEC_CTL), 0x7f);
    pc.outb(SEC + 2, 0x55);
    assert_eq!(pc.inb(SEC + 2), 0x7f);
}

#[test]
fn absent_slave_reads_zero_status_but_latches_registers() {
    let mut pc = pc_with_disk(64);
    pc.select(PRI, true);
    assert_eq!(pc.status(PRI), 0);
    assert_eq!(pc.inb(PRI_CTL), 0);
    pc.outb(PRI + 2, 0x55);
    pc.outb(PRI + 3, 0xaa);
    assert_eq!((pc.inb(PRI + 2), pc.inb(PRI + 3), pc.inb(PRI + 6)), (0x55, 0xaa, 0xb0));
}

#[test]
fn commands_to_an_absent_slave_are_ignored() {
    let mut pc = pc_with_disk(64);
    pc.select(PRI, true);
    pc.outb(PRI + 7, 0xec);
    assert_eq!(pc.status(PRI), 0);
    assert!(!pc.ide.irq(0));
}

#[test]
fn identify_reports_geometry_capacity_and_modes() {
    let sectors = 16 * 63 * 20 + 5;
    let mut pc = pc_with_disk(sectors);
    let w = pc.identify(PRI, 0, 0xec);
    assert_eq!(w[0], 0x0040);
    assert_eq!((w[1], w[3], w[6]), (20, 16, 63));
    assert_eq!(w[60] as u32 | (w[61] as u32) << 16, sectors as u32);
    assert_eq!(w[100] as u64 | (w[101] as u64) << 16, sectors as u64);
    assert_ne!(w[49] & (1 << 9), 0, "LBA");
    assert_ne!(w[49] & (1 << 8), 0, "DMA");
    assert_ne!(w[83] & (1 << 10), 0, "LBA48");
    assert_eq!(w[63] & 0x07, 0x07, "MWDMA 0-2");
    assert_eq!(w[88] & 0x3f, 0x3f, "UDMA 0-5");
    assert_eq!(ascii(&w[27..47]), "86slopper HARDDISK");
    assert_eq!(w[93] & 0xdf61, 0x4041, "device 0 reports no device 1");
    assert_eq!(w[255] & 0xff, 0xa5);
    let sum = w.iter().fold(0u8, |s, &x| s.wrapping_add(x as u8).wrapping_add((x >> 8) as u8));
    assert_eq!(sum, 0, "integrity checksum");
    assert_eq!(pc.status(PRI) & 0x89, 0, "BSY, DRQ and ERR clear after the transfer");
}

#[test]
fn identify_of_a_big_disk_caps_chs_and_lba28_fields() {
    let mut pc = Pc::new();
    let len = (1u64 << 30) * 512;
    pc.ide.attach_disk(0, 0, Box::new(SparseDisk { len, written: Default::default() }));
    let w = pc.identify(PRI, 0, 0xec);
    assert_eq!((w[1], w[3], w[6]), (16383, 16, 63));
    assert_eq!(w[60] as u32 | (w[61] as u32) << 16, 0x0fff_ffff);
    let lba48 = (0..4).fold(0u64, |a, i| a | (w[100 + i] as u64) << (16 * i));
    assert_eq!(lba48, 1 << 30);
}

#[test]
fn identify_raises_and_status_read_clears_interrupt() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xa0);
    pc.outb(PRI + 7, 0xec);
    assert!(pc.ide.irq(0));
    pc.inb(PRI_CTL);
    assert!(pc.ide.irq(0), "alternate status must not clear INTRQ");
    pc.status(PRI);
    assert!(!pc.ide.irq(0));
}

#[test]
fn nien_masks_the_line_without_dropping_the_pending_interrupt() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI_CTL, 0x02);
    pc.outb(PRI + 6, 0xa0);
    pc.outb(PRI + 7, 0xe7);
    assert!(!pc.ide.irq(0));
    pc.outb(PRI_CTL, 0x00);
    assert!(pc.ide.irq(0));
}

#[test]
fn reads_sectors_with_16_bit_pio() {
    let mut pc = pc_with_disk(64);
    pc.lba28(PRI, 0, 5, 3, 0x20);
    let mut data = Vec::new();
    for _ in 0..3 {
        assert!(pc.ide.irq(0), "each DRQ block interrupts");
        assert_eq!(pc.status(PRI) & 0x89, 0x08);
        data.extend(pc.read_pio(PRI, 512, 2));
    }
    assert_eq!(data, expected(5, 3));
    assert_eq!(pc.status(PRI), 0x50);
}

#[test]
fn reads_sectors_with_32_bit_pio() {
    let mut pc = pc_with_disk(64);
    pc.lba28(PRI, 0, 9, 2, 0x20);
    let data = pc.read_pio(PRI, 1024, 4);
    assert_eq!(data, expected(9, 2));
    assert_eq!(pc.status(PRI) & 0x08, 0);
}

#[test]
fn reads_with_chs_addressing() {
    let mut pc = pc_with_disk(16 * 63 * 4);
    // Cylinder 1, head 2, sector 3 under the 16/63 translation.
    pc.outb(PRI + 6, 0xa2);
    pc.outb(PRI + 2, 1);
    pc.outb(PRI + 3, 3);
    pc.outb(PRI + 4, 1);
    pc.outb(PRI + 5, 0);
    pc.outb(PRI + 7, 0x20);
    let lba = (16 + 2) * 63 + 2;
    assert_eq!(pc.read_pio(PRI, 512, 2), expected(lba, 1));
}

#[test]
fn chs_sector_zero_is_rejected_with_idnf() {
    let mut pc = pc_with_disk(16 * 63 * 4);
    pc.outb(PRI + 6, 0xa0);
    pc.outb(PRI + 2, 1);
    pc.outb(PRI + 3, 0);
    pc.outb(PRI + 7, 0x20);
    assert_eq!(pc.status(PRI), 0x41);
    assert_eq!(pc.inb(PRI + 1), 0x10);
}

#[test]
fn writes_sectors_with_16_and_32_bit_pio() {
    let mut pc = pc_with_disk(64);
    let payload: Vec<u8> = (0..1024u32).map(|i| (i * 3) as u8).collect();
    pc.lba28(PRI, 0, 10, 2, 0x30);
    assert_eq!(pc.status(PRI) & 0x08, 0x08);
    assert!(!pc.ide.irq(0), "first write block does not interrupt");
    pc.write_pio(PRI, &payload[..512], 2);
    assert!(pc.ide.irq(0));
    pc.write_pio(PRI, &payload[512..], 4);
    assert!(pc.ide.irq(0), "completion interrupts");
    assert_eq!(pc.status(PRI), 0x50);

    pc.lba28(PRI, 0, 10, 2, 0x20);
    assert_eq!(pc.read_pio(PRI, 1024, 2), payload);
}

#[test]
fn read_past_the_end_fails_with_idnf() {
    let mut pc = pc_with_disk(64);
    pc.lba28(PRI, 0, 63, 2, 0x20);
    assert_eq!(pc.status(PRI), 0x41);
    assert_eq!(pc.inb(PRI + 1), 0x10);
}

#[test]
fn lba48_reads_and_writes_above_the_28_bit_limit() {
    let mut pc = Pc::new();
    pc.ide.attach_disk(0, 0, Box::new(SparseDisk { len: 1 << 45, written: Default::default() }));
    let lba = 0x1_2345_6789u64;
    pc.lba48(PRI, lba, 2, 0x24);
    assert_eq!(pc.read_pio(PRI, 1024, 4), expected(lba, 2));

    let payload = vec![0x5a; 512];
    pc.lba48(PRI, lba + 7, 1, 0x34);
    pc.write_pio(PRI, &payload, 2);
    assert_eq!(pc.status(PRI), 0x50);
    pc.lba48(PRI, lba + 7, 1, 0x24);
    assert_eq!(pc.read_pio(PRI, 512, 2), payload);
}

#[test]
fn lba48_registers_read_back_through_hob() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 3, 0x11);
    pc.outb(PRI + 3, 0x22);
    assert_eq!(pc.inb(PRI + 3), 0x22);
    pc.outb(PRI_CTL, 0x80);
    assert_eq!(pc.inb(PRI + 3), 0x11);
}

#[test]
fn read_multiple_interrupts_once_per_block() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xe0);
    pc.outb(PRI + 2, 4);
    pc.outb(PRI + 7, 0xc6);
    assert_eq!(pc.status(PRI), 0x50);
    assert_eq!(pc.identify(PRI, 0, 0xec)[59], 0x104);

    pc.lba28(PRI, 0, 1, 10, 0xc4);
    let mut data = Vec::new();
    let mut blocks = 0;
    while pc.ide.irq(0) && pc.status(PRI) & 0x08 != 0 {
        blocks += 1;
        let n = if blocks < 3 { 4 * 512 } else { 2 * 512 };
        data.extend(pc.read_pio(PRI, n, 4));
    }
    assert_eq!(blocks, 3);
    assert_eq!(data, expected(1, 10));
}

#[test]
fn write_multiple_takes_blocks_of_the_multiple_count() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xe0);
    pc.outb(PRI + 2, 2);
    pc.outb(PRI + 7, 0xc6);
    pc.status(PRI);
    let payload: Vec<u8> = (0..3 * 512).map(|i| (i / 7) as u8).collect();
    pc.lba28(PRI, 0, 20, 3, 0xc5);
    pc.write_pio(PRI, &payload[..1024], 2);
    assert_eq!(pc.status(PRI) & 0x08, 0x08, "second block requested");
    pc.write_pio(PRI, &payload[1024..], 2);
    assert_eq!(pc.status(PRI), 0x50);
    pc.lba28(PRI, 0, 20, 3, 0x20);
    let back: Vec<u8> = (0..3).flat_map(|_| pc.read_pio(PRI, 512, 2)).collect();
    assert_eq!(back, payload);
}

#[test]
fn set_multiple_rejects_counts_that_are_not_powers_of_two() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xe0);
    pc.outb(PRI + 2, 3);
    pc.outb(PRI + 7, 0xc6);
    assert_eq!(pc.status(PRI), 0x41);
    assert_eq!(pc.inb(PRI + 1), 0x04);
}

#[test]
fn read_multiple_aborts_when_multiple_mode_is_disabled() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xe0);
    pc.outb(PRI + 2, 0);
    pc.outb(PRI + 7, 0xc6);
    pc.lba28(PRI, 0, 0, 1, 0xc4);
    assert_eq!(pc.status(PRI), 0x41);
}

#[test]
fn soft_reset_loads_ata_and_atapi_signatures() {
    let mut pc = Pc::new();
    pc.ide.attach_disk(0, 0, pattern_disk(64));
    pc.ide.attach_cdrom(0, 1, Some(pattern_cd(16)));
    pc.outb(PRI + 2, 0x33);
    pc.outb(PRI_CTL, 0x06);
    assert_eq!(pc.inb(PRI_CTL) & 0x80, 0x80, "BSY while SRST is asserted");
    pc.outb(PRI_CTL, 0x02);
    assert_eq!(pc.inb(PRI + 6) & 0x10, 0, "device 0 selected after reset");
    let master = [2, 3, 4, 5].map(|r| pc.inb(PRI + r));
    assert_eq!(master, [1, 1, 0, 0]);
    assert_eq!(pc.status(PRI), 0x50);
    assert_eq!(pc.inb(PRI + 1), 0x01);
    pc.select(PRI, true);
    let slave = [2, 3, 4, 5].map(|r| pc.inb(PRI + r));
    assert_eq!(slave, [1, 1, 0x14, 0xeb]);
    assert_eq!(pc.status(PRI), 0x00);
    assert!(!pc.ide.irq(0));
}

#[test]
fn unknown_commands_abort() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xa0);
    pc.outb(PRI + 7, 0xa0);
    assert!(pc.ide.irq(0));
    assert_eq!(pc.status(PRI), 0x41);
    assert_eq!(pc.inb(PRI + 1), 0x04);
}

#[test]
fn set_features_selects_a_dma_mode() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xa0);
    pc.outb(PRI + 1, 0x03);
    pc.outb(PRI + 2, 0x22);
    pc.outb(PRI + 7, 0xef);
    assert_eq!(pc.status(PRI), 0x50);
    let w = pc.identify(PRI, 0, 0xec);
    assert_eq!(w[63], 0x0407);
    assert_eq!(w[88], 0x003f);

    pc.outb(PRI + 1, 0x03);
    pc.outb(PRI + 2, 0x46);
    pc.outb(PRI + 7, 0xef);
    assert_eq!(pc.status(PRI), 0x41, "UDMA 6 is not supported");
}

#[test]
fn initialize_device_parameters_changes_the_current_translation() {
    let mut pc = pc_with_disk(16 * 63 * 10);
    pc.outb(PRI + 6, 0xa7);
    pc.outb(PRI + 2, 32);
    pc.outb(PRI + 7, 0x91);
    assert_eq!(pc.status(PRI), 0x50);
    let w = pc.identify(PRI, 0, 0xec);
    assert_eq!((w[54], w[55], w[56]), (16 * 63 * 10 / (8 * 32), 8, 32));
    // Cylinder 0, head 1, sector 1 is now LBA 32.
    pc.outb(PRI + 6, 0xa1);
    pc.outb(PRI + 2, 1);
    pc.outb(PRI + 3, 1);
    pc.outb(PRI + 4, 0);
    pc.outb(PRI + 5, 0);
    pc.outb(PRI + 7, 0x20);
    assert_eq!(pc.read_pio(PRI, 512, 2), expected(32, 1));
}

#[test]
fn read_native_max_returns_the_last_lba() {
    let mut pc = pc_with_disk(1000);
    pc.outb(PRI + 6, 0xe0);
    pc.outb(PRI + 7, 0xf8);
    assert_eq!(pc.status(PRI), 0x50);
    let lba = pc.inb(PRI + 3) as u32 | (pc.inb(PRI + 4) as u32) << 8 | (pc.inb(PRI + 5) as u32) << 16;
    assert_eq!(lba, 999);
}

#[test]
fn non_data_commands_complete_with_an_interrupt() {
    let mut pc = pc_with_disk(64);
    for cmd in [0x10, 0x40, 0x70, 0xe0, 0xe1, 0xe2, 0xe3, 0xe7, 0xea] {
        pc.lba28(PRI, 0, 1, 1, cmd);
        assert!(pc.ide.irq(0), "command {cmd:#x}");
        assert_eq!(pc.status(PRI) & 0xc9, 0x40, "command {cmd:#x}");
    }
}

#[test]
fn check_power_mode_reports_active() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xa0);
    pc.outb(PRI + 7, 0xe5);
    assert_eq!(pc.inb(PRI + 2), 0xff);
}

#[test]
fn execute_device_diagnostic_reports_pass_and_signature() {
    let mut pc = pc_with_disk(64);
    pc.outb(PRI + 6, 0xa0);
    pc.outb(PRI + 4, 0x99);
    pc.outb(PRI + 7, 0x90);
    assert!(pc.ide.irq(0));
    assert_eq!(pc.inb(PRI + 1), 0x01);
    assert_eq!((pc.inb(PRI + 4), pc.inb(PRI + 5)), (0, 0));
    assert_eq!(pc.status(PRI), 0x50);
}

#[test]
fn writes_to_a_read_only_disk_abort() {
    let mut pc = Pc::new();
    pc.ide.attach_disk(0, 0, Box::new(MemBackend::new(vec![0; 64 * 512], true)));
    pc.lba28(PRI, 0, 0, 1, 0x30);
    pc.write_pio(PRI, &[1; 512], 2);
    assert_eq!(pc.status(PRI), 0x41);
}

#[test]
fn channel_decode_can_be_disabled() {
    let mut pc = pc_with_disk(64);
    pc.ide.set_channel_enabled(0, false);
    assert_eq!(pc.ide.io_read(PRI + 7, 1), None);
}

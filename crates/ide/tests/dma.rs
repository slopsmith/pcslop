mod common;

use common::*;

const PRD: u32 = 0x1000;

fn expected(lba: u64, sectors: u64) -> Vec<u8> {
    (lba * 512..(lba + sectors) * 512).map(pattern).collect()
}

fn pc_with_disk() -> Pc {
    let mut pc = Pc::new();
    pc.ide.attach_disk(0, 0, pattern_disk(256));
    pc
}

/// Programs the primary channel's PRD pointer and direction, then clears
/// the interrupt and error bits.
fn setup_bm(pc: &mut Pc, to_memory: bool) {
    pc.outl(BM + 4, PRD);
    pc.outb(BM, if to_memory { 0x08 } else { 0x00 });
    pc.outb(BM + 2, 0x06);
}

#[test]
fn bus_master_registers_read_back() {
    let mut pc = Pc::new();
    pc.outl(BM + 8 + 4, 0x0012_3457);
    assert_eq!(pc.inl(BM + 8 + 4), 0x0012_3454, "PRD pointer is dword aligned");
    pc.outb(BM + 8 + 2, 0x60);
    assert_eq!(pc.inb(BM + 8 + 2), 0x60, "drive DMA capable bits are writable");
    pc.outb(BM + 8, 0x08);
    assert_eq!(pc.inb(BM + 8), 0x08);
}

#[test]
fn bus_master_ports_move_with_the_bar() {
    let mut pc = Pc::new();
    pc.ide.set_bus_master_base(Some(0xd000));
    assert_eq!(pc.ide.io_read(BM, 1), None);
    assert!(pc.ide.io_read(0xd000, 1).is_some());
    pc.ide.set_bus_master_base(None);
    assert_eq!(pc.ide.io_read(0xd000, 1), None);
}

#[test]
fn read_dma_scatters_over_several_prd_entries() {
    let mut pc = pc_with_disk();
    pc.write_prd(PRD, &[(0x10000, 512, false), (0x20000, 1024, false), (0x30000, 1536, true)]);
    setup_bm(&mut pc, true);
    pc.lba28(PRI, 0, 40, 6, 0xc8);
    assert_eq!(pc.status(PRI) & 0x08, 0x08, "DRQ while waiting for the engine");
    pc.outb(BM, 0x09);
    assert!(pc.ide.irq(0));
    assert_eq!(pc.inb(BM + 2) & 0x07, 0x04, "interrupt set, active and error clear");
    pc.outb(BM, 0x08);
    assert_eq!(pc.status(PRI), 0x50);
    let want = expected(40, 6);
    assert_eq!(&pc.ram.0[0x10000..0x10000 + 512], &want[..512]);
    assert_eq!(&pc.ram.0[0x20000..0x20000 + 1024], &want[512..1536]);
    assert_eq!(&pc.ram.0[0x30000..0x30000 + 1536], &want[1536..]);
}

#[test]
fn read_dma_runs_immediately_when_the_engine_is_already_started() {
    let mut pc = pc_with_disk();
    pc.write_prd(PRD, &[(0x10000, 1024, true)]);
    setup_bm(&mut pc, true);
    pc.outb(BM, 0x09);
    assert_eq!(pc.inb(BM + 2) & 0x01, 0x01, "active while waiting for the device");
    pc.lba28(PRI, 0, 2, 2, 0xc8);
    assert_eq!(pc.inb(BM + 2) & 0x07, 0x04);
    assert_eq!(&pc.ram.0[0x10000..0x10000 + 1024], expected(2, 2).as_slice());
}

#[test]
fn write_dma_gathers_from_several_prd_entries() {
    let mut pc = pc_with_disk();
    let payload: Vec<u8> = (0..2048u32).map(|i| (i * 13 + 1) as u8).collect();
    pc.ram.0[0x40000..0x40000 + 1536].copy_from_slice(&payload[..1536]);
    pc.ram.0[0x50000..0x50000 + 512].copy_from_slice(&payload[1536..]);
    pc.write_prd(PRD, &[(0x40000, 1536, false), (0x50000, 512, true)]);
    setup_bm(&mut pc, false);
    pc.lba28(PRI, 0, 100, 4, 0xca);
    pc.outb(BM, 0x01);
    assert_eq!(pc.inb(BM + 2) & 0x07, 0x04);
    pc.outb(BM, 0x00);
    assert_eq!(pc.status(PRI), 0x50);
    pc.lba28(PRI, 0, 100, 4, 0x20);
    let back: Vec<u8> = (0..4).flat_map(|_| pc.read_pio(PRI, 512, 2)).collect();
    assert_eq!(back, payload);
}

#[test]
fn lba48_read_dma_ext() {
    let mut pc = pc_with_disk();
    pc.write_prd(PRD, &[(0x10000, 512, true)]);
    setup_bm(&mut pc, true);
    pc.lba48(PRI, 200, 1, 0x25);
    pc.outb(BM, 0x09);
    assert_eq!(&pc.ram.0[0x10000..0x10000 + 512], expected(200, 1).as_slice());
}

#[test]
fn prd_table_larger_than_the_transfer_leaves_active_set() {
    let mut pc = pc_with_disk();
    pc.write_prd(PRD, &[(0x10000, 4096, true)]);
    setup_bm(&mut pc, true);
    pc.lba28(PRI, 0, 0, 1, 0xc8);
    pc.outb(BM, 0x09);
    assert_eq!(pc.inb(BM + 2) & 0x07, 0x05);
    assert_eq!(pc.status(PRI), 0x50);
}

#[test]
fn prd_table_shorter_than_the_transfer_flags_an_error() {
    let mut pc = pc_with_disk();
    pc.write_prd(PRD, &[(0x10000, 512, true)]);
    setup_bm(&mut pc, true);
    pc.lba28(PRI, 0, 0, 2, 0xc8);
    pc.outb(BM, 0x09);
    assert_eq!(pc.inb(BM + 2) & 0x03, 0x02);
    assert_eq!(pc.status(PRI) & 0x01, 0x01);
}

#[test]
fn zero_length_prd_entry_means_64k() {
    let mut pc = pc_with_disk();
    pc.write_prd(PRD, &[(0x100000, 0, true)]);
    setup_bm(&mut pc, true);
    pc.lba28(PRI, 0, 0, 128, 0xc8);
    pc.outb(BM, 0x09);
    assert_eq!(pc.inb(BM + 2) & 0x07, 0x04);
    assert_eq!(&pc.ram.0[0x100000..0x110000], expected(0, 128).as_slice());
}

#[test]
fn bus_master_interrupt_bit_is_write_one_to_clear() {
    let mut pc = pc_with_disk();
    pc.write_prd(PRD, &[(0x10000, 512, true)]);
    setup_bm(&mut pc, true);
    pc.lba28(PRI, 0, 0, 1, 0xc8);
    pc.outb(BM, 0x09);
    pc.outb(BM + 2, 0x00);
    assert_eq!(pc.inb(BM + 2) & 0x04, 0x04);
    pc.outb(BM + 2, 0x04);
    assert_eq!(pc.inb(BM + 2) & 0x04, 0);
}

#[test]
fn atapi_read10_over_dma() {
    let mut pc = Pc::new();
    pc.ide.attach_cdrom(1, 0, Some(pattern_cd(64)));
    pc.select(SEC, false);
    pc.clear_ua(SEC);
    pc.write_prd(PRD, &[(0x10000, 2048, false), (0x20000, 2048, true)]);
    pc.outl(BM + 8 + 4, PRD);
    pc.outb(BM + 8, 0x08);
    pc.outb(BM + 8 + 2, 0x06);
    pc.outb(SEC + 1, 0x01);
    pc.outb(SEC + 4, 0);
    pc.outb(SEC + 5, 0);
    pc.outb(SEC + 7, 0xa0);
    pc.write_pio(SEC, &[0x28, 0, 0, 0, 0, 10, 0, 0, 2, 0, 0, 0], 2);
    assert!(!pc.ide.irq(1), "no interrupt before the engine runs");
    pc.outb(BM + 8, 0x09);
    assert!(pc.ide.irq(1));
    assert_eq!(pc.inb(BM + 8 + 2) & 0x07, 0x04);
    assert_eq!(pc.status(SEC), 0x50);
    assert_eq!(pc.inb(SEC + 2) & 3, 3);
    assert_eq!(&pc.ram.0[0x10000..0x10004], &10u32.to_le_bytes());
    assert_eq!(&pc.ram.0[0x20000..0x20004], &11u32.to_le_bytes());
}

#[test]
fn atapi_dma_command_without_data_completes_without_the_engine() {
    let mut pc = Pc::new();
    pc.ide.attach_cdrom(1, 0, Some(pattern_cd(64)));
    pc.select(SEC, false);
    pc.clear_ua(SEC);
    pc.outb(SEC + 1, 0x01);
    pc.outb(SEC + 7, 0xa0);
    pc.write_pio(SEC, &[0; 12], 2);
    assert!(pc.ide.irq(1));
    assert_eq!(pc.status(SEC), 0x50);
}

#[test]
fn reset_clears_bus_master_state() {
    let mut pc = Pc::new();
    pc.outl(BM + 4, PRD);
    pc.outb(BM, 0x09);
    pc.ide.reset();
    assert_eq!(pc.inb(BM), 0);
    assert_eq!(pc.inl(BM + 4), 0);
}

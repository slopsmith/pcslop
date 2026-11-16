mod common;

use common::*;
use ide::FileBackend;

fn pc_with_cd(sectors: u32) -> Pc {
    let mut pc = Pc::new();
    pc.ide.attach_cdrom(1, 0, Some(pattern_cd(sectors)));
    pc.select(SEC, false);
    pc
}

fn read10(lba: u32, count: u16) -> [u8; 10] {
    let l = lba.to_be_bytes();
    let c = count.to_be_bytes();
    [0x28, 0, l[0], l[1], l[2], l[3], 0, c[0], c[1], 0]
}

fn cd_sector(n: u32) -> Vec<u8> {
    let mut s = vec![n as u8; 2048];
    s[..4].copy_from_slice(&n.to_le_bytes());
    s
}

#[test]
fn powers_up_with_the_packet_signature() {
    let mut pc = pc_with_cd(16);
    assert_eq!([2, 3, 4, 5].map(|r| pc.inb(SEC + r)), [1, 1, 0x14, 0xeb]);
    assert_eq!(pc.status(SEC), 0);
}

#[test]
fn identify_device_aborts_and_leaves_the_signature() {
    let mut pc = pc_with_cd(16);
    pc.outb(SEC + 4, 0);
    pc.outb(SEC + 7, 0xec);
    assert_eq!(pc.status(SEC) & 0x01, 0x01);
    assert_eq!(pc.inb(SEC + 1) & 0x04, 0x04);
    assert_eq!((pc.inb(SEC + 4), pc.inb(SEC + 5)), (0x14, 0xeb));
}

#[test]
fn identify_packet_device_describes_a_removable_cdrom() {
    let mut pc = pc_with_cd(16);
    let w = pc.identify(SEC, 0, 0xa1);
    assert_eq!(w[0], 0x85c0);
    assert_ne!(w[49] & (1 << 8), 0, "DMA");
    assert_eq!(pc.status(SEC), 0x50);
}

#[test]
fn reports_power_on_unit_attention_once() {
    let mut pc = pc_with_cd(16);
    let (_, st) = pc.packet(SEC, &[0x00], 0);
    assert_eq!(st & 0x01, 0x01);
    assert_eq!(pc.inb(SEC + 1) >> 4, 6);
    assert_eq!(pc.request_sense(SEC), (6, 0x29, 0));
    let (_, st) = pc.packet(SEC, &[0x00], 0);
    assert_eq!(st & 0x01, 0);
}

#[test]
fn request_sense_returns_a_pending_unit_attention() {
    let mut pc = pc_with_cd(16);
    assert_eq!(pc.request_sense(SEC), (6, 0x29, 0));
    assert_eq!(pc.request_sense(SEC), (0, 0, 0));
}

#[test]
fn media_change_raises_unit_attention() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    pc.ide.attach_cdrom(1, 0, Some(pattern_cd(32)));
    let (_, st) = pc.packet(SEC, &read10(0, 1), 2048);
    assert_eq!(st & 0x01, 0x01);
    assert_eq!(pc.request_sense(SEC), (6, 0x28, 0));
    assert_eq!(pc.packet_data(SEC, &read10(31, 1), 2048), cd_sector(31));
}

#[test]
fn no_medium_reports_not_ready() {
    let mut pc = Pc::new();
    pc.ide.attach_cdrom(1, 0, None);
    pc.select(SEC, false);
    let (_, st) = pc.packet(SEC, &[0x00], 0);
    assert_eq!(st & 0x01, 0x01);
    assert_eq!(pc.request_sense(SEC), (2, 0x3a, 0x01));
}

#[test]
fn inquiry_identifies_a_cdrom() {
    let mut pc = pc_with_cd(16);
    let d = pc.packet_data(SEC, &[0x12, 0, 0, 0, 36, 0], 36);
    assert_eq!(d.len(), 36);
    assert_eq!(d[0], 0x05);
    assert_eq!(d[1] & 0x80, 0x80);
    assert_eq!(d[4] as usize, 36 - 5);
}

#[test]
fn inquiry_is_truncated_to_the_allocation_length() {
    let mut pc = pc_with_cd(16);
    assert_eq!(pc.packet_data(SEC, &[0x12, 0, 0, 0, 5, 0], 0xfffe).len(), 5);
}

#[test]
fn read_capacity_returns_last_lba_and_block_size() {
    let mut pc = pc_with_cd(100);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &[0x25], 8);
    assert_eq!(d, [0, 0, 0, 99, 0, 0, 8, 0]);
}

#[test]
fn read10_returns_whole_sectors() {
    let mut pc = pc_with_cd(64);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &read10(16, 2), 0xfffe);
    assert_eq!(d, [cd_sector(16), cd_sector(17)].concat());
}

#[test]
fn read10_splits_data_at_the_byte_count_limit() {
    let mut pc = pc_with_cd(64);
    pc.clear_ua(SEC);
    let (blocks, st) = pc.packet(SEC, &read10(3, 3), 0x1000);
    assert_eq!(st, 0x50);
    assert_eq!(blocks.iter().map(Vec::len).collect::<Vec<_>>(), [4096, 2048]);
    assert_eq!(blocks.concat(), [cd_sector(3), cd_sector(4), cd_sector(5)].concat());
    assert_eq!(pc.inb(SEC + 2) & 3, 3, "completion reason is IO|CoD");
}

#[test]
fn odd_byte_count_limit_is_rounded_down_when_splitting() {
    let mut pc = pc_with_cd(64);
    pc.clear_ua(SEC);
    let (blocks, _) = pc.packet(SEC, &read10(0, 1), 1001);
    assert!(blocks.iter().all(|b| b.len() <= 1000 && b.len() % 2 == 0), "{:?}", blocks.iter().map(Vec::len).collect::<Vec<_>>());
    assert_eq!(blocks.concat(), cd_sector(0));
}

#[test]
fn every_data_block_raises_an_interrupt() {
    let mut pc = pc_with_cd(64);
    pc.clear_ua(SEC);
    pc.outb(SEC + 4, 0x00);
    pc.outb(SEC + 5, 0x08);
    pc.outb(SEC + 7, 0xa0);
    assert!(!pc.ide.irq(1), "no interrupt when asking for the CDB");
    pc.write_pio(SEC, &[read10(0, 2).as_slice(), &[0, 0]].concat(), 2);
    for _ in 0..2 {
        assert!(pc.ide.irq(1));
        pc.status(SEC);
        pc.read_pio(SEC, 2048, 4);
    }
    assert!(pc.ide.irq(1), "completion interrupt");
    assert_eq!(pc.status(SEC), 0x50);
}

#[test]
fn read_beyond_the_medium_is_an_illegal_request() {
    let mut pc = pc_with_cd(64);
    pc.clear_ua(SEC);
    let (_, st) = pc.packet(SEC, &read10(63, 2), 0xfffe);
    assert_eq!(st & 1, 1);
    assert_eq!(pc.request_sense(SEC), (5, 0x21, 0));
}

#[test]
fn read12_and_read_cd_return_user_data() {
    let mut pc = pc_with_cd(64);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &[0xa8, 0, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0], 0xfffe);
    assert_eq!(d, cd_sector(7));
    let d = pc.packet_data(SEC, &[0xbe, 0, 0, 0, 0, 8, 0, 0, 1, 0x10, 0, 0], 0xfffe);
    assert_eq!(d, cd_sector(8));
}

#[test]
fn read_cd_raw_sector_has_sync_header_and_data() {
    let mut pc = pc_with_cd(64);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &[0xbe, 0, 0, 0, 0, 9, 0, 0, 1, 0xf8, 0, 0], 0xfffe);
    assert_eq!(d.len(), 2352);
    assert_eq!(&d[..12], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    assert_eq!(&d[12..16], &[0x00, 0x02, 0x09, 0x01], "BCD MSF 00:02:09, mode 1");
    assert_eq!(&d[16..16 + 2048], cd_sector(9).as_slice());
}

#[test]
fn read_toc_format_0_in_lba_and_msf() {
    let mut pc = pc_with_cd(1000);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &[0x43, 0, 0, 0, 0, 0, 0, 0, 0xff, 0], 0xfffe);
    assert_eq!(d, [0, 18, 1, 1, 0, 0x14, 1, 0, 0, 0, 0, 0, 0, 0x16, 0xaa, 0, 0, 0, 0x03, 0xe8]);
    let d = pc.packet_data(SEC, &[0x43, 2, 0, 0, 0, 0, 0, 0, 0xff, 0], 0xfffe);
    assert_eq!(&d[8..12], &[0, 0, 2, 0], "track 1 at 00:02:00");
    assert_eq!(&d[16..20], &[0, 0, 15, 25], "lead-out at 1150 frames");
}

#[test]
fn read_toc_formats_1_and_2() {
    let mut pc = pc_with_cd(1000);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &[0x43, 0, 1, 0, 0, 0, 0, 0, 0xff, 0], 0xfffe);
    assert_eq!(d, [0, 10, 1, 1, 0, 0x14, 1, 0, 0, 0, 0, 0]);
    let d = pc.packet_data(SEC, &[0x43, 0, 2, 0, 0, 0, 0, 0, 0xff, 0], 0xfffe);
    assert_eq!(d.len(), 4 + 4 * 11);
    assert_eq!(d[7], 0xa0);
    assert_eq!(d[29], 0xa2);
    assert_eq!(&d[33..37], &1000u32.to_be_bytes());
}

#[test]
fn read_toc_rejects_a_missing_track() {
    let mut pc = pc_with_cd(1000);
    pc.clear_ua(SEC);
    let (_, st) = pc.packet(SEC, &[0x43, 0, 0, 0, 0, 0, 5, 0, 0xff, 0], 0xfffe);
    assert_eq!(st & 1, 1);
    assert_eq!(pc.request_sense(SEC), (5, 0x24, 0));
}

#[test]
fn unknown_opcodes_are_illegal_requests() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    let (_, st) = pc.packet(SEC, &[0xff], 0xfffe);
    assert_eq!(st & 1, 1);
    assert_eq!(pc.inb(SEC + 1) >> 4, 5);
    assert_eq!(pc.request_sense(SEC), (5, 0x20, 0));
}

#[test]
fn mode_sense_returns_the_capabilities_page() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &[0x5a, 0, 0x2a, 0, 0, 0, 0, 0, 0xff, 0], 0xfffe);
    assert_eq!(u16::from_be_bytes([d[0], d[1]]) as usize, d.len() - 2);
    assert_eq!(d[8], 0x2a);
    assert_eq!(d[9] as usize, d.len() - 10);
    let d6 = pc.packet_data(SEC, &[0x1a, 0, 0x01, 0, 0xff, 0], 0xfffe);
    assert_eq!(d6[0] as usize, d6.len() - 1);
    assert_eq!(&d6[4..6], &[0x01, 0x06]);
}

#[test]
fn mode_sense_all_pages_and_saved_values() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &[0x5a, 0, 0x3f, 0, 0, 0, 0, 0x10, 0, 0], 0xfffe);
    assert!(d.windows(2).any(|w| w == [0x2a, 20]));
    let (_, st) = pc.packet(SEC, &[0x5a, 0, 0xc1, 0, 0, 0, 0, 0, 0xff, 0], 0xfffe);
    assert_eq!(st & 1, 1);
    assert_eq!(pc.request_sense(SEC), (5, 0x39, 0));
}

#[test]
fn mode_select_accepts_a_parameter_list() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    pc.outb(SEC + 4, 0xfe);
    pc.outb(SEC + 5, 0xff);
    pc.outb(SEC + 7, 0xa0);
    pc.write_pio(SEC, &[0x55, 0x10, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0], 2);
    assert_eq!(pc.status(SEC) & 0x08, 0x08);
    assert_eq!(pc.inb(SEC + 2) & 3, 0, "data-out reason");
    assert_eq!(pc.inb(SEC + 4), 16);
    pc.write_pio(SEC, &[0; 16], 2);
    assert_eq!(pc.status(SEC), 0x50);
}

#[test]
fn get_configuration_reports_cdrom_as_current_profile() {
    let mut pc = pc_with_cd(16);
    let d = pc.packet_data(SEC, &[0x46, 0, 0, 0, 0, 0, 0, 0x01, 0, 0], 0xfffe);
    assert_eq!(u32::from_be_bytes(d[0..4].try_into().unwrap()) as usize, d.len() - 4);
    assert_eq!(&d[6..8], &[0x00, 0x08]);
    assert_eq!(&d[8..10], &[0, 0], "first feature is the profile list");
    let profiles: Vec<(u16, u8)> = d[12..12 + d[11] as usize].chunks(4).map(|p| (u16::from_be_bytes([p[0], p[1]]), p[2] & 1)).collect();
    assert_eq!(profiles, [(0x10, 0), (0x08, 1)]);
}

#[test]
fn get_configuration_with_rt_2_returns_one_feature() {
    let mut pc = pc_with_cd(16);
    let d = pc.packet_data(SEC, &[0x46, 2, 0, 0x1e, 0, 0, 0, 0x01, 0, 0], 0xfffe);
    assert_eq!(d.len(), 8 + 8);
    assert_eq!(&d[8..10], &[0x00, 0x1e]);
}

#[test]
fn event_status_reports_new_media_once() {
    let mut pc = pc_with_cd(16);
    pc.ide.attach_cdrom(1, 0, Some(pattern_cd(16)));
    let cdb = [0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8, 0];
    let d = pc.packet_data(SEC, &cdb, 0xfffe);
    assert_eq!(d, [0, 6, 4, 0x10, 2, 2, 0, 0]);
    let d = pc.packet_data(SEC, &cdb, 0xfffe);
    assert_eq!(d[4], 0);
}

#[test]
fn eject_and_load_through_start_stop_unit() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    pc.packet_data(SEC, &[0x1b, 0, 0, 0, 0x02, 0], 0);
    let (_, st) = pc.packet(SEC, &[0x00], 0);
    assert_eq!(st & 1, 1);
    assert_eq!(pc.request_sense(SEC), (2, 0x3a, 0x02));
    pc.packet_data(SEC, &[0x1b, 0, 0, 0, 0x03, 0], 0);
    assert_eq!(pc.packet_data(SEC, &read10(2, 1), 2048), cd_sector(2));
}

#[test]
fn locked_tray_refuses_eject() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    pc.packet_data(SEC, &[0x1e, 0, 0, 0, 1, 0], 0);
    let (_, st) = pc.packet(SEC, &[0x1b, 0, 0, 0, 0x02, 0], 0);
    assert_eq!(st & 1, 1);
    assert_eq!(pc.request_sense(SEC), (2, 0x53, 0x02));
}

#[test]
fn disc_track_and_mechanism_queries_succeed() {
    let mut pc = pc_with_cd(500);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &[0x51, 0, 0, 0, 0, 0, 0, 0, 34, 0], 0xfffe);
    assert_eq!((d.len(), d[2]), (34, 0x0e));
    let d = pc.packet_data(SEC, &[0x52, 1, 0, 0, 0, 1, 0, 0, 36, 0], 0xfffe);
    assert_eq!(&d[24..28], &500u32.to_be_bytes());
    let d = pc.packet_data(SEC, &[0xbd, 0, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0], 0xfffe);
    assert_eq!(d.len(), 8);
    let d = pc.packet_data(SEC, &[0x42, 0, 0x40, 1, 0, 0, 0, 0, 16, 0], 0xfffe);
    assert_eq!((d.len(), d[4]), (16, 1));
    pc.packet_data(SEC, &[0xbb, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0], 0);
    pc.packet_data(SEC, &[0x2b, 0, 0, 0, 1, 0, 0, 0, 0, 0], 0);
}

#[test]
fn data_command_with_zero_byte_count_limit_aborts() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    let (_, st) = pc.packet(SEC, &read10(0, 1), 0);
    assert_eq!(st, 0x41);
    assert_eq!(pc.inb(SEC + 1), 0x04);
}

#[test]
fn soft_reset_does_not_raise_unit_attention() {
    let mut pc = pc_with_cd(16);
    pc.clear_ua(SEC);
    pc.outb(SEC_CTL, 0x04);
    pc.outb(SEC_CTL, 0x00);
    let (_, st) = pc.packet(SEC, &[0x00], 0);
    assert_eq!(st & 1, 0);
}

#[test]
fn reads_volume_descriptors_from_the_xp_iso() {
    let path = std::path::Path::new(env!("HOME"))
        .join("Documents/ISOs/en_windows_xp_professional_with_service_pack_3_x86_cd_vl_x14-73974.iso");
    let Ok(iso) = FileBackend::open(&path, true) else {
        eprintln!("skipping: {} is not available on this machine", path.display());
        return;
    };
    let mut pc = Pc::new();
    pc.ide.attach_cdrom(1, 0, Some(Box::new(iso)));
    pc.select(SEC, false);
    pc.clear_ua(SEC);
    let d = pc.packet_data(SEC, &read10(16, 2), 2048);
    assert_eq!(&d[..6], b"\x01CD001", "primary volume descriptor");
    assert_eq!(&d[2048..2048 + 7], b"\x00CD001\x01");
    assert_eq!(&d[2048 + 7..2048 + 30], b"EL TORITO SPECIFICATION");
    let cap = pc.packet_data(SEC, &[0x25], 8);
    let blocks = u32::from_be_bytes(cap[0..4].try_into().unwrap()) + 1;
    let pvd_blocks = u32::from_le_bytes(d[80..84].try_into().unwrap());
    assert_eq!(blocks, pvd_blocks);
}

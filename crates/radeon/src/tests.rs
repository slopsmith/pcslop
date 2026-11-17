use super::*;

fn card() -> Radeon {
    Radeon::new(vec![0x55, 0xaa])
}

#[test]
fn io_bar_mirrors_the_first_registers() {
    let mut r = card();
    r.mmio_write(0x10, 4, 0x1234_5678);
    assert_eq!(r.io_read(0x10, 4), 0x1234_5678);
    assert_eq!(r.io_read(0x12, 1), 0x34);
    r.io_write(0x11, 1, 0xab);
    assert_eq!(r.mmio_read(0x10, 4), 0x1234_ab78);
}

#[test]
fn mm_index_reaches_registers_and_vram() {
    let mut r = card();
    r.io_write(0, 4, 0x0e40);
    assert_eq!(r.io_read(4, 4), RBBM_FIFOCNT_IDLE);
    r.io_write(0, 4, 0x8000_1000);
    r.io_write(4, 4, 0xdead_beef);
    assert_eq!(&r.vram()[0x1000..0x1004], &0xdead_beefu32.to_le_bytes());
    assert_eq!(r.io_read(6, 2), 0xdead);
}

#[test]
fn pll_writes_need_pll_wr_en() {
    let mut r = card();
    r.io_write(8, 1, 0x0e);
    r.io_write(0xc, 4, 0x1111);
    assert_eq!(r.io_read(0xc, 4), 0);
    r.io_write(8, 1, 0x0e | PLL_WR_EN as u32);
    r.io_write(0xc, 4, 0x1111);
    r.io_write(0xd, 1, 0x22);
    r.io_write(8, 1, 0x0e);
    assert_eq!(r.io_read(0xc, 4), 0x2211);
}

#[test]
fn apertures_follow_the_bars() {
    let mut r = card();
    r.bars = Bars { fb: 0xe000_0000, io: 0xc000, mmio: 0xfebf_0000 };
    assert_eq!(r.mmio_read(CONFIG_APER_0_BASE, 4), 0xe000_0000);
    assert_eq!(r.mmio_read(CONFIG_APER_1_BASE, 4), 0xe400_0000);
    assert_eq!(r.mmio_read(CONFIG_APER_SIZE, 4), 64 << 20);
    assert_eq!(Radeon::fb_offset(0x0400_0010), 0x10);
}

#[test]
fn mmio_mirrors_config_space() {
    let mut r = card();
    r.config[..4].copy_from_slice(&[0x02, 0x10, 0x44, 0x4e]);
    assert_eq!(r.mmio_read(0xf00, 4), 0x4e44_1002);
    assert_eq!(r.mmio_read(0xf02, 2), 0x4e44);
}

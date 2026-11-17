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

fn set_640x480x16(r: &mut Radeon) {
    r.mmio_write(CRTC_H_TOTAL_DISP, 4, 0x004f_0063);
    r.mmio_write(CRTC_V_TOTAL_DISP, 4, 0x01df_020c);
    r.mmio_write(CRTC_PITCH, 4, 0x50);
    r.mmio_write(CRTC_OFFSET, 4, 0x1000);
    r.mmio_write(CRTC_GEN_CNTL, 4, CRTC_EN | CRTC_EXT_DISP_EN | 4 << 8);
}

#[test]
fn crtc_mode_from_the_vbe_registers_the_rom_writes() {
    let mut r = card();
    set_640x480x16(&mut r);
    let m = r.mode().unwrap();
    assert_eq!((m.width, m.height, m.bits, m.pitch, m.offset), (640, 480, 16, 1280, 0x1000));
    r.vram_mut()[0x1000..0x1002].copy_from_slice(&0xf800u16.to_le_bytes());
    let mut out = Vec::new();
    assert_eq!(r.render(&mut out), Some((640, 480)));
    assert_eq!(out[0], 0xff_0000);
    r.mmio_write(CRTC_EXT_CNTL, 4, CRTC_DISPLAY_DIS);
    assert_eq!(r.render(&mut out), None);
}

#[test]
fn palette_registers_reach_the_dac() {
    let mut r = card();
    r.mmio_write(DAC_CNTL, 4, DAC_8BIT_EN);
    r.mmio_write(PALETTE_INDEX, 4, 5);
    r.mmio_write(PALETTE_DATA, 4, 0x0012_3456);
    r.mmio_write(PALETTE_DATA, 4, 0x00ab_cdef);
    assert_eq!(r.vga().dac_entry(6), [0xab, 0xcd, 0xef]);
    r.mmio_write(PALETTE_INDEX, 4, 5 << 16);
    assert_eq!(r.mmio_read(PALETTE_DATA, 4), 0x0012_3456);
    assert_eq!(r.mmio_read(PALETTE_INDEX, 4) >> 16, 6);
}

#[test]
fn extended_modes_bank_the_a0000_window() {
    let mut r = card();
    set_640x480x16(&mut r);
    r.mmio_write(MEM_VGA_WP_SEL, 4, 0x000c_0008);
    r.mmio_write(MEM_VGA_RP_SEL, 4, 0x000c_0008);
    r.vga().mem_write8(0xa8001, 0x77);
    assert_eq!(r.vram()[0xc * 0x2000 + 1], 0x77);
    assert_eq!(r.vga().mem_read8(0xa8001), 0x77);
    r.mmio_write(CRTC_EXT_CNTL, 4, VGA_ATI_LINEAR);
    r.vga().mem_write8(0xa0002, 0x55);
    assert_eq!(r.vram()[8 * 0x8000 + 2], 0x55);
    r.mmio_write(CRTC_EXT_CNTL, 4, 0);
    r.mmio_write(CRTC_GEN_CNTL, 4, 0);
    r.vga().mem_write8(0xa8001, 0x11);
    assert_eq!(r.vram()[0xc * 0x2000 + 1], 0x77);
}

#[test]
fn vblank_status_latches_until_acknowledged() {
    let mut r = card();
    set_640x480x16(&mut r);
    r.set_time(1_000_000_000);
    assert_ne!(r.mmio_read(CRTC_STATUS, 4) & CRTC_VBLANK_SAVE, 0);
    r.mmio_write(CRTC_STATUS, 4, CRTC_VBLANK_SAVE);
    assert_eq!(r.mmio_read(CRTC_STATUS, 4) & CRTC_VBLANK_SAVE, 0);
    r.set_time(1_000_000_000 + 17_000_000);
    assert_ne!(r.mmio_read(CRTC_STATUS, 4) & CRTC_VBLANK_SAVE, 0);
    assert_ne!(r.mmio_read(GEN_INT_STATUS, 4) & VSYNC_INT, 0);
}

struct I2cMaster<'a>(&'a mut Radeon);

impl I2cMaster<'_> {
    fn set(&mut self, scl: bool, sda: bool) {
        let mut v = GPIO_EN_0 | GPIO_EN_1;
        v |= if scl { GPIO_A_1 } else { 0 } | if sda { GPIO_A_0 } else { 0 };
        self.0.mmio_write(GPIO_VGA_DDC, 4, v);
    }

    fn sda(&mut self) -> bool {
        self.0.mmio_read(GPIO_VGA_DDC, 4) & GPIO_Y_0 != 0
    }

    fn start(&mut self) {
        self.set(true, true);
        self.set(true, false);
        self.set(false, false);
    }

    fn stop(&mut self) {
        self.set(false, false);
        self.set(true, false);
        self.set(true, true);
    }

    fn write(&mut self, byte: u8) -> bool {
        for i in (0..8).rev() {
            let bit = byte >> i & 1 != 0;
            self.set(false, bit);
            self.set(true, bit);
            self.set(false, bit);
        }
        self.set(false, true);
        self.set(true, true);
        let ack = !self.sda();
        self.set(false, true);
        ack
    }

    fn read(&mut self, ack: bool) -> u8 {
        let mut v = 0;
        for _ in 0..8 {
            self.set(false, true);
            self.set(true, true);
            v = v << 1 | self.sda() as u8;
        }
        self.set(false, !ack);
        self.set(true, !ack);
        self.set(false, !ack);
        v
    }
}

#[test]
fn edid_reads_back_over_bit_banged_ddc() {
    let mut r = card();
    let mut m = I2cMaster(&mut r);
    m.start();
    assert!(!m.write(0xa4), "nothing answers at 0x52");
    m.stop();
    m.start();
    assert!(m.write(0xa0));
    assert!(m.write(0x00));
    m.start();
    assert!(m.write(0xa1));
    let got: Vec<u8> = (0..128).map(|i| m.read(i != 127)).collect();
    m.stop();
    assert_eq!(got, ddc::edid());
    assert_eq!(&got[..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    assert_eq!(got.iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0);
}

#[test]
fn dac_compare_finds_a_monitor() {
    let mut r = card();
    r.mmio_write(DAC_CNTL, 4, 0x0a);
    assert_ne!(r.mmio_read(DAC_CNTL, 1) & DAC_CMP_OUTPUT, 0);
    r.mmio_write(DAC_CNTL, 4, 0x02);
    assert_eq!(r.mmio_read(DAC_CNTL, 1) & DAC_CMP_OUTPUT, 0);
}

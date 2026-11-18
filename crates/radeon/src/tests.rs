use super::*;

fn card() -> Radeon {
    Radeon::new(vec![0x55, 0xaa])
}

#[test]
fn io_bar_mirrors_the_first_registers() {
    let mut r = card();
    r.mmio_write(0x10, 4, 0x1234_5678, &mut NoBus);
    assert_eq!(r.io_read(0x10, 4), 0x1234_5678);
    assert_eq!(r.io_read(0x12, 1), 0x34);
    r.io_write(0x11, 1, 0xab, &mut NoBus);
    assert_eq!(r.mmio_read(0x10, 4), 0x1234_ab78);
}

#[test]
fn mm_index_reaches_registers_and_vram() {
    let mut r = card();
    r.io_write(0, 4, 0x0e40, &mut NoBus);
    assert_eq!(r.io_read(4, 4), RBBM_FIFOCNT_IDLE);
    r.io_write(0, 4, 0x8000_1000, &mut NoBus);
    r.io_write(4, 4, 0xdead_beef, &mut NoBus);
    assert_eq!(&r.vram()[0x1000..0x1004], &0xdead_beefu32.to_le_bytes());
    assert_eq!(r.io_read(6, 2), 0xdead);
}

#[test]
fn pll_writes_need_pll_wr_en() {
    let mut r = card();
    r.io_write(8, 1, 0x0e, &mut NoBus);
    r.io_write(0xc, 4, 0x1111, &mut NoBus);
    assert_eq!(r.io_read(0xc, 4), 0);
    r.io_write(8, 1, 0x0e | PLL_WR_EN, &mut NoBus);
    r.io_write(0xc, 4, 0x1111, &mut NoBus);
    r.io_write(0xd, 1, 0x22, &mut NoBus);
    r.io_write(8, 1, 0x0e, &mut NoBus);
    assert_eq!(r.io_read(0xc, 4), 0x2211);
}

#[test]
fn apertures_follow_the_bars() {
    let mut r = card();
    r.bars = Bars {
        fb: 0xe000_0000,
        io: 0xc000,
        mmio: 0xfebf_0000,
    };
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
    r.mmio_write(CRTC_H_TOTAL_DISP, 4, 0x004f_0063, &mut NoBus);
    r.mmio_write(CRTC_V_TOTAL_DISP, 4, 0x01df_020c, &mut NoBus);
    r.mmio_write(CRTC_PITCH, 4, 0x50, &mut NoBus);
    r.mmio_write(CRTC_OFFSET, 4, 0x1000, &mut NoBus);
    r.mmio_write(
        CRTC_GEN_CNTL,
        4,
        CRTC_EN | CRTC_EXT_DISP_EN | 4 << 8,
        &mut NoBus,
    );
}

#[test]
fn crtc_mode_from_the_vbe_registers_the_rom_writes() {
    let mut r = card();
    set_640x480x16(&mut r);
    let m = r.mode().unwrap();
    assert_eq!(
        (m.width, m.height, m.bits, m.pitch, m.offset),
        (640, 480, 16, 1280, 0x1000)
    );
    r.vram_mut()[0x1000..0x1002].copy_from_slice(&0xf800u16.to_le_bytes());
    let mut out = Vec::new();
    assert_eq!(r.render(&mut out), Some((640, 480)));
    assert_eq!(out[0], 0xff_0000);
    r.mmio_write(CRTC_EXT_CNTL, 4, CRTC_DISPLAY_DIS, &mut NoBus);
    assert_eq!(r.render(&mut out), None);
}

#[test]
fn scanout_at_32bpp_wraps_rows_past_the_end_of_vram() {
    let mut r = card();
    set_640x480x16(&mut r);
    r.mmio_write(CRTC_GEN_CNTL, 4, CRTC_EN | CRTC_EXT_DISP_EN | 6 << 8, &mut NoBus);
    let row = 640 * 4;
    r.mmio_write(CRTC_OFFSET, 4, (VRAM_SIZE - row - 4) as u32, &mut NoBus);
    let vram = r.vram_mut();
    vram[VRAM_SIZE - row - 4..][..4].copy_from_slice(&0xff12_3456u32.to_le_bytes());
    vram[VRAM_SIZE - 4..].copy_from_slice(&0x00ab_cdefu32.to_le_bytes());
    vram[..4].copy_from_slice(&0x0065_4321u32.to_le_bytes());
    let mut out = Vec::new();
    r.render(&mut out);
    assert_eq!([out[0], out[640], out[641]], [0x12_3456, 0xab_cdef, 0x65_4321]);
}

#[test]
fn palette_registers_reach_the_dac() {
    let mut r = card();
    r.mmio_write(DAC_CNTL, 4, DAC_8BIT_EN, &mut NoBus);
    r.mmio_write(PALETTE_INDEX, 4, 5, &mut NoBus);
    r.mmio_write(PALETTE_DATA, 4, 0x0012_3456, &mut NoBus);
    r.mmio_write(PALETTE_DATA, 4, 0x00ab_cdef, &mut NoBus);
    assert_eq!(r.vga().dac_entry(6), [0xab, 0xcd, 0xef]);
    r.mmio_write(PALETTE_INDEX, 4, 5 << 16, &mut NoBus);
    assert_eq!(r.mmio_read(PALETTE_DATA, 4), 0x0012_3456);
    assert_eq!(r.mmio_read(PALETTE_INDEX, 4) >> 16, 6);
}

#[test]
fn extended_modes_bank_the_a0000_window() {
    let mut r = card();
    set_640x480x16(&mut r);
    r.mmio_write(MEM_VGA_WP_SEL, 4, 0x000c_0008, &mut NoBus);
    r.mmio_write(MEM_VGA_RP_SEL, 4, 0x000c_0008, &mut NoBus);
    r.vga().mem_write8(0xa8001, 0x77);
    assert_eq!(r.vram()[0xc * 0x2000 + 1], 0x77);
    assert_eq!(r.vga().mem_read8(0xa8001), 0x77);
    r.mmio_write(CRTC_EXT_CNTL, 4, VGA_ATI_LINEAR, &mut NoBus);
    r.vga().mem_write8(0xa0002, 0x55);
    assert_eq!(r.vram()[8 * 0x8000 + 2], 0x55);
    r.mmio_write(CRTC_EXT_CNTL, 4, 0, &mut NoBus);
    r.mmio_write(CRTC_GEN_CNTL, 4, 0, &mut NoBus);
    r.vga().mem_write8(0xa8001, 0x11);
    assert_eq!(r.vram()[0xc * 0x2000 + 1], 0x77);
}

#[test]
fn vblank_status_latches_until_acknowledged() {
    let mut r = card();
    set_640x480x16(&mut r);
    r.set_time(1_000_000_000);
    assert_ne!(r.mmio_read(CRTC_STATUS, 4) & CRTC_VBLANK_SAVE, 0);
    r.mmio_write(CRTC_STATUS, 4, CRTC_VBLANK_SAVE, &mut NoBus);
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
        self.0.mmio_write(GPIO_VGA_DDC, 4, v, &mut NoBus);
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
    r.mmio_write(DAC_CNTL, 4, 0x0a, &mut NoBus);
    assert_ne!(r.mmio_read(DAC_CNTL, 1) & DAC_CMP_OUTPUT, 0);
    r.mmio_write(DAC_CNTL, 4, 0x02, &mut NoBus);
    assert_eq!(r.mmio_read(DAC_CNTL, 1) & DAC_CMP_OUTPUT, 0);
}

#[test]
fn vga_crtc_writes_reach_the_extended_timings() {
    let mut r = card();
    r.vga_io_write(0x3c2, 1, 0x67);
    r.mmio_write(CRTC_V_TOTAL_DISP, 4, 0x01df_020c, &mut NoBus);
    r.vga_io_write(0x3d4, 2, 0x1f07);
    r.vga_io_write(0x3d4, 2, 0xde12);
    assert_eq!(r.mmio_read(CRTC_V_TOTAL_DISP + 2, 1), 0xde);
    r.vga_io_write(0x3d4, 1, 0x01);
    r.vga_io_write(0x3d5, 1, 0x4f);
    assert_eq!(r.mmio_read(CRTC_H_TOTAL_DISP, 4) >> 16, 0x4f);
}

#[test]
fn scanout_counts_from_display_base_addr() {
    let mut r = card();
    set_640x480x16(&mut r);
    r.mmio_write(MC_FB_LOCATION, 4, 0xf7ff_f000, &mut NoBus);
    r.mmio_write(DISPLAY_BASE_ADDR, 4, 0xf000_0000, &mut NoBus);
    assert_eq!(r.mode().unwrap().offset, 0x1000);
    assert_ne!(r.mmio_read(MC_STATUS, 4) & 1 << 4, 0);
}

#[test]
fn memory_reports_power_up_complete_after_an_sdram_reset() {
    let mut r = card();
    r.mmio_write(0x0158, 4, 1 << 31, &mut NoBus);
    r.mmio_write(0x0158, 4, 0, &mut NoBus);
    assert_eq!(r.mmio_read(MC_STATUS, 4) & 0x0f, 0x0f, "XP's ATI driver waits for every channel to power up");
}

#[test]
fn pll_atomic_updates_finish_at_once() {
    let mut r = card();
    r.io_write(8, 1, PPLL_REF_DIV as u32 | PLL_WR_EN, &mut NoBus);
    r.io_write(0xc, 4, 0x0030_800c, &mut NoBus);
    assert_eq!(r.io_read(0xc, 4), 0x0030_000c);
}

#[test]
fn edid_checksums_to_zero() {
    assert_eq!(ddc::edid().iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0);
}

#[test]
fn edid_prefers_1440x900_at_60_hz() {
    let e = ddc::edid();
    assert_ne!(e[24] & 2, 0);
    let d = &e[54..72];
    let clock_khz = u16::from_le_bytes([d[0], d[1]]) as u32 * 10;
    let h = d[2] as u32 | (d[4] as u32 >> 4) << 8;
    let hblank = d[3] as u32 | (d[4] as u32 & 15) << 8;
    let v = d[5] as u32 | (d[7] as u32 >> 4) << 8;
    let vblank = d[6] as u32 | (d[7] as u32 & 15) << 8;
    assert_eq!((h, v), (1440, 900));
    let refresh = clock_khz as f64 * 1000.0 / ((h + hblank) * (v + vblank)) as f64;
    assert!(
        (59.5..60.5).contains(&refresh),
        "{refresh} Hz is outside the range limits we report"
    );
    assert!(clock_khz <= e[81] as u32 * 10_000);
}

#[test]
fn edid_lists_the_common_lower_modes() {
    let e = ddc::edid();
    assert_eq!((e[35] & 0x21, e[36] & 0x08), (0x21, 0x08));
    let standard: Vec<(u32, u32)> = e[38..54]
        .chunks(2)
        .filter(|c| c != &[1, 1])
        .map(|c| {
            let w = (c[0] as u32 + 31) * 8;
            let h = match c[1] >> 6 {
                0 => w * 10 / 16,
                1 => w * 3 / 4,
                2 => w * 4 / 5,
                _ => w * 9 / 16,
            };
            (w, h)
        })
        .collect();
    assert_eq!(standard, [(1280, 1024), (1280, 800), (1440, 900)]);
}

fn i2c_read(r: &mut Radeon, from: u8, count: u32) -> (u32, Vec<u8>) {
    let start = I2C_GO | I2C_START | 1 << 6;
    r.mmio_write(I2C_CNTL_0, 4, I2C_DONE | I2C_NACK | I2C_HALT | I2C_SOFT_RST, &mut NoBus);
    r.mmio_write(I2C_DATA, 1, 0xa0, &mut NoBus);
    r.mmio_write(I2C_DATA, 1, from as u32, &mut NoBus);
    r.mmio_write(I2C_CNTL_1, 4, 1 << 17 | 1 << 16 | 0x100 | 1, &mut NoBus);
    r.mmio_write(I2C_CNTL_0, 4, start, &mut NoBus);
    r.mmio_write(I2C_CNTL_0, 4, I2C_DONE | I2C_NACK | I2C_HALT | I2C_SOFT_RST, &mut NoBus);
    r.mmio_write(I2C_DATA, 1, 0xa1, &mut NoBus);
    r.mmio_write(I2C_CNTL_1, 4, 1 << 17 | 1 << 16 | 0x100 | count, &mut NoBus);
    r.mmio_write(I2C_CNTL_0, 4, start | I2C_STOP | I2C_RECEIVE, &mut NoBus);
    let status = r.mmio_read(I2C_CNTL_0, 4);
    (status, (0..count).map(|_| r.mmio_read(I2C_DATA, 1) as u8).collect())
}

#[test]
fn the_i2c_engine_reads_the_edid_in_chunks() {
    let mut r = card();
    let (status, head) = i2c_read(&mut r, 0, 8);
    assert_eq!(status & (I2C_GO | I2C_DONE | I2C_NACK), I2C_DONE);
    assert_eq!(head, ddc::edid()[..8]);
    let (_, more) = i2c_read(&mut r, 0x36, 4);
    assert_eq!(more, ddc::edid()[0x36..0x3a]);
}

#[test]
fn the_i2c_engine_nacks_absent_devices() {
    let mut r = card();
    r.mmio_write(I2C_DATA, 1, 0xa4, &mut NoBus);
    r.mmio_write(I2C_CNTL_1, 4, 1, &mut NoBus);
    r.mmio_write(I2C_CNTL_0, 4, I2C_GO | I2C_START | I2C_STOP, &mut NoBus);
    assert_eq!(r.mmio_read(I2C_CNTL_0, 4) & (I2C_GO | I2C_DONE | I2C_NACK), I2C_NACK);
}

#[test]
fn an_i2c_abort_drops_go_at_once() {
    let mut r = card();
    r.mmio_write(I2C_CNTL_0 + 1, 1, (I2C_GO | I2C_ABORT) >> 8, &mut NoBus);
    assert_eq!(r.mmio_read(I2C_CNTL_0 + 1, 1) & (I2C_GO | I2C_ABORT) >> 8, 0);
}

#[test]
fn queue_status_reads_idle_for_xps_wait_loop() {
    let mut r = card();
    assert_eq!(r.mmio_read(0x0790, 4) & 0x1f, 2);
    assert_eq!(r.mmio_read(0x07b0, 4) & 0x1f, 8);
    assert_eq!(r.mmio_read(0x0a10, 4) & 0xffff, 0x3333);
}

#[test]
fn the_idct_engine_reads_idle_for_xps_mode_switch() {
    let mut r = card();
    assert_ne!(r.mmio_read(IDCT_STATUS, 4) & IDCT_IDLE, 0);
}

#[test]
fn random_source_flushes_then_hands_out_nonzero_words() {
    let mut r = card();
    for cntl in RNG_CNTL {
        r.mmio_write(cntl, 4, RNG_FLUSH, &mut NoBus);
        assert_eq!(r.mmio_read(cntl, 4), 0, "a flushed channel at {cntl:#x} must read empty");
        r.mmio_write(cntl, 4, 0, &mut NoBus);
        assert_eq!(r.mmio_read(cntl, 4), RNG_READY, "a started channel at {cntl:#x} must have a word ready");
        let a = r.mmio_read(cntl + 4, 4);
        let b = r.mmio_read(cntl + 4, 4);
        assert!(a != 0 && b != 0 && a != b, "the driver rejects zero and expects fresh words: {a:#x} {b:#x}");
        assert_eq!(r.mmio_read(cntl, 4), RNG_READY);
    }
}

#[test]
fn random_source_never_completes_a_key_exchange() {
    let mut r = card();
    let cntl = RNG_CNTL[0];
    r.mmio_write(cntl, 4, 0, &mut NoBus);
    r.mmio_write(cntl + 4, 4, 0x1234_5678, &mut NoBus);
    for _ in 0..3 {
        assert_eq!(r.mmio_read(cntl, 4), RNG_BUSY, "the driver's bounded wait must time out instead of seeing a reply");
    }
    r.mmio_write(cntl, 4, RNG_FLUSH, &mut NoBus);
    assert_eq!(r.mmio_read(cntl, 4), 0, "a flush must still empty the channel");
}

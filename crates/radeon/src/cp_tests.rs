use super::*;

struct Ram(Vec<u8>);

impl Ram {
    fn new() -> Self {
        Ram(vec![0; 16 << 20])
    }

    fn put(&mut self, addr: u32, dwords: &[u32]) {
        for (i, d) in dwords.iter().enumerate() {
            let a = addr as usize + 4 * i;
            self.0[a..a + 4].copy_from_slice(&d.to_le_bytes());
        }
    }

    fn get(&self, addr: u32) -> u32 {
        let a = addr as usize;
        u32::from_le_bytes(self.0[a..a + 4].try_into().unwrap())
    }
}

impl BusMemory for Ram {
    fn read(&mut self, addr: u32, buf: &mut [u8]) {
        let a = addr as usize;
        buf.copy_from_slice(&self.0[a..a + buf.len()]);
    }

    fn write(&mut self, addr: u32, buf: &[u8]) {
        let a = addr as usize;
        self.0[a..a + buf.len()].copy_from_slice(buf);
    }
}

const AGP: u32 = 0xf800_0000;
const AGP_BUS: u32 = 0x0010_0000;
const RING: u32 = AGP;
const RPTR_ADDR: u32 = AGP + 0x1_0000;
const RING_DWORDS: u32 = 128;

fn pkt0(reg: u32, count: u32) -> u32 {
    (count - 1) << 16 | reg >> 2
}

fn pkt3(opcode: u8, count: u32) -> u32 {
    0xc000_0000 | (count - 1) << 16 | (opcode as u32) << 8
}

/// The 2.6.24 DRM's layout: VRAM at 0xF0000000, a ring at the AGP aperture's start.
fn card_with_ring(ram: &mut Ram) -> Radeon {
    let mut r = Radeon::new(vec![0x55, 0xaa]);
    let mut w = |reg, v| r.mmio_write(reg, 4, v, ram);
    w(MC_FB_LOCATION, 0xf7ff_f000);
    w(MC_AGP_LOCATION, 0xf87f_f800);
    w(AGP_BASE, AGP_BUS);
    w(CP_RB_BASE, RING);
    w(CP_RB_CNTL, 6);
    w(CP_RB_RPTR_ADDR, RPTR_ADDR);
    w(SCRATCH_ADDR, RPTR_ADDR + 0x20);
    w(SCRATCH_UMSK, 0x7);
    w(CP_CSQ_CNTL, 0x4000_0000);
    r
}

fn submit(r: &mut Radeon, ram: &mut Ram, packets: &[u32]) {
    let mut wptr = r.mmio_read(CP_RB_WPTR, 4);
    for &d in packets {
        ram.put(AGP_BUS + 4 * wptr, &[d]);
        wptr = (wptr + 1) % RING_DWORDS;
    }
    r.mmio_write(CP_RB_WPTR, 4, wptr, ram);
}

fn px32(r: &Radeon, x: usize, y: usize) -> u32 {
    u32::from_le_bytes(r.vram()[y * 256 + x * 4..][..4].try_into().unwrap())
}

#[test]
fn ring_packets_write_registers_and_the_read_pointer_is_written_back() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    submit(
        &mut r,
        &mut ram,
        &[pkt0(SCRATCH_REG0 + 8, 1), 0x1234, 0x8000_0000],
    );
    assert_eq!(r.mmio_read(SCRATCH_REG0 + 8, 4), 0x1234);
    assert_eq!(r.mmio_read(CP_RB_RPTR, 4), 3);
    assert_eq!(ram.get(AGP_BUS + 0x1_0000), 3);
    assert_eq!(ram.get(AGP_BUS + 0x1_0028), 0x1234);
}

#[test]
fn scratch_writes_from_the_cpu_reach_memory_only_when_unmasked() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    r.mmio_write(SCRATCH_REG0 + 4, 4, 0xdead_beef, &mut ram);
    r.mmio_write(SCRATCH_REG0 + 12, 4, 0x5555, &mut ram);
    assert_eq!(ram.get(AGP_BUS + 0x1_0024), 0xdead_beef);
    assert_eq!(ram.get(AGP_BUS + 0x1_002c), 0);
}

#[test]
fn resync_writes_land_in_the_scratch_register_they_name() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    submit(&mut r, &mut ram, &[pkt0(CP_RESYNC_ADDR, 2), 2, 0x77]);
    assert_eq!(r.mmio_read(SCRATCH_REG0 + 8, 4), 0x77);
    assert_eq!(ram.get(AGP_BUS + 0x1_0028), 0x77);
}

#[test]
fn indirect_buffers_run_from_the_ring() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    let ib = AGP + 0x2_0000;
    ram.put(
        AGP_BUS + 0x2_0000,
        &[pkt0(SCRATCH_REG0, 2), 7, 8, 0x8000_0000],
    );
    submit(
        &mut r,
        &mut ram,
        &[pkt0(CP_IB_BASE, 2), ib, 4, pkt0(SCRATCH_REG0 + 8, 1), 9],
    );
    assert_eq!(
        [0, 4, 8].map(|o| ram.get(AGP_BUS + 0x1_0020 + o)),
        [7, 8, 9]
    );
}

#[test]
fn packets_that_cross_the_end_of_the_ring_run_whole() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    r.mmio_write(CP_RB_CNTL, 4, 6 | RB_RPTR_WR_ENA, &mut ram);
    r.mmio_write(CP_RB_RPTR_WR, 4, 126, &mut ram);
    r.mmio_write(CP_RB_WPTR, 4, 126, &mut ram);
    r.mmio_write(CP_RB_CNTL, 4, 6, &mut ram);
    submit(&mut r, &mut ram, &[pkt0(SCRATCH_REG0, 2), 0x11, 0x22]);
    assert_eq!(r.mmio_read(SCRATCH_REG0 + 4, 4), 0x22);
    assert_eq!(r.mmio_read(CP_RB_RPTR, 4), 1);
}

#[test]
fn a_packet_cut_short_by_the_write_pointer_waits() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    ram.put(AGP_BUS, &[pkt0(SCRATCH_REG0, 2), 0x11, 0x22]);
    r.mmio_write(CP_RB_WPTR, 4, 2, &mut ram);
    assert_eq!(r.mmio_read(CP_RB_RPTR, 4), 0);
    assert_eq!(r.mmio_read(SCRATCH_REG0, 4), 0);
    r.mmio_write(CP_RB_WPTR, 4, 3, &mut ram);
    assert_eq!(r.mmio_read(SCRATCH_REG0 + 4, 4), 0x22);
}

#[test]
fn the_ring_waits_while_the_cp_is_disabled() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    r.mmio_write(CP_CSQ_CNTL, 4, 0, &mut ram);
    submit(&mut r, &mut ram, &[pkt0(SCRATCH_REG0, 1), 5]);
    assert_eq!(r.mmio_read(CP_RB_RPTR, 4), 0);
    r.mmio_write(CP_CSQ_CNTL, 4, 0x4000_0000, &mut ram);
    assert_eq!(r.mmio_read(SCRATCH_REG0, 4), 5);
}

#[test]
fn every_primary_bus_master_mode_runs_the_ring() {
    for (cntl, mode) in [(0x2000_0000, 0), (0x6000_0000, 0), (0x8000_0000, 0), (0, CSQ_PRIMARY_ENABLE | CSQ_PRIMARY_BM)] {
        let mut ram = Ram::new();
        let mut r = card_with_ring(&mut ram);
        r.mmio_write(CP_CSQ_CNTL, 4, cntl, &mut ram);
        r.mmio_write(CP_CSQ_MODE, 4, mode, &mut ram);
        submit(&mut r, &mut ram, &[pkt0(SCRATCH_REG0, 1), 5]);
        assert_eq!((cntl, mode, r.mmio_read(SCRATCH_REG0, 4)), (cntl, mode, 5));
    }
}

#[test]
fn one_reg_wr_packets_write_one_register_repeatedly() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    submit(&mut r, &mut ram, &[pkt0(SCRATCH_REG0, 3) | 0x8000, 1, 2, 3]);
    assert_eq!(r.mmio_read(SCRATCH_REG0, 4), 3);
    assert_eq!(r.mmio_read(SCRATCH_REG0 + 4, 4), 0);
}

#[test]
fn the_pci_gart_maps_pages_through_its_table() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    let table = 0x0020_0000;
    ram.put(table, &[0x0030_0000, 0x0050_0000]);
    let mut w = |reg, v| r.mmio_write(reg, 4, v, &mut ram);
    w(AIC_PT_BASE, table);
    w(AIC_LO_ADDR, 0xe000_0000);
    w(AIC_HI_ADDR, 0xe0ff_ffff);
    w(AIC_CNTL, PCIGART_TRANSLATE_EN);
    w(SCRATCH_ADDR, 0xe000_1000 - 4);
    w(SCRATCH_REG0, 0xaaaa);
    w(SCRATCH_REG0 + 4, 0xbbbb);
    assert_eq!(ram.get(0x0030_0ffc), 0xaaaa);
    assert_eq!(ram.get(0x0050_0000), 0xbbbb);
}

#[test]
fn software_interrupts_raise_inta_until_acknowledged() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    r.mmio_write(GEN_INT_CNTL, 4, 1 << 25, &mut ram);
    assert!(!r.irq());
    submit(&mut r, &mut ram, &[pkt0(GEN_INT_STATUS, 1), SW_INT_FIRE]);
    assert!(r.irq());
    assert_ne!(r.mmio_read(GEN_INT_STATUS, 4) & SW_INT_TEST, 0);
    r.mmio_write(GEN_INT_STATUS, 4, SW_INT_TEST, &mut ram);
    assert!(!r.irq());
}

#[test]
fn vertical_blank_interrupts_follow_gen_int_cntl() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    r.set_time(1_000_000_000);
    assert!(!r.irq());
    r.mmio_write(GEN_INT_CNTL, 4, CRTC_VBLANK_STAT, &mut ram);
    assert!(r.irq());
    r.mmio_write(GEN_INT_STATUS, 4, CRTC_VBLANK_STAT, &mut ram);
    assert!(!r.irq());
    let next = r.next_irq_ns().unwrap();
    r.set_time(next);
    assert!(r.irq());
}

#[test]
fn paint_multi_packets_fill_vram() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    let gmc = GMC_DST_PITCH_OFFSET_CNTL | 13 << 4 | 6 << 8 | 3 << 12 | 0xf0 << 16;
    let pitch_offset = (256 / 64) << 22 | 0xf000_0000 >> 10;
    submit(
        &mut r,
        &mut ram,
        &[
            pkt3(cp::op::PAINT_MULTI, 5),
            gmc,
            pitch_offset,
            0x00ff_8040,
            2 << 16 | 1,
            2 << 16 | 1,
        ],
    );
    assert_eq!(px32(&r, 2, 1), 0x00ff_8040);
    assert_eq!(px32(&r, 3, 1), 0x00ff_8040);
    assert_eq!(px32(&r, 4, 1), 0);
    assert_eq!(px32(&r, 2, 2), 0);
}

#[test]
fn hostdata_blt_packets_expand_monochrome_text() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    let gmc = GMC_DST_PITCH_OFFSET_CNTL
        | GMC_DST_CLIPPING
        | 15 << 4
        | 6 << 8
        | 1 << 12
        | 1 << 14
        | 0xcc << 16
        | 3 << 24;
    let pitch_offset = (256 / 64) << 22 | 0xf000_0000 >> 10;
    let packet = [
        gmc,
        pitch_offset,
        0,
        2 << 16 | 4,
        0x00ff_ffff,
        0,
        0,
        2 << 16 | 32,
        2,
        0b0101,
        0b1_1010,
    ];
    let mut ring = vec![pkt3(cp::op::HOSTDATA_BLT, packet.len() as u32)];
    ring.extend_from_slice(&packet);
    r.vram_mut()[256 + 4 * 4..][..4].copy_from_slice(&0x77u32.to_le_bytes());
    submit(&mut r, &mut ram, &ring);
    assert_eq!(
        [0, 1, 2, 3].map(|x| px32(&r, x, 0)),
        [0xff_ffff, 0, 0xff_ffff, 0]
    );
    assert_eq!(
        [0, 1, 2, 3, 4].map(|x| px32(&r, x, 1)),
        [0, 0xff_ffff, 0, 0xff_ffff, 0x77]
    );
}

#[test]
fn trans_bitblt_packets_skip_the_source_reference_colour() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    r.vram_mut()[..8].copy_from_slice(&[7, 0, 0, 0, 8, 0, 0, 0]);
    let gmc = GMC_SRC_PITCH_OFFSET_CNTL | GMC_DST_PITCH_OFFSET_CNTL | 15 << 4 | 6 << 8 | 3 << 12 | 0xcc << 16 | 2 << 24;
    let po = (256 / 64) << 22 | 0xf000_0000 >> 10;
    submit(&mut r, &mut ram, &[pkt3(cp::op::TRANS_BITBLT, 9), gmc, po, po, 4 | 1 << 24, 7, 0, 0, 1, 2 << 16 | 1]);
    assert_eq!([px32(&r, 0, 1), px32(&r, 1, 1)], [0, 8]);
}

#[test]
fn nextchar_packets_draw_a_glyph_with_the_current_colours() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    let gmc = GMC_DST_PITCH_OFFSET_CNTL | 15 << 4 | 6 << 8 | 1 << 14 | 0xcc << 16 | 3 << 24;
    let po = (256 / 64) << 22 | 0xf000_0000 >> 10;
    r.mmio_write(DP_GUI_MASTER_CNTL, 4, gmc, &mut ram);
    r.mmio_write(DST_PITCH_OFFSET, 4, po, &mut ram);
    r.mmio_write(DP_SRC_FRGD_CLR, 4, 0xaa, &mut ram);
    r.mmio_write(DP_SRC_BKGD_CLR, 4, 0x11, &mut ram);
    submit(&mut r, &mut ram, &[pkt3(cp::op::NEXTCHAR, 3), 1 << 16 | 2, 1 << 16 | 4, 0b0110]);
    assert_eq!([2, 3, 4, 5].map(|x| px32(&r, x, 1)), [0x11, 0xaa, 0xaa, 0x11]);
}

#[test]
fn hostdata_blt_translates_8bpp_pixels_through_the_loaded_palette() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    let gmc = GMC_DST_PITCH_OFFSET_CNTL | 15 << 4 | 6 << 8 | 1 << 12 | 0xcc << 16 | 4 << 24 | 1 << 27;
    let po = (256 / 64) << 22 | 0xf000_0000 >> 10;
    let mut ring = vec![pkt3(cp::op::LOAD_PALETTE, 17), 1];
    ring.extend((0..16).map(|i| 0x0010_0000 * i + i));
    ring.extend_from_slice(&[pkt3(cp::op::HOSTDATA_BLT, 9), gmc, po, 0, 0, 1 << 16 | 1, 2 << 16 | 3, 2, 0x0302_0100, 0x0f05]);
    submit(&mut r, &mut ram, &ring);
    assert_eq!([1, 2, 3].map(|x| px32(&r, x, 1)), [0x0000_0000, 0x0010_0001, 0x0020_0002]);
    assert_eq!([1, 2, 3].map(|x| px32(&r, x, 2)), [0x0030_0003, 0x0050_0005, 0x00f0_000f]);
}

#[test]
fn an_empty_gui_dma_list_at_the_end_of_a_buffer_raises_the_guidma_interrupt() {
    let mut ram = Ram::new();
    let mut r = card_with_ring(&mut ram);
    r.mmio_write(GEN_INT_CNTL, 4, GUIDMA_INT, &mut ram);
    submit(&mut r, &mut ram, &[pkt0(SCRATCH_REG0 + 0x18, 1), 0x1c, pkt0(GUI_DMA_CMD, 1), GUI_DMA_EOL | GUI_DMA_INTDIS]);
    assert!(!r.irq(), "INTDIS must keep the list silent");
    submit(&mut r, &mut ram, &[pkt0(SCRATCH_REG0 + 0x18, 1), 0x1d, pkt0(GUI_DMA_CMD, 1), GUI_DMA_EOL]);
    assert!(r.irq(), "Vista's scheduler waits for this interrupt to retire the buffer");
    assert_ne!(r.mmio_read(GEN_INT_STATUS, 4) & GUIDMA_INT, 0);
    assert_eq!(r.mmio_read(SCRATCH_REG0 + 0x18, 4), 0x1d, "the fence must be visible when the interrupt is");
    r.mmio_write(GEN_INT_STATUS, 4, GUIDMA_INT, &mut ram);
    assert!(!r.irq(), "writing the status bit back acknowledges it");
}

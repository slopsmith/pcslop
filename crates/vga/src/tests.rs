use crate::vbe::*;
use crate::*;

struct Mode {
    misc: u8,
    seq: [u8; 4],
    crtc: [u8; 25],
    attr: [u8; 20],
    gfx: [u8; 9],
}

// Standard IBM VGA register values for the BIOS modes, as listed in
// SeaVGABIOS vgasrc/stdvgamodes.c.
const MODE_03: Mode = Mode {
    misc: 0x67,
    seq: [0x00, 0x03, 0x00, 0x02],
    crtc: [
        0x5f, 0x4f, 0x50, 0x82, 0x55, 0x81, 0xbf, 0x1f, 0x00, 0x4f, 0x0d, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x9c, 0x8e,
        0x8f, 0x28, 0x1f, 0x96, 0xb9, 0xa3, 0xff,
    ],
    attr: [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x0c, 0x00,
        0x0f, 0x08,
    ],
    gfx: [0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x0e, 0x0f, 0xff],
};

const MODE_04: Mode = Mode {
    misc: 0x63,
    seq: [0x09, 0x03, 0x00, 0x02],
    crtc: [
        0x2d, 0x27, 0x28, 0x90, 0x2b, 0x80, 0xbf, 0x1f, 0x00, 0xc1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x9c, 0x8e,
        0x8f, 0x14, 0x00, 0x96, 0xb9, 0xa2, 0xff,
    ],
    attr: [
        0x00, 0x13, 0x15, 0x17, 0x02, 0x04, 0x06, 0x07, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x01, 0x00,
        0x03, 0x00,
    ],
    gfx: [0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x0f, 0x0f, 0xff],
};

const MODE_12: Mode = Mode {
    misc: 0xe3,
    seq: [0x01, 0x0f, 0x00, 0x06],
    crtc: [
        0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0x0b, 0x3e, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xea, 0x8c,
        0xdf, 0x28, 0x00, 0xe7, 0x04, 0xe3, 0xff,
    ],
    attr: [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x01, 0x00,
        0x0f, 0x00,
    ],
    gfx: [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x0f, 0xff],
};

const MODE_13: Mode = Mode {
    misc: 0x63,
    seq: [0x01, 0x0f, 0x00, 0x0e],
    crtc: [
        0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0xbf, 0x1f, 0x00, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x9c, 0x8e,
        0x8f, 0x28, 0x40, 0x96, 0xb9, 0xa3, 0xff,
    ],
    attr: [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x41, 0x00,
        0x0f, 0x00,
    ],
    gfx: [0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x05, 0x0f, 0xff],
};

fn out8(v: &mut Vga, port: u16, val: u8) {
    assert!(v.io_write(port, 1, val as u32));
}

fn in8(v: &mut Vga, port: u16) -> u8 {
    v.io_read(port, 1, 0).unwrap() as u8
}

fn seq(v: &mut Vga, i: u8, val: u8) {
    v.io_write(0x3c4, 2, (val as u32) << 8 | i as u32);
}

fn gfx(v: &mut Vga, i: u8, val: u8) {
    v.io_write(0x3ce, 2, (val as u32) << 8 | i as u32);
}

fn crtc(v: &mut Vga, i: u8, val: u8) {
    v.io_write(0x3d4, 2, (val as u32) << 8 | i as u32);
}

fn attr(v: &mut Vga, i: u8, val: u8) {
    in8(v, 0x3da);
    out8(v, 0x3c0, i);
    out8(v, 0x3c0, val);
}

fn enable_display(v: &mut Vga) {
    in8(v, 0x3da);
    out8(v, 0x3c0, 0x20);
}

fn dac(v: &mut Vga, i: u8, rgb: [u8; 3]) {
    out8(v, 0x3c8, i);
    for c in rgb {
        out8(v, 0x3c9, c);
    }
}

fn set_mode(v: &mut Vga, m: &Mode) {
    out8(v, 0x3c2, m.misc);
    for (i, &a) in m.attr.iter().enumerate() {
        attr(v, i as u8, a);
    }
    attr(v, 0x14, 0);
    seq(v, 0, 0x03);
    for (i, &s) in m.seq.iter().enumerate() {
        seq(v, i as u8 + 1, s);
    }
    for (i, &g) in m.gfx.iter().enumerate() {
        gfx(v, i as u8, g);
    }
    crtc(v, 0x11, 0);
    for (i, &c) in m.crtc.iter().enumerate() {
        crtc(v, i as u8, c);
    }
    enable_display(v);
}

fn rgb6(r: u8, g: u8, b: u8) -> u32 {
    let e = |c: u8| ((c << 2) | (c >> 4)) as u32;
    e(r) << 16 | e(g) << 8 | e(b)
}

fn planar_setup(v: &mut Vga) {
    seq(v, 2, 0x0f);
    seq(v, 4, 0x06);
    gfx(v, 6, 0x05);
    gfx(v, 8, 0xff);
}

fn plane_bytes(v: &Vga, offset: usize) -> [u8; 4] {
    v.vram()[offset * 4..offset * 4 + 4].try_into().unwrap()
}

fn vbe_set(v: &mut Vga, i: u16, val: u16) {
    v.io_write(0x1ce, 2, i as u32);
    v.io_write(0x1cf, 2, val as u32);
}

fn vbe_get(v: &mut Vga, i: u16) -> u16 {
    v.io_write(0x1ce, 2, i as u32);
    v.io_read(0x1cf, 2, 0).unwrap() as u16
}

fn vbe_mode(v: &mut Vga, w: u16, h: u16, bpp: u16, flags: u16) {
    vbe_set(v, INDEX_ENABLE, 0);
    vbe_set(v, INDEX_BPP, bpp);
    vbe_set(v, INDEX_XRES, w);
    vbe_set(v, INDEX_YRES, h);
    vbe_set(v, INDEX_BANK, 0);
    vbe_set(v, INDEX_ENABLE, ENABLED | flags);
    enable_display(v);
}

#[test]
fn ignores_ports_outside_vga_and_dispi() {
    let mut v = Vga::new();
    assert_eq!(v.io_read(0x60, 1, 0), None);
    assert!(!v.io_write(0x3e0, 1, 0));
}

#[test]
fn word_write_sets_index_then_data() {
    let mut v = Vga::new();
    v.io_write(0x3c4, 2, 0x0f02);
    assert_eq!(in8(&mut v, 0x3c4), 2);
    assert_eq!(in8(&mut v, 0x3c5), 0x0f);
    v.io_write(0x3ce, 2, 0xff05);
    assert_eq!(v.io_read(0x3ce, 2, 0), Some(0x7b05));
}

#[test]
fn crtc_follows_misc_output_io_select() {
    let mut v = Vga::new();
    out8(&mut v, 0x3c2, 0x01);
    crtc(&mut v, 0x0c, 0x12);
    assert_eq!(in8(&mut v, 0x3b5), 0xff);
    out8(&mut v, 0x3c2, 0x00);
    v.io_write(0x3b4, 2, 0x340c);
    assert_eq!(in8(&mut v, 0x3b5), 0x34);
    out8(&mut v, 0x3c2, 0x01);
    out8(&mut v, 0x3d4, 0x0c);
    assert_eq!(in8(&mut v, 0x3d5), 0x34);
}

#[test]
fn crtc_protect_locks_registers_0_to_7_except_line_compare_bit() {
    let mut v = Vga::new();
    out8(&mut v, 0x3c2, 0x01);
    crtc(&mut v, 0x01, 0x4f);
    crtc(&mut v, 0x07, 0x00);
    crtc(&mut v, 0x11, 0x80);
    crtc(&mut v, 0x01, 0x27);
    crtc(&mut v, 0x07, 0xff);
    crtc(&mut v, 0x13, 0x28);
    let mut read = |i| {
        out8(&mut v, 0x3d4, i);
        in8(&mut v, 0x3d5)
    };
    assert_eq!(read(0x01), 0x4f);
    assert_eq!(read(0x07), 0x10);
    assert_eq!(read(0x13), 0x28);
}

#[test]
fn reading_input_status_resets_attribute_flip_flop() {
    let mut v = Vga::new();
    out8(&mut v, 0x3c2, 0x01);
    in8(&mut v, 0x3da);
    out8(&mut v, 0x3c0, 0x03);
    out8(&mut v, 0x3c0, 0x2a);
    assert_eq!(in8(&mut v, 0x3c1), 0x2a);

    out8(&mut v, 0x3c0, 0x05);
    in8(&mut v, 0x3da);
    out8(&mut v, 0x3c0, 0x23);
    assert_eq!(in8(&mut v, 0x3c0), 0x23);
    out8(&mut v, 0x3c0, 0x11);
    assert_eq!(in8(&mut v, 0x3c1), 0x11);
}

#[test]
fn dac_auto_increments_and_keeps_six_bits() {
    let mut v = Vga::new();
    dac(&mut v, 5, [0xff, 0x20, 0x01]);
    dac(&mut v, 6, [0x02, 0x03, 0x04]);
    assert_eq!(in8(&mut v, 0x3c8), 7);
    out8(&mut v, 0x3c7, 5);
    assert_eq!(in8(&mut v, 0x3c7), 3);
    let read: Vec<u8> = (0..6).map(|_| in8(&mut v, 0x3c9)).collect();
    assert_eq!(read, [0x3f, 0x20, 0x01, 0x02, 0x03, 0x04]);
}

#[test]
fn write_mode_0_applies_set_reset_per_enabled_plane() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    gfx(&mut v, 1, 0x03);
    gfx(&mut v, 0, 0x01);
    v.mem_write8(0xa0000, 0x5a);
    assert_eq!(plane_bytes(&v, 0), [0xff, 0x00, 0x5a, 0x5a]);
}

#[test]
fn write_mode_0_rotates_and_combines_with_latches() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    v.mem_write8(0xa0000, 0xf0);
    v.mem_read8(0xa0000);
    gfx(&mut v, 3, 0x18 | 4);
    gfx(&mut v, 8, 0x3c);
    v.mem_write8(0xa0000, 0x0f);
    assert_eq!(plane_bytes(&v, 0), [0xc0; 4]);
}

#[test]
fn map_mask_limits_written_planes() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    seq(&mut v, 2, 0x06);
    v.mem_write8(0xa0010, 0x77);
    assert_eq!(plane_bytes(&v, 0x10), [0, 0x77, 0x77, 0]);
}

#[test]
fn write_mode_1_copies_latches() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    gfx(&mut v, 1, 0x0f);
    gfx(&mut v, 0, 0x09);
    v.mem_write8(0xa0000, 0);
    gfx(&mut v, 1, 0);
    v.mem_read8(0xa0000);
    gfx(&mut v, 5, 0x01);
    v.mem_write8(0xa1234, 0x00);
    assert_eq!(plane_bytes(&v, 0x1234), [0xff, 0x00, 0x00, 0xff]);
}

#[test]
fn write_mode_2_expands_color_under_bit_mask() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    gfx(&mut v, 5, 0x02);
    gfx(&mut v, 8, 0x81);
    v.mem_read8(0xa0000);
    v.mem_write8(0xa0000, 0x0a);
    assert_eq!(plane_bytes(&v, 0), [0x00, 0x81, 0x00, 0x81]);
}

#[test]
fn write_mode_3_uses_rotated_data_as_bit_mask() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    gfx(&mut v, 1, 0x0f);
    gfx(&mut v, 0, 0x03);
    v.mem_write8(0xa0000, 0);
    gfx(&mut v, 1, 0);
    v.mem_read8(0xa0000);
    gfx(&mut v, 5, 0x03);
    gfx(&mut v, 0, 0x0c);
    gfx(&mut v, 3, 0x01);
    gfx(&mut v, 8, 0x7f);
    v.mem_write8(0xa0000, 0xe1);
    assert_eq!(plane_bytes(&v, 0), [0x8f, 0x8f, 0x70, 0x70]);
}

#[test]
fn read_mode_0_returns_selected_plane() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    v.vram_mut()[0x40 * 4..0x40 * 4 + 4].copy_from_slice(&[1, 2, 3, 4]);
    gfx(&mut v, 4, 2);
    assert_eq!(v.mem_read8(0xa0040), 3);
}

#[test]
fn read_mode_1_compares_colors_with_dont_care() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    v.vram_mut()[0..4].copy_from_slice(&[0b1100, 0b1010, 0b0000, 0b1111]);
    gfx(&mut v, 5, 0x08);
    gfx(&mut v, 2, 0b1001);
    gfx(&mut v, 7, 0x0f);
    assert_eq!(v.mem_read8(0xa0000), 0b0000_0100);
    gfx(&mut v, 7, 0b1000);
    assert_eq!(v.mem_read8(0xa0000), 0b0000_1111);
}

#[test]
fn chain4_selects_plane_from_low_address_bits() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_13);
    for i in 0..8u32 {
        v.mem_write8(0xa0000 + i, 0x10 + i as u8);
    }
    assert_eq!(plane_bytes(&v, 0), [0x10, 0x11, 0x12, 0x13]);
    assert_eq!(plane_bytes(&v, 4), [0x14, 0x15, 0x16, 0x17]);
    assert_eq!(v.mem_read8(0xa0006), 0x16);
}

#[test]
fn odd_even_splits_characters_and_attributes() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_03);
    v.mem_write8(0xb8000, b'A');
    v.mem_write8(0xb8001, 0x1e);
    v.mem_write8(0xb8002, b'B');
    assert_eq!(plane_bytes(&v, 0)[..2], [b'A', 0x1e]);
    assert_eq!(plane_bytes(&v, 2)[0], b'B');
    assert_eq!(v.mem_read8(0xb8001), 0x1e);
    assert_eq!(v.mem_read8(0xb8002), b'B');
}

#[test]
fn memory_map_select_picks_the_window() {
    let mut v = Vga::new();
    planar_setup(&mut v);
    for (mm, inside, outside) in [(0, 0xb_ffff, 0xc_0000), (1, 0xa_ffff, 0xb_0000), (2, 0xb_7fff, 0xb_8000), (3, 0xb_8000, 0xb_7fff)] {
        gfx(&mut v, 6, 0x01 | mm << 2);
        v.mem_write8(inside, 0x42);
        assert_eq!(v.mem_read8(inside), 0x42, "map {mm}");
        assert_eq!(v.mem_read8(outside), 0xff, "map {mm}");
    }
}

fn load_glyph(v: &mut Vga, ch: u8, rows: &[u8]) {
    seq(v, 2, 0x04);
    seq(v, 4, 0x06);
    gfx(v, 4, 0x02);
    gfx(v, 5, 0x00);
    gfx(v, 6, 0x04);
    for (i, &r) in rows.iter().enumerate() {
        v.mem_write8(0xa0000 + ch as u32 * 32 + i as u32, r);
    }
    seq(v, 2, 0x03);
    seq(v, 4, 0x02);
    gfx(v, 4, 0x00);
    gfx(v, 5, 0x10);
    gfx(v, 6, 0x0e);
}

fn text_setup() -> Vga {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_03);
    dac(&mut v, 0x01, [0, 0, 0x2a]);
    dac(&mut v, 0x3e, [0x3f, 0x3f, 0x15]);
    load_glyph(&mut v, b'A', &[0x81; 16]);
    load_glyph(&mut v, 0xc4, &[0x01; 16]);
    crtc(&mut v, 0x0e, 0x07);
    v
}

const YELLOW: u32 = 0xffff55;
const BLUE: u32 = 0x0000aa;

#[test]
fn text_mode_draws_glyph_with_attribute_colors() {
    let mut v = text_setup();
    v.mem_write8(0xb8000, b'A');
    v.mem_write8(0xb8001, 0x1e);
    let mut out = Vec::new();
    assert_eq!(v.render_at(&mut out, 0), Some((720, 400)));
    let (fg, bg) = (YELLOW, BLUE);
    assert_eq!(out[..10], [fg, bg, bg, bg, bg, bg, bg, fg, bg, 0]);
    assert_eq!(out[15 * 720 + 7], fg);
}

#[test]
fn ninth_column_repeats_only_for_line_graphics() {
    let mut v = text_setup();
    for (i, ch) in [b'A', 0xc4].into_iter().enumerate() {
        v.mem_write8(0xb8000 + i as u32 * 2, ch);
        v.mem_write8(0xb8001 + i as u32 * 2, 0x1e);
    }
    let mut out = Vec::new();
    v.render_at(&mut out, 0);
    assert_eq!(out[8], BLUE);
    assert_eq!(out[9 + 8], YELLOW);
    attr(&mut v, 0x10, 0x08);
    enable_display(&mut v);
    v.render_at(&mut out, 0);
    assert_eq!(out[9 + 8], BLUE);
}

#[test]
fn text_cursor_blinks_on_configured_scanlines() {
    let mut v = text_setup();
    v.mem_write8(0xb8003, 0x1e);
    crtc(&mut v, 0x0e, 0x00);
    crtc(&mut v, 0x0f, 0x01);
    let mut out = Vec::new();
    v.render_at(&mut out, 0);
    assert_eq!(out[12 * 720 + 9], BLUE);
    assert_eq!(out[13 * 720 + 9], YELLOW);
    assert_eq!(out[14 * 720 + 17], YELLOW);
    assert_eq!(out[15 * 720 + 9], BLUE);
    v.render_at(&mut out, 300_000_000);
    assert_eq!(out[13 * 720 + 9], BLUE);
}

#[test]
fn blink_attribute_hides_glyph_and_limits_background() {
    let mut v = text_setup();
    v.mem_write8(0xb8000, b'A');
    v.mem_write8(0xb8001, 0x9e);
    let mut out = Vec::new();
    v.render_at(&mut out, 0);
    assert_eq!(out[..2], [YELLOW, BLUE]);
    v.render_at(&mut out, 600_000_000);
    assert_eq!(out[..2], [BLUE, BLUE]);
}

#[test]
fn text_start_address_scrolls_by_characters() {
    let mut v = text_setup();
    v.mem_write8(0xb8000 + 160, b'A');
    v.mem_write8(0xb8001 + 160, 0x1e);
    crtc(&mut v, 0x0d, 80);
    let mut out = Vec::new();
    v.render_at(&mut out, 0);
    assert_eq!(out[0], YELLOW);
}

#[test]
fn mode_13h_doubles_pixels_in_both_directions() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_13);
    dac(&mut v, 0x20, [0x3f, 0, 0]);
    v.mem_write8(0xa0000 + 3 * 320 + 5, 0x20);
    let mut out = Vec::new();
    assert_eq!(v.render(&mut out), Some((640, 400)));
    let red = rgb6(0x3f, 0, 0);
    for (x, y) in [(10, 6), (11, 6), (10, 7), (11, 7)] {
        assert_eq!(out[y * 640 + x], red);
    }
    assert_eq!(out.iter().filter(|&&p| p == red).count(), 4);
}

#[test]
fn unchained_256_color_reads_planes_sequentially() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_13);
    seq(&mut v, 4, 0x06);
    crtc(&mut v, 0x14, 0x00);
    crtc(&mut v, 0x17, 0xe3);
    dac(&mut v, 0x07, [0, 0x3f, 0]);
    seq(&mut v, 2, 0x02);
    v.mem_write8(0xa0000 + 80 + 1, 0x07);
    let mut out = Vec::new();
    v.render(&mut out);
    let green = rgb6(0, 0x3f, 0);
    // Plane 1 at offset 81 is pixel x = 4 * 1 + 1, y = 1.
    assert_eq!(out[2 * 640 + 10], green);
    assert_eq!(out[2 * 640 + 11], green);
    assert_eq!(out.iter().filter(|&&p| p == green).count(), 4);
}

#[test]
fn planar_16_color_mode_combines_four_planes() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_12);
    dac(&mut v, 0x05, [0x2a, 0, 0x2a]);
    gfx(&mut v, 5, 0x02);
    gfx(&mut v, 8, 0x80 >> (9 & 7));
    let addr = 0xa0000 + 2 * 80 + 9 / 8;
    v.mem_read8(addr);
    v.mem_write8(addr, 5);
    let mut out = Vec::new();
    assert_eq!(v.render(&mut out), Some((640, 480)));
    let magenta = rgb6(0x2a, 0, 0x2a);
    assert_eq!(out[2 * 640 + 9], magenta);
    assert_eq!(out.iter().filter(|&&p| p == magenta).count(), 1);
}

#[test]
fn color_plane_enable_masks_pixel_bits() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_12);
    dac(&mut v, 0x01, [0x3f, 0x3f, 0x3f]);
    seq(&mut v, 2, 0x01);
    v.mem_write8(0xa0000, 0x80);
    let mut out = Vec::new();
    v.render(&mut out);
    assert_eq!(out[0], 0xffffff);
    attr(&mut v, 0x12, 0x0e);
    enable_display(&mut v);
    v.render(&mut out);
    assert_eq!(out[0], 0);
}

#[test]
fn cga_four_color_mode_interleaves_scanlines_and_pixels() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_04);
    let colors = [[0x3f, 0, 0], [0, 0x3f, 0], [0, 0, 0x3f]];
    for (i, c) in [0x13, 0x15, 0x17].into_iter().zip(colors) {
        dac(&mut v, i, c);
    }
    v.mem_write8(0xb8000, 0b00_01_10_11);
    v.mem_write8(0xba000, 0b11_00_00_00);
    let mut out = Vec::new();
    assert_eq!(v.render(&mut out), Some((640, 400)));
    let px = |x: usize, y: usize| out[y * 640 + x];
    assert_eq!(px(0, 0), 0);
    assert_eq!(px(2, 0), 0xff0000);
    assert_eq!(px(4, 1), 0x00ff00);
    assert_eq!(px(7, 0), 0x0000ff);
    assert_eq!(px(0, 2), 0x0000ff);
    assert_eq!(px(1, 3), 0x0000ff);
}

#[test]
fn line_compare_restarts_scanout_at_address_zero() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_12);
    dac(&mut v, 0x3f, [0x3f, 0x3f, 0x3f]);
    v.mem_write8(0xa0000, 0xff);
    crtc(&mut v, 0x0d, 80);
    crtc(&mut v, 0x07, 0x3e & !0x10);
    crtc(&mut v, 0x09, 0x00);
    crtc(&mut v, 0x18, 99);
    let mut out = Vec::new();
    v.render(&mut out);
    assert_eq!(out[0], 0);
    assert_eq!(out[100 * 640], 0xffffff);
}

#[test]
fn blanked_when_palette_source_is_cleared() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_12);
    in8(&mut v, 0x3da);
    out8(&mut v, 0x3c0, 0x00);
    assert_eq!(v.render(&mut Vec::new()), None);
}

#[test]
fn retrace_bits_follow_crtc_timing() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_03);
    let samples: Vec<u8> = (0..20_000u64).map(|i| v.io_read(0x3da, 1, i * 1_000).unwrap() as u8).collect();
    let vretrace = samples.iter().filter(|&&s| s & 8 != 0).count();
    let blank = samples.iter().filter(|&&s| s & 1 != 0).count();
    assert!(vretrace > 0 && vretrace < samples.len() / 20);
    assert!(blank > vretrace && blank < samples.len() / 2);
}

#[test]
fn dispi_id_accepts_known_versions_only() {
    let mut v = Vga::new();
    assert_eq!(vbe_get(&mut v, INDEX_ID), 0xb0c5);
    vbe_set(&mut v, INDEX_ID, 0xb0c0);
    assert_eq!(vbe_get(&mut v, INDEX_ID), 0xb0c0);
    vbe_set(&mut v, INDEX_ID, 0xb0c6);
    assert_eq!(vbe_get(&mut v, INDEX_ID), 0xb0c0);
    assert_eq!(vbe_get(&mut v, INDEX_VIDEO_MEMORY_64K), 256);
}

#[test]
fn dispi_getcaps_reports_limits() {
    let mut v = Vga::new();
    vbe_set(&mut v, INDEX_ENABLE, GETCAPS);
    assert_eq!(vbe_get(&mut v, INDEX_XRES), 16000);
    assert_eq!(vbe_get(&mut v, INDEX_BPP), 32);
    vbe_set(&mut v, INDEX_ENABLE, 0);
    assert_eq!(vbe_get(&mut v, INDEX_BPP), 0);
}

#[test]
fn vbe_32bpp_renders_linear_framebuffer() {
    let mut v = Vga::new();
    v.vram_mut()[..16].fill(0xaa);
    vbe_mode(&mut v, 640, 480, 32, LFB_ENABLED);
    assert_eq!(v.vram()[0], 0);
    let i = (7 * 640 + 3) * 4;
    v.vram_mut()[i..i + 4].copy_from_slice(&[0x11, 0x22, 0x33, 0x00]);
    let mut out = Vec::new();
    assert_eq!(v.render(&mut out), Some((640, 480)));
    assert_eq!(out[7 * 640 + 3], 0x332211);
}

#[test]
fn vbe_16bpp_expands_565() {
    let mut v = Vga::new();
    vbe_mode(&mut v, 800, 600, 16, LFB_ENABLED);
    v.vram_mut()[2..4].copy_from_slice(&0xf800u16.to_le_bytes());
    v.vram_mut()[4..6].copy_from_slice(&0x07e0u16.to_le_bytes());
    let mut out = Vec::new();
    assert_eq!(v.render(&mut out), Some((800, 600)));
    assert_eq!(out[..3], [0, 0xff0000, 0x00ff00]);
}

#[test]
fn vbe_15_and_24bpp_decode_pixels() {
    let mut v = Vga::new();
    vbe_mode(&mut v, 640, 480, 15, 0);
    v.vram_mut()[..2].copy_from_slice(&0x7c1fu16.to_le_bytes());
    let mut out = Vec::new();
    v.render(&mut out);
    assert_eq!(out[0], 0xff00ff);
    vbe_mode(&mut v, 640, 480, 24, 0);
    v.vram_mut()[3..6].copy_from_slice(&[0x01, 0x02, 0x03]);
    v.render(&mut out);
    assert_eq!(out[1], 0x030201);
}

#[test]
fn vbe_virtual_width_and_offsets_move_the_viewport() {
    let mut v = Vga::new();
    vbe_mode(&mut v, 640, 480, 32, LFB_ENABLED);
    vbe_set(&mut v, INDEX_VIRT_WIDTH, 1024);
    vbe_set(&mut v, INDEX_X_OFFSET, 2);
    vbe_set(&mut v, INDEX_Y_OFFSET, 10);
    assert_eq!(vbe_get(&mut v, INDEX_VIRT_HEIGHT), ((16 << 20) / 4096) as u16);
    let i = (10 * 1024 + 2) * 4;
    v.vram_mut()[i..i + 4].copy_from_slice(&[0xff, 0, 0, 0]);
    let mut out = Vec::new();
    v.render(&mut out);
    assert_eq!(out[0], 0x0000ff);
}

#[test]
fn vbe_8bpp_uses_dac_and_8bit_dac_mode() {
    let mut v = Vga::new();
    vbe_mode(&mut v, 640, 480, 8, DAC_8BIT);
    dac(&mut v, 9, [0xff, 0x80, 0x01]);
    v.vram_mut()[0] = 9;
    let mut out = Vec::new();
    v.render(&mut out);
    assert_eq!(out[0], 0xff8001);
    out8(&mut v, 0x3c7, 9);
    assert_eq!(in8(&mut v, 0x3c9), 0xff);
}

#[test]
fn vbe_bank_selects_64k_window_into_vram() {
    let mut v = Vga::new();
    vbe_mode(&mut v, 1024, 768, 8, 0);
    vbe_set(&mut v, INDEX_BANK, 3);
    v.mem_write8(0xa0005, 0x5e);
    assert_eq!(v.vram()[0x3_0005], 0x5e);
    assert_eq!(v.mem_read8(0xa0005), 0x5e);
    vbe_set(&mut v, INDEX_BANK, 0);
    assert_eq!(v.mem_read8(0xa0005), 0);
    assert_eq!(v.mem_read8(0xb0000), 0xff);
}

#[test]
fn vbe_enable_with_noclearmem_keeps_vram() {
    let mut v = Vga::new();
    v.vram_mut()[100] = 7;
    vbe_mode(&mut v, 640, 480, 8, NOCLEARMEM);
    assert_eq!(v.vram()[100], 7);
}

#[test]
fn vbe_4bpp_is_planar() {
    let mut v = Vga::new();
    set_mode(&mut v, &MODE_12);
    dac(&mut v, 0x3f, [0x3f, 0x3f, 0x3f]);
    vbe_mode(&mut v, 1024, 768, 4, 0);
    vbe_set(&mut v, INDEX_BANK, 1);
    v.mem_write8(0xa0000, 0x80);
    assert_eq!(plane_bytes(&v, 0x1_0000), [0x80; 4]);
    let mut out = Vec::new();
    assert_eq!(v.render(&mut out), Some((1024, 768)));
    assert_eq!(out[512 * 1024..512 * 1024 + 2], [0xffffff, 0]);
}

#[test]
#[ignore = "writes a PPM image of mode 3 text to the temp directory"]
fn dump_text_mode_frame() {
    let mut v = text_setup();
    for i in 0..64u8 {
        let level = |hi: u8, lo: u8| (i >> hi & 1) * 0x2a + (i >> lo & 1) * 0x15;
        dac(&mut v, i, [level(2, 5), level(1, 4), level(0, 3)]);
    }
    for i in 0..80 * 25u32 {
        v.mem_write8(0xb8000 + i * 2, if i % 3 == 0 { 0xc4 } else { b'A' });
        v.mem_write8(0xb8001 + i * 2, (i % 256) as u8);
    }
    let mut out = Vec::new();
    let (w, h) = v.render_at(&mut out, 0).unwrap();
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for p in out {
        ppm.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
    }
    let path = std::env::temp_dir().join("vga-mode3.ppm");
    std::fs::write(&path, ppm).unwrap();
    eprintln!("wrote {}", path.display());
}

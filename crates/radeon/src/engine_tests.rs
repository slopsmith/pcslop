use super::*;
use engine::rop3;

const PITCH: usize = 256;
const SOLID: u32 = 13 << 4;
const NO_BRUSH: u32 = 15 << 4;
const SRC_COLOR: u32 = 3 << 12;
const SRC_MONO_FG_BG: u32 = 0;
const SRC_MONO_FG_LA: u32 = 1 << 12;
const LSB_FIRST: u32 = 1 << 14;
const FROM_VRAM: u32 = 2 << 24;
const FROM_HOST: u32 = 3 << 24;

fn engine(datatype: u32) -> (Radeon, u32) {
    let mut r = Radeon::new(vec![0x55, 0xaa]);
    let pitch_offset = (PITCH as u32 / 64) << 22 | 0xf000_0000 >> 10;
    w(&mut r, MC_FB_LOCATION, 0xf7ff_f000);
    w(&mut r, DEFAULT_OFFSET, pitch_offset);
    w(&mut r, DP_CNTL, DST_X_LEFT_TO_RIGHT | DST_Y_TOP_TO_BOTTOM);
    (r, datatype << 8)
}

fn w(r: &mut Radeon, reg: u32, v: u32) {
    r.mmio_write(reg, 4, v, &mut NoBus);
}

fn px(r: &Radeon, bpp: usize, x: usize, y: usize) -> u32 {
    let a = y * PITCH + x * bpp;
    r.vram()[a..a + bpp]
        .iter()
        .rev()
        .fold(0, |v, &b| v << 8 | b as u32)
}

fn set_px(r: &mut Radeon, bpp: usize, x: usize, y: usize, v: u32) {
    let a = y * PITCH + x * bpp;
    r.vram_mut()[a..a + bpp].copy_from_slice(&v.to_le_bytes()[..bpp]);
}

fn fill(r: &mut Radeon, gmc: u32, colour: u32, (x, y, width, height): (u32, u32, u32, u32)) {
    w(r, DP_GUI_MASTER_CNTL, gmc);
    w(r, DP_BRUSH_FRGD_CLR, colour);
    w(r, DST_Y_X, y << 16 | x);
    w(r, DST_WIDTH_HEIGHT, width << 16 | height);
}

fn numbered(r: &mut Radeon, bpp: usize, w: usize, h: usize) {
    let mask = if bpp == 4 {
        u32::MAX
    } else {
        (1 << (8 * bpp)) - 1
    };
    for y in 0..h {
        for x in 0..w {
            set_px(r, bpp, x, y, (y * 64 + x) as u32 & mask);
        }
    }
}

#[test]
fn rop3_codes_combine_pattern_source_and_destination() {
    let (p, s, d) = (0xf0f0_f0f0, 0xcccc_cccc, 0xaaaa_aaaa);
    assert_eq!(rop3(0xcc, p, s, d), s);
    assert_eq!(rop3(0xf0, p, s, d), p);
    assert_eq!(rop3(0x66, p, s, d), s ^ d);
    assert_eq!(rop3(0x5a, p, s, d), p ^ d);
    assert_eq!(rop3(0x88, p, s, d), s & d);
    assert_eq!(rop3(0x00, p, s, d), 0);
    assert_eq!(rop3(0xff, p, s, d), u32::MAX);
}

#[test]
fn solid_fills_cover_their_rectangle_at_each_depth() {
    for (dt, bpp, colour) in [(2, 1, 0x5a), (4, 2, 0xf81f), (6, 4, 0x0012_3456)] {
        let (mut r, dt) = engine(dt);
        fill(
            &mut r,
            dt | SOLID | SRC_COLOR | 0xf0 << 16,
            colour,
            (2, 1, 3, 2),
        );
        let got = [(2, 1), (4, 2), (5, 1), (2, 3), (1, 1)].map(|(x, y)| px(&r, bpp, x, y));
        assert_eq!((bpp, got), (bpp, [colour, colour, 0, 0, 0]));
    }
}

#[test]
fn fills_keep_the_planes_dp_write_mask_protects() {
    let (mut r, dt) = engine(6);
    set_px(&mut r, 4, 0, 0, 0x1122_3344);
    w(&mut r, DP_GUI_MASTER_CNTL, dt | SOLID | 0xf0 << 16);
    w(&mut r, DP_WRITE_MASK, 0x00ff_00ff);
    w(&mut r, DP_BRUSH_FRGD_CLR, 0xaabb_ccdd);
    w(&mut r, DST_Y_X, 0);
    w(&mut r, DST_WIDTH_HEIGHT, 1 << 16 | 1);
    assert_eq!(px(&r, 4, 0, 0), 0x11bb_33dd);
}

#[test]
fn fills_with_a_rop_mix_the_destination() {
    let (mut r, dt) = engine(6);
    set_px(&mut r, 4, 0, 0, 0x0f0f_0f0f);
    fill(&mut r, dt | SOLID | 0x5a << 16, 0x00ff_00ff, (0, 0, 1, 1));
    assert_eq!(px(&r, 4, 0, 0), 0x0ff0_0ff0);
}

#[test]
fn the_scissor_clips_with_exclusive_bottom_right_edges() {
    let (mut r, dt) = engine(6);
    w(
        &mut r,
        DP_GUI_MASTER_CNTL,
        dt | SOLID | GMC_DST_CLIPPING | 0xf0 << 16,
    );
    w(&mut r, SC_TOP_LEFT, 1 << 16 | 1);
    w(&mut r, SC_BOTTOM_RIGHT, 3 << 16 | 3);
    w(&mut r, DP_BRUSH_FRGD_CLR, 9);
    w(&mut r, DST_Y_X, 0);
    w(&mut r, DST_WIDTH_HEIGHT, 5 << 16 | 5);
    let got: Vec<u32> = (0..4)
        .flat_map(|y| (0..4).map(move |x| (x, y)))
        .map(|(x, y)| px(&r, 4, x, y))
        .collect();
    assert_eq!(got, [0, 0, 0, 0, 0, 9, 9, 0, 0, 9, 9, 0, 0, 0, 0, 0]);
}

#[test]
fn writing_gui_master_without_dst_clipping_restores_the_default_scissor() {
    let (mut r, dt) = engine(6);
    w(&mut r, SC_BOTTOM_RIGHT, 1 << 16 | 1);
    fill(&mut r, dt | SOLID | 0xf0 << 16, 9, (2, 2, 1, 1));
    assert_eq!(px(&r, 4, 2, 2), 9);
}

fn reference_blit(
    r: &Radeon,
    bpp: usize,
    (sx, sy): (usize, usize),
    (dx, dy): (usize, usize),
    (w, h): (usize, usize),
) -> Vec<u8> {
    let mut v = r.vram()[..PITCH * 64].to_vec();
    let saved = v.clone();
    for y in 0..h {
        for x in 0..w {
            let s = (sy + y) * PITCH + (sx + x) * bpp;
            let d = (dy + y) * PITCH + (dx + x) * bpp;
            v[d..d + bpp].copy_from_slice(&saved[s..s + bpp]);
        }
    }
    v
}

fn copy(r: &mut Radeon, gmc: u32, ctl: u32, src: (u32, u32), dst: (u32, u32), size: (u32, u32)) {
    w(r, DP_GUI_MASTER_CNTL, gmc);
    w(r, DP_CNTL, ctl);
    w(r, SRC_Y_X, src.1 << 16 | src.0);
    w(r, DST_Y_X, dst.1 << 16 | dst.0);
    w(r, DST_HEIGHT_WIDTH, size.1 << 16 | size.0);
}

#[test]
fn overlapping_copies_move_pixels_whole_in_every_direction() {
    for (src, dst) in [
        ((2, 2), (5, 4)),
        ((5, 4), (2, 2)),
        ((2, 4), (6, 1)),
        ((6, 1), (2, 4)),
    ] {
        let (mut r, dt) = engine(4);
        numbered(&mut r, 2, 32, 32);
        let want = reference_blit(&r, 2, src, dst, (10, 8));
        let (s, d) = ((src.0 as u32, src.1 as u32), (dst.0 as u32, dst.1 as u32));
        copy(
            &mut r,
            dt | NO_BRUSH | SRC_COLOR | FROM_VRAM | 0xcc << 16,
            3,
            s,
            d,
            (10, 8),
        );
        assert_eq!(
            (src, dst, r.vram()[..PITCH * 64] == want[..]),
            (src, dst, true)
        );
    }
}

#[test]
fn right_to_left_copies_name_the_far_corner() {
    let (mut r, dt) = engine(6);
    numbered(&mut r, 4, 16, 16);
    let want = reference_blit(&r, 4, (1, 1), (3, 3), (4, 4));
    copy(
        &mut r,
        dt | NO_BRUSH | SRC_COLOR | FROM_VRAM | 0xcc << 16,
        0,
        (4, 4),
        (6, 6),
        (4, 4),
    );
    assert!(r.vram()[..PITCH * 64] == want[..]);
}

#[test]
fn overlapping_copies_with_a_rop_read_the_source_before_writing() {
    let (mut r, dt) = engine(6);
    numbered(&mut r, 4, 16, 16);
    let before: Vec<u32> = (0..6).map(|x| px(&r, 4, x, 0)).collect();
    copy(
        &mut r,
        dt | NO_BRUSH | SRC_COLOR | FROM_VRAM | 0x66 << 16,
        3,
        (0, 0),
        (1, 0),
        (4, 1),
    );
    let got: Vec<u32> = (1..5).map(|x| px(&r, 4, x, 0)).collect();
    let want: Vec<u32> = (1..5).map(|x| before[x] ^ before[x - 1]).collect();
    assert_eq!(got, want);
}

#[test]
fn copies_skip_pixels_matching_the_transparent_colour() {
    let (mut r, dt) = engine(6);
    set_px(&mut r, 4, 0, 0, 7);
    set_px(&mut r, 4, 1, 0, 8);
    w(&mut r, CLR_CMP_CLR_SRC, 7);
    w(&mut r, CLR_CMP_MASK, u32::MAX);
    w(&mut r, CLR_CMP_CNTL, 4 | 1 << 24);
    copy(
        &mut r,
        dt | NO_BRUSH | SRC_COLOR | FROM_VRAM | 0xcc << 16,
        3,
        (0, 0),
        (0, 1),
        (2, 1),
    );
    assert_eq!([px(&r, 4, 0, 1), px(&r, 4, 1, 1)], [0, 8]);
    copy(
        &mut r,
        dt | NO_BRUSH | SRC_COLOR | FROM_VRAM | GMC_CLR_CMP_CNTL_DIS | 0xcc << 16,
        3,
        (0, 0),
        (0, 2),
        (2, 1),
    );
    assert_eq!([px(&r, 4, 0, 2), px(&r, 4, 1, 2)], [7, 8]);
}

fn host_blit(r: &mut Radeon, gmc: u32, (x, y, width, height): (u32, u32, u32, u32), data: &[u32]) {
    w(r, DP_GUI_MASTER_CNTL, gmc);
    w(r, DP_SRC_FRGD_CLR, 0xff);
    w(r, DP_SRC_BKGD_CLR, 0x11);
    w(r, DST_Y_X, y << 16 | x);
    w(r, DST_HEIGHT_WIDTH, height << 16 | width);
    for (i, &d) in data.iter().enumerate() {
        let reg = if i + 1 == data.len() {
            HOST_DATA_LAST
        } else {
            HOST_DATA0 + 4 * (i as u32 % 8)
        };
        w(r, reg, d);
    }
}

#[test]
fn monochrome_host_data_expands_to_both_colours() {
    let (mut r, dt) = engine(6);
    host_blit(
        &mut r,
        dt | NO_BRUSH | SRC_MONO_FG_BG | LSB_FIRST | FROM_HOST | 0xcc << 16,
        (0, 0, 32, 2),
        &[0b0110, 1 << 31],
    );
    assert_eq!(
        [0, 1, 2, 3].map(|x| px(&r, 4, x, 0)),
        [0x11, 0xff, 0xff, 0x11]
    );
    assert_eq!([30, 31].map(|x| px(&r, 4, x, 1)), [0x11, 0xff]);
}

#[test]
fn transparent_monochrome_host_data_leaves_background_pixels() {
    let (mut r, dt) = engine(6);
    set_px(&mut r, 4, 0, 0, 0x42);
    host_blit(
        &mut r,
        dt | NO_BRUSH | SRC_MONO_FG_LA | LSB_FIRST | FROM_HOST | 0xcc << 16,
        (0, 0, 32, 1),
        &[0b10],
    );
    assert_eq!([px(&r, 4, 0, 0), px(&r, 4, 1, 0)], [0x42, 0xff]);
}

#[test]
fn msb_first_host_data_takes_the_top_bit_of_each_byte_first() {
    let (mut r, dt) = engine(6);
    host_blit(
        &mut r,
        dt | NO_BRUSH | SRC_MONO_FG_BG | FROM_HOST | 0xcc << 16,
        (0, 0, 32, 1),
        &[0x0000_0180],
    );
    assert_eq!(
        [0, 1, 7, 8, 15].map(|x| px(&r, 4, x, 0)),
        [0xff, 0x11, 0x11, 0x11, 0xff]
    );
}

#[test]
fn colour_host_data_writes_an_image() {
    let (mut r, dt) = engine(4);
    host_blit(
        &mut r,
        dt | NO_BRUSH | SRC_COLOR | FROM_HOST | 0xcc << 16,
        (1, 1, 2, 2),
        &[0x2222_1111, 0x4444_3333],
    );
    assert_eq!(
        [(1, 1), (2, 1), (1, 2), (2, 2)].map(|(x, y)| px(&r, 2, x, y)),
        [0x1111, 0x2222, 0x3333, 0x4444]
    );
}

#[test]
fn mono_patterns_repeat_every_eight_pixels() {
    let (mut r, dt) = engine(2);
    w(&mut r, DP_GUI_MASTER_CNTL, dt | SRC_COLOR | 0xf0 << 16);
    w(&mut r, DP_BRUSH_FRGD_CLR, 1);
    w(&mut r, DP_BRUSH_BKGD_CLR, 2);
    w(&mut r, BRUSH_DATA0, 0x0000_0081);
    w(&mut r, BRUSH_DATA0 + 4, 0);
    w(&mut r, DST_Y_X, 0);
    w(&mut r, DST_WIDTH_HEIGHT, 16 << 16 | 9);
    assert_eq!([0, 1, 7, 8, 15].map(|x| px(&r, 1, x, 0)), [1, 2, 1, 1, 1]);
    assert_eq!([px(&r, 1, 0, 1), px(&r, 1, 0, 8)], [2, 1]);
}

#[test]
fn lines_stop_short_of_their_last_pixel() {
    let (mut r, dt) = engine(6);
    w(&mut r, DP_GUI_MASTER_CNTL, dt | SOLID | 0xf0 << 16);
    w(&mut r, DP_BRUSH_FRGD_CLR, 5);
    w(&mut r, DST_LINE_START, 1 << 16 | 1);
    w(&mut r, DST_LINE_END, 1 << 16 | 4);
    assert_eq!([0, 1, 2, 3, 4].map(|x| px(&r, 4, x, 1)), [0, 5, 5, 5, 0]);
}

#[test]
fn blits_land_at_dst_pitch_offset() {
    let (mut r, dt) = engine(6);
    w(
        &mut r,
        DST_PITCH_OFFSET,
        (512 / 64) << 22 | (0xf000_0000u32 + 0x1_0000) >> 10,
    );
    fill(
        &mut r,
        dt | SOLID | GMC_DST_PITCH_OFFSET_CNTL | 0xf0 << 16,
        3,
        (1, 1, 1, 1),
    );
    assert_eq!(
        u32::from_le_bytes(r.vram()[0x1_0000 + 512 + 4..][..4].try_into().unwrap()),
        3
    );
}

#[test]
fn destination_compare_keeps_pixels_of_the_reference_colour() {
    let (mut r, dt) = engine(6);
    set_px(&mut r, 4, 0, 1, 5);
    set_px(&mut r, 4, 0, 0, 9);
    set_px(&mut r, 4, 1, 0, 9);
    w(&mut r, CLR_CMP_CLR_DST, 5);
    w(&mut r, CLR_CMP_CNTL, 4 << 8);
    copy(&mut r, dt | NO_BRUSH | SRC_COLOR | FROM_VRAM | 0xcc << 16, 3, (0, 0), (0, 1), (2, 1));
    assert_eq!([px(&r, 4, 0, 1), px(&r, 4, 1, 1)], [5, 9]);
}

use super::regs::*;
use crate::Radeon;
use crate::mem::BusMemory;
use crate::regs::*;

struct Ram(Vec<u8>);

impl BusMemory for Ram {
    fn read(&mut self, addr: u32, buf: &mut [u8]) {
        let a = addr as usize % self.0.len();
        buf.copy_from_slice(&self.0[a..a + buf.len()]);
    }

    fn write(&mut self, addr: u32, buf: &[u8]) {
        let a = addr as usize % self.0.len();
        self.0[a..a + buf.len()].copy_from_slice(buf);
    }
}

const VRAM: u32 = 0xf000_0000;
const COLOUR: u32 = 0x1_0000;
const DEPTH: u32 = 0x2_0000;
const TEXTURE: u32 = 0x3_0000;
const PITCH: usize = 64;
/// Where the scissor and clip rectangles put pixel (0, 0) with 1/16 subpixels.
const SC: u32 = 1088;

fn pkt3(opcode: u8, count: usize) -> u32 {
    0xc000_0000 | (count as u32 - 1) << 16 | (opcode as u32) << 8
}

fn f(v: f32) -> u32 {
    v.to_bits()
}

/// A vector PVS instruction writing all of register `dst` of `class`.
fn pvs(op: u32, class: u32, dst: u32, srcs: [u32; 3]) -> [u32; 4] {
    [op | class << 8 | dst << 13 | 0xf << 20, srcs[0], srcs[1], srcs[2]]
}

/// A PVS source: register `index` of `class` (0 temporary, 1 input, 2 constant).
fn src(class: u32, index: u32) -> u32 {
    class | index << 5 | 1 << 16 | 2 << 19 | 3 << 22
}

const ZERO: u32 = 1 | 4 << 13 | 4 << 16 | 4 << 19 | 4 << 22;

/// RGB and alpha both output the first temporary, which the RS fills.
const PASS_COLOUR: [u32; 4] = [7 << 26, 21 << 7 | 20 << 14, 1 << 24, 9 | 17 << 7 | 16 << 14];

struct Gpu {
    r: Radeon,
    ram: Ram,
}

impl Gpu {
    fn new() -> Self {
        let mut g = Gpu { r: Radeon::new(vec![0x55, 0xaa]), ram: Ram(vec![0; 1 << 20]) };
        g.w(MC_FB_LOCATION, 0xf7ff_f000);
        g.w(MC_AGP_LOCATION, 0xffff_fff0);
        g.w(GB_TILE_CONFIG, 0x10017);
        g.w(SC_SCISSOR0, SC | SC << 13);
        g.w(SC_SCISSOR1, (SC + 63) | (SC + 63) << 13);
        g.w(SC_CLIP_RULE, 0xffff);
        g.w(RB3D_COLOROFFSET0, VRAM + COLOUR);
        g.w(RB3D_COLORPITCH0, PITCH as u32 | 6 << 21);
        g.w(RB3D_COLOR_CHANNEL_MASK, 0xf);
        g.w(VAP_VTE_CNTL, 0x300);
        g.w(VAP_PROG_STREAM_CNTL_0, 3 | 3 << 16 | 1 << 24 | 1 << 29);
        g.w(VAP_PROG_STREAM_CNTL_EXT_0, 0xf688 | 0xf688 << 16);
        g.w(VAP_VTX_SIZE, 8);
        g.w(VAP_OUT_VTX_FMT_0, 3);
        g.vertex_program(&[pvs(3, 2, 0, [src(1, 0), ZERO, ZERO]), pvs(3, 2, 1, [src(1, 1), ZERO, ZERO])]);
        g.w(GA_COLOR_CONTROL, 0xaaaa);
        g.w(RS_INST_COUNT, 0);
        g.w(RS_IP_0, 0);
        g.w(RS_INST_0, 1 << 14);
        g.w(US_OUT_FMT_0, 0x1b00);
        g.fragment(&[PASS_COLOUR], 0);
        g
    }

    fn w(&mut self, reg: u32, v: u32) {
        self.r.mmio_write(reg, 4, v, &mut self.ram);
    }

    fn vertex_program(&mut self, code: &[[u32; 4]]) {
        self.w(VAP_PVS_VECTOR_INDX_REG, 0);
        for d in code.iter().flatten() {
            self.w(VAP_PVS_VECTOR_DATA_REG, *d);
        }
        self.w(VAP_PVS_CODE_CNTL_0, (code.len() as u32 - 1) << 20);
    }

    fn constant(&mut self, index: u32, v: [f32; 4]) {
        self.w(VAP_PVS_VECTOR_INDX_REG, 512 + index);
        for c in v {
            self.w(VAP_PVS_VECTOR_DATA_REG, f(c));
        }
    }

    fn fragment(&mut self, alu: &[[u32; 4]], tex: usize) {
        for (i, a) in alu.iter().enumerate() {
            let i = 4 * i as u32;
            self.w(US_ALU_RGB_ADDR_0 + i, a[0]);
            self.w(US_ALU_RGB_INST_0 + i, a[1]);
            self.w(US_ALU_ALPHA_ADDR_0 + i, a[2]);
            self.w(US_ALU_ALPHA_INST_0 + i, a[3]);
        }
        let tex_size = tex.max(1) as u32 - 1;
        self.w(US_CONFIG, if tex > 0 { 1 << 3 } else { 0 });
        self.w(US_CODE_ADDR_0 + 12, (alu.len() as u32 - 1) << 6 | tex_size << 17 | 1 << 22);
    }

    fn draw(&mut self, prim: u32, verts: &[[f32; 8]]) {
        let mut p = vec![pkt3(0x35, 1 + 8 * verts.len()), prim | 3 << 4 | (verts.len() as u32) << 16];
        p.extend(verts.iter().flatten().map(|&v| f(v)));
        self.r.execute(&mut self.ram, &p);
    }

    fn put(&mut self, at: usize, v: u32) {
        self.r.vram_mut()[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn px(&self, x: usize, y: usize) -> u32 {
        let a = COLOUR as usize + (y * PITCH + x) * 4;
        u32::from_le_bytes(self.r.vram()[a..a + 4].try_into().unwrap())
    }

    fn count(&self, colour: u32) -> usize {
        (0..64).flat_map(|y| (0..64).map(move |x| (x, y))).filter(|&(x, y)| self.px(x, y) == colour).count()
    }
}

fn v(x: f32, y: f32, c: [f32; 4]) -> [f32; 8] {
    [x, y, 0.5, 1.0, c[0], c[1], c[2], c[3]]
}

fn quad(x0: f32, y0: f32, x1: f32, y1: f32, c: [f32; 4]) -> [[f32; 8]; 4] {
    [v(x0, y0, c), v(x1, y0, c), v(x1, y1, c), v(x0, y1, c)]
}

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

#[test]
fn a_quad_fills_exactly_the_pixels_whose_centres_it_covers() {
    let mut g = Gpu::new();
    g.draw(13, &quad(2.0, 3.0, 10.0, 7.0, RED));
    assert_eq!(g.count(0xffff_0000), 8 * 4);
    assert_eq!(g.px(2, 3), 0xffff_0000);
    assert_eq!(g.px(9, 6), 0xffff_0000);
    assert_eq!(g.px(10, 6), 0);
    assert_eq!(g.px(9, 7), 0);
}

#[test]
fn triangles_sharing_an_edge_touch_each_pixel_once() {
    let mut g = Gpu::new();
    g.w(RB3D_BLENDCNTL, 1 | 33 << 16 | 33 << 24);
    let c = [0.25, 0.0, 0.0, 0.0];
    let (a, b, d, e) = (v(0.0, 0.0, c), v(37.3, 5.1, c), v(11.7, 40.9, c), v(50.2, 44.4, c));
    g.draw(4, &[a, b, d, b, e, d]);
    assert!(g.count(0x0040_0000) > 500);
    assert_eq!(g.count(0x0080_0000), 0);
}

#[test]
fn colours_are_interpolated_across_a_triangle() {
    let mut g = Gpu::new();
    g.draw(4, &[v(0.0, 0.0, RED), v(64.0, 0.0, BLUE), v(0.0, 64.0, RED)]);
    let (left, right) = (g.px(0, 10), g.px(50, 10));
    assert!(left >> 16 & 0xff > 0xf0 && left & 0xff < 0x10);
    assert!(right & 0xff > 0xc0 && right >> 16 & 0xff < 0x40);
}

#[test]
fn the_vertex_program_transforms_positions_with_its_constants() {
    let mut g = Gpu::new();
    g.constant(0, [2.0, 2.0, 1.0, 1.0]);
    g.constant(1, [16.0, 8.0, 0.0, 0.0]);
    g.vertex_program(&[pvs(4, 2, 0, [src(1, 0), src(2, 0), src(2, 1)]), pvs(3, 2, 1, [src(1, 1), ZERO, ZERO])]);
    g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
    assert_eq!(g.count(0xffff_0000), 64);
    assert_eq!(g.px(16, 8), 0xffff_0000);
    assert_eq!(g.px(23, 15), 0xffff_0000);
    assert_eq!(g.px(24, 15), 0);
}

#[test]
fn front_and_back_faces_are_culled_only_when_asked() {
    let mut g = Gpu::new();
    let tri = [v(0.0, 0.0, RED), v(0.0, 20.0, RED), v(20.0, 0.0, RED)];
    g.w(SU_CULL_MODE, 1);
    g.draw(4, &tri);
    assert_eq!(g.count(0xffff_0000), 0);
    g.w(SU_CULL_MODE, 2);
    g.draw(4, &tri);
    assert!(g.count(0xffff_0000) > 100);
}

#[test]
fn the_depth_test_keeps_the_nearer_surface_whatever_the_order() {
    let mut g = Gpu::new();
    g.w(ZB_CNTL, 6);
    g.w(ZB_ZSTENCILCNTL, 1);
    g.w(ZB_FORMAT, 2);
    g.w(ZB_DEPTHOFFSET, VRAM + DEPTH);
    g.w(ZB_DEPTHPITCH, PITCH as u32);
    g.w(SU_DEPTH_SCALE, f(16_777_215.0));
    for i in 0..PITCH * 64 {
        g.put(DEPTH as usize + 4 * i, 0xffff_ff00);
    }
    let at = |z: f32, c: [f32; 4]| {
        quad(0.0, 0.0, 16.0, 16.0, c).map(|mut v| {
            v[2] = z;
            v
        })
    };
    g.draw(13, &at(0.25, RED));
    g.draw(13, &at(0.75, BLUE));
    assert_eq!(g.count(0xffff_0000), 256);
}

#[test]
fn the_clip_rule_limits_drawing_to_the_inclusive_clip_rectangle() {
    let mut g = Gpu::new();
    g.w(SC_CLIP_0_A, (SC + 4) | (SC + 4) << 13);
    g.w(SC_CLIP_0_A + 4, (SC + 7) | (SC + 5) << 13);
    g.w(SC_CLIP_RULE, 0xaaaa);
    g.draw(13, &quad(0.0, 0.0, 64.0, 64.0, RED));
    assert_eq!(g.count(0xffff_0000), 4 * 2);
    assert_eq!(g.px(4, 4), 0xffff_0000);
    assert_eq!(g.px(7, 5), 0xffff_0000);
}

/// XP's AlphaBlend: a textured point sprite with a premultiplied blend.
#[test]
fn a_textured_point_sprite_blends_like_xps_alphablend() {
    let mut g = Gpu::new();
    for y in 0..4 {
        for x in 0..4 {
            let texel = if x < 2 { 0x8000_0080 } else { 0xff00_ff00 };
            g.put(TEXTURE as usize + y * 32 + x * 4, texel);
        }
    }
    for i in 0..PITCH * 64 {
        g.put(COLOUR as usize + 4 * i, 0xff80_0000);
    }
    g.w(GB_ENABLE, 1 | 1 << 16);
    g.w(GA_POINT_SIZE, 32 << 16 | 32);
    for (reg, val) in [(GA_POINT_S0, 0.0), (GA_POINT_T0, 1.0), (GA_POINT_S1, 1.0), (GA_POINT_T1, 0.0)] {
        g.w(reg, f(val));
    }
    g.w(VAP_OUT_VTX_FMT_0, 1);
    g.w(RS_IP_0, 1 << 16 | 4 << 19 | 5 << 22);
    g.w(RS_INST_0, 1 << 3);
    g.w(TX_FILTER0_0, 2 | 2 << 3 | 1 << 9 | 1 << 11);
    g.w(TX_FORMAT0_0, 3 | 3 << 11);
    g.w(TX_FORMAT1_0, 0xa60c);
    g.w(TX_OFFSET_0, VRAM + TEXTURE);
    g.w(TX_ENABLE, 1);
    g.w(US_TEX_INST_0, 1 << 15);
    g.fragment(&[PASS_COLOUR], 1);
    g.w(RB3D_BLENDCNTL, 0x2721_0005);
    g.draw(1, &[v(4.0, 4.0, RED)]);
    assert_eq!(g.px(2, 2), 0xff40_0080);
    assert_eq!(g.px(4, 5), 0xff00_ff00);
    assert_eq!(g.px(6, 2), 0xff80_0000);
}

#[test]
fn shader_constants_decode_from_s16e7() {
    use super::us::s16e7;
    assert_eq!(s16e7(0x3f_0000), 1.0);
    assert_eq!(s16e7(0x3e_8000), 0.75);
    assert_eq!(s16e7(0xbf_0000), -1.0);
    assert_eq!(s16e7(0), 0.0);
}

#[test]
fn the_fragment_cmp_selects_by_the_sign_of_its_third_operand() {
    let mut g = Gpu::new();
    let rgb_inst = 21 | 20 << 7 | 1 << 14 | 8 << 23;
    g.fragment(&[[32 | 7 << 26, rgb_inst, 1 << 24, 17 | 17 << 7 | 16 << 14]], 0);
    g.w(US_ALU_CONST_R_0, 0xbf_0000);
    g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
    assert_eq!(g.px(1, 1), 0xff00_0000);
    g.w(US_ALU_CONST_R_0, 0x3f_0000);
    g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
    assert_eq!(g.px(1, 1), 0xffff_ffff);
}

#[test]
fn indexed_draws_fetch_vertices_from_arrays_of_structures() {
    let mut g = Gpu::new();
    let base = 0x8000;
    for (i, d) in quad(0.0, 0.0, 8.0, 8.0, BLUE).iter().flatten().enumerate() {
        g.ram.0[base + 4 * i..][..4].copy_from_slice(&f(*d).to_le_bytes());
    }
    let p = [
        pkt3(0x2f, 3),
        1,
        8 | 8 << 8,
        base as u32,
        pkt3(0x36, 4),
        4 | 1 << 4 | 6 << 16,
        1 << 16,
        2,
        2 | 3 << 16,
    ];
    g.r.execute(&mut g.ram, &p);
    assert_eq!(g.count(0xff00_00ff), 64);
}

#[test]
fn an_index_buffer_may_start_two_bytes_into_a_dword() {
    let mut g = Gpu::new();
    let (verts, indices) = (0x8000, 0x9000);
    for (i, d) in quad(0.0, 0.0, 8.0, 8.0, BLUE).iter().flatten().enumerate() {
        g.ram.0[verts + 4 * i..][..4].copy_from_slice(&f(*d).to_le_bytes());
    }
    for (i, x) in [3u16, 0, 1, 2, 0, 2, 3, 0].iter().enumerate() {
        g.ram.0[indices + 2 * i..][..2].copy_from_slice(&x.to_le_bytes());
    }
    let p = [
        pkt3(0x2f, 3),
        1,
        8 | 8 << 8,
        verts as u32,
        pkt3(0x36, 1),
        4 | 1 << 4 | 6 << 16,
        pkt3(0x33, 3),
        1 << 31 | VAP_PVS_VECTOR_DATA_REG >> 2,
        indices as u32 + 2,
        4,
    ];
    g.r.execute(&mut g.ram, &p);
    assert_eq!(g.count(0xff00_00ff), 64);
}

#[test]
fn blits_reach_system_memory_through_the_pci_gart() {
    let mut g = Gpu::new();
    let (table, page) = (0x1000u32, 0x5_0000u32);
    g.ram.0[table as usize..][..4].copy_from_slice(&page.to_le_bytes());
    g.w(AIC_PT_BASE, table);
    g.w(AIC_LO_ADDR, 0xfc00_0000);
    g.w(AIC_HI_ADDR, 0xfc0f_ffff);
    g.w(AIC_CNTL, PCIGART_TRANSLATE_EN);
    g.put(COLOUR as usize, 0x1234_5678);
    let src = 1 << 22 | (VRAM + COLOUR) >> 10;
    let dst = 1 << 22 | 0xfc00_0000 >> 10;
    g.r.execute(&mut g.ram, &[pkt3(0x9b, 6), 0x52cc_36f3, src, dst, 0, 0, 1 << 16 | 1]);
    assert_eq!(u32::from_le_bytes(g.ram.0[page as usize..][..4].try_into().unwrap()), 0x1234_5678);
}

#[test]
fn the_viewport_takes_effect_through_either_register_address() {
    let mut g = Gpu::new();
    g.w(VAP_VTE_CNTL, 0x300 | 0xf);
    for (reg, val) in [(0x1d98, 2.0), (0x1d9c, 8.0), (0x1da0, 1.0), (0x1da4, 4.0)] {
        g.w(reg, f(val));
    }
    g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
    assert_eq!(g.count(0xffff_0000), 32);
    assert_eq!(g.px(8, 4), 0xffff_0000);
}

#[test]
fn triangles_crossing_the_eye_plane_draw_only_their_visible_part() {
    let mut g = Gpu::new();
    g.w(VAP_VTE_CNTL, 0xf | 1 << 10);
    for (reg, val) in [(VAP_VPORT_XSCALE, 32.0), (VAP_VPORT_XOFFSET, 32.0), (VAP_VPORT_YSCALE, 32.0), (VAP_VPORT_YOFFSET, 32.0)] {
        g.w(reg, f(val));
    }
    let at = |x: f32, y: f32, w: f32| [x, y, 0.0, w, 1.0, 0.0, 0.0, 1.0];
    g.draw(4, &[at(-0.5, -0.5, 1.0), at(0.5, -0.5, 1.0), at(0.0, 1.5, -1.0)]);
    assert_eq!(g.px(32, 30), 0xffff_0000);
    assert_eq!(g.px(32, 10), 0);
}

fn clip_space(g: &mut Gpu) {
    g.w(VAP_VTE_CNTL, 0xf | 1 << 10);
    for (reg, val) in [(VAP_VPORT_XSCALE, 32.0), (VAP_VPORT_XOFFSET, 32.0), (VAP_VPORT_YSCALE, 32.0), (VAP_VPORT_YOFFSET, 32.0)] {
        g.w(reg, f(val));
    }
}

/// Direct3D's near plane is z = 0: a vertex just in front of the eye but
/// behind it must not stretch its triangle across the screen.
#[test]
fn triangles_are_clipped_to_the_direct3d_near_plane() {
    let mut g = Gpu::new();
    clip_space(&mut g);
    g.w(VAP_CNTL, DX_CLIP_SPACE_DEF);
    let at = |x: f32, y: f32, z: f32| [x, y, z, 1.0, 1.0, 0.0, 0.0, 1.0];
    g.draw(4, &[at(-1.0, -1.0, 0.5), at(1.0, -1.0, 0.5), at(0.0, 1.0, -0.5)]);
    assert_eq!(g.px(32, 20), 0xffff_0000);
    assert_eq!(g.px(32, 40), 0);
    g.w(VAP_CLIP_CNTL, CLIP_DISABLE);
    g.draw(4, &[at(-1.0, -1.0, 0.5), at(1.0, -1.0, 0.5), at(0.0, 1.0, -0.5)]);
    assert_eq!(g.px(32, 40), 0xffff_0000);
}

#[test]
fn opengl_clip_space_keeps_z_down_to_minus_w_and_clips_beyond_the_far_plane() {
    let mut g = Gpu::new();
    clip_space(&mut g);
    let at = |x: f32, y: f32, z: f32| [x, y, z, 1.0, 1.0, 0.0, 0.0, 1.0];
    g.draw(4, &[at(-1.0, -1.0, -0.5), at(1.0, -1.0, -0.5), at(0.0, 1.0, -0.9)]);
    assert_eq!(g.px(32, 40), 0xffff_0000);
    g.draw(4, &[at(-1.0, -1.0, 0.5), at(1.0, -1.0, 0.5), at(0.0, 1.0, 1.5)].map(|mut v| {
        v[6] = 1.0;
        v
    }));
    assert_eq!(g.px(32, 20), 0xffff_00ff);
    assert_eq!(g.px(32, 40), 0xffff_0000);
}

/// A vertex far outside the guard band is clipped before the divide, so
/// what is left of the triangle still covers the screen it should.
#[test]
fn the_guard_band_clips_vertices_far_off_screen() {
    let mut g = Gpu::new();
    clip_space(&mut g);
    g.w(VAP_GB_HORZ_CLIP_ADJ, f(2.0));
    g.w(VAP_GB_VERT_CLIP_ADJ, f(2.0));
    let at = |x: f32, y: f32| [x, y, 0.5, 1.0, 1.0, 0.0, 0.0, 1.0];
    g.draw(4, &[at(-1.0, -1.0), at(1.0e12, -1.0), at(-1.0, 1.0e12)]);
    assert_eq!(g.count(0xffff_0000), 64 * 64);
}

fn textured_sprite(g: &mut Gpu, unit: u32, logical_id: u32) {
    for i in 0..16 {
        g.put(TEXTURE as usize + i * 4, 0xff00_ff00);
    }
    g.w(GB_ENABLE, 1 | 1 << 16);
    g.w(GA_POINT_SIZE, 32 << 16 | 32);
    g.w(VAP_OUT_VTX_FMT_0, 1);
    g.w(RS_IP_0, 1 << 16 | 4 << 19 | 5 << 22);
    g.w(RS_INST_0, 1 << 3);
    let u = 4 * unit;
    g.w(TX_FILTER0_0 + u, logical_id << 28);
    g.w(TX_FORMAT0_0 + u, 3 | 3 << 11);
    g.w(TX_FORMAT1_0 + u, 0xa60c);
    g.w(TX_OFFSET_0 + u, VRAM + TEXTURE);
    g.w(TX_ENABLE, 1 << unit);
    g.w(US_TEX_INST_0, 1 << 15);
    g.fragment(&[PASS_COLOUR], 1);
    g.draw(1, &[v(4.0, 4.0, RED)]);
}

#[test]
fn the_fragment_program_finds_a_texture_map_by_its_logical_id() {
    let mut g = Gpu::new();
    textured_sprite(&mut g, 3, 0);
    assert_eq!(g.px(4, 4), 0xff00_ff00);
}

#[test]
fn a_texture_map_with_another_logical_id_reads_as_disabled() {
    let mut g = Gpu::new();
    textured_sprite(&mut g, 0, 3);
    assert_eq!(g.px(4, 4), 0xff00_0000);
}

/// Linear fog by eye distance, as Sniper: Path of Vengeance sets it up on
/// Catalyst: half the fog colour at w = 2.
fn fogged(g: &mut Gpu, function: u32) {
    g.w(GB_SELECT, 4);
    g.w(GA_FOG_SCALE, f(0.25));
    g.w(GA_FOG_OFFSET, f(0.0));
    g.w(FG_FOG_BLEND, 1 | function << 1);
    for (c, v) in [0, 0, 1023].into_iter().enumerate() {
        g.w(FG_FOG_COLOR_R + 4 * c as u32, v);
    }
    let near = quad(0.0, 0.0, 16.0, 16.0, RED);
    let far = quad(16.0, 0.0, 32.0, 16.0, RED).map(|mut v| {
        v[3] = 0.5;
        v
    });
    g.draw(13, &near);
    g.draw(13, &far);
}

#[test]
fn linear_fog_blends_towards_the_fog_colour_with_eye_distance() {
    let mut g = Gpu::new();
    fogged(&mut g, 0);
    assert_eq!(g.px(4, 4), 0xffbf_0040);
    assert_eq!(g.px(20, 4), 0xff80_0080);
    g.w(FG_FOG_BLEND, 0);
    g.draw(13, &quad(0.0, 0.0, 16.0, 16.0, RED));
    assert_eq!(g.px(4, 4), 0xffff_0000);
}

#[test]
fn indx_buffer_streams_a_vertex_program_into_vector_memory() {
    let mut g = Gpu::new();
    g.constant(0, [2.0, 2.0, 1.0, 1.0]);
    g.constant(1, [16.0, 8.0, 0.0, 0.0]);
    let code = [pvs(4, 2, 0, [src(1, 0), src(2, 0), src(2, 1)]), pvs(3, 2, 1, [src(1, 1), ZERO, ZERO])];
    let base = 0x8000;
    for (i, d) in code.iter().flatten().enumerate() {
        g.ram.0[base + 4 * i..][..4].copy_from_slice(&d.to_le_bytes());
    }
    g.w(VAP_PVS_VECTOR_INDX_REG, 0);
    let one_reg = 1 << 31 | VAP_PVS_VECTOR_DATA_REG >> 2;
    g.r.execute(&mut g.ram, &[pkt3(0x33, 3), one_reg, base as u32, 8]);
    g.w(VAP_PVS_CODE_CNTL_0, 1 << 20);
    g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
    assert_eq!(g.count(0xffff_0000), 64);
    assert_eq!(g.px(16, 8), 0xffff_0000);
}

/// The transform from the test above, uploaded as Catalyst uploads a program
/// that runs past the last code slot: its first instruction in slot 255 and
/// its second in slot 0.
fn wrapped_program(g: &mut Gpu) {
    g.constant(0, [2.0, 2.0, 1.0, 1.0]);
    g.constant(1, [16.0, 8.0, 0.0, 0.0]);
    let code = [pvs(4, 2, 0, [src(1, 0), src(2, 0), src(2, 1)]), pvs(3, 2, 1, [src(1, 1), ZERO, ZERO])];
    for (slot, inst) in [(255, code[0]), (0, code[1])] {
        g.w(VAP_PVS_VECTOR_INDX_REG, slot);
        for d in inst {
            g.w(VAP_PVS_VECTOR_DATA_REG, d);
        }
    }
    g.w(VAP_PVS_CODE_CNTL_0, 255);
    g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
}

#[test]
fn a_vertex_program_wraps_from_the_last_code_slot_to_the_first() {
    let mut g = Gpu::new();
    wrapped_program(&mut g);
    assert_eq!(g.count(0xffff_0000), 64);
    assert_eq!(g.px(16, 8), 0xffff_0000);
    assert_eq!(g.px(23, 15), 0xffff_0000);
}

#[test]
fn draw_128_takes_indices_when_its_walk_says_so() {
    let mut g = Gpu::new();
    let base = 0x8000;
    for (i, d) in quad(0.0, 0.0, 8.0, 8.0, BLUE).iter().flatten().enumerate() {
        g.ram.0[base + 4 * i..][..4].copy_from_slice(&f(*d).to_le_bytes());
    }
    let p = [pkt3(0x2f, 3), 1, 8 | 8 << 8, base as u32, pkt3(0x39, 4), 4 | 1 << 4 | 6 << 16, 1 << 16, 2, 2 | 3 << 16];
    g.r.execute(&mut g.ram, &p);
    assert_eq!(g.count(0xff00_00ff), 64);
}

#[test]
fn a_dual_math_instruction_leaves_a_reciprocal_square_root_in_the_alternate_temporary() {
    let mut g = Gpu::new();
    g.constant(0, [4.0, 0.0, 0.0, 0.0]);
    let mut add = pvs(3, 0, 1, [src(1, 1), ZERO, src(2, 0) | 3 << 27]);
    add[0] |= 2 << 27;
    let alt_w = 3 | 3 << 13 | 3 << 16 | 3 << 19 | 3 << 22;
    let scale = pvs(2, 2, 1, [src(0, 1), alt_w, ZERO]);
    g.vertex_program(&[pvs(3, 2, 0, [src(1, 0), ZERO, ZERO]), add, scale]);
    g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
    let red = g.px(1, 1) >> 16 & 0xff;
    assert!((0x7f..=0x80).contains(&red), "red {red:#x}");
}

#[test]
fn lit_takes_its_dot_products_and_exponent_from_each_operands_w() {
    let mut g = Gpu::new();
    g.constant(0, [9.0, 9.0, 9.0, 0.5]);
    g.constant(1, [9.0, 9.0, 9.0, 0.5]);
    g.constant(2, [0.0, 0.0, 0.0, 2.0]);
    let lit = pvs(4 | 1 << 6, 2, 1, [src(2, 0), src(2, 1), src(2, 2)]);
    g.vertex_program(&[pvs(3, 2, 0, [src(1, 0), ZERO, ZERO]), lit]);
    g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
    let p = g.px(1, 1);
    assert_eq!(p >> 16 & 0xff, 0xff);
    assert!((0x7f..=0x80).contains(&(p >> 8 & 0xff)));
    assert!((0x3f..=0x40).contains(&(p & 0xff)));
}

#[cfg(target_os = "macos")]
mod against_metal {
    use super::*;

    /// Draws `scene` on the CPU and through Metal and expects the same
    /// pixels: every channel within `tolerance`, apart from at most `edges`
    /// pixels where the two rasterizers' subpixel precision differs.
    fn same(scene: impl Fn(&mut Gpu), tolerance: u32, edges: usize) {
        let mut cpu = Gpu::new();
        scene(&mut cpu);
        let mut gpu = Gpu::new();
        if !gpu.r.use_metal() {
            eprintln!("no Metal device; skipping");
            return;
        }
        scene(&mut gpu);
        gpu.r.gpu_sync();
        let (draws, fallbacks, _) = gpu.r.metal_stats().unwrap();
        assert!(draws > 0 && fallbacks == 0, "Metal drew {draws}, left {fallbacks} to the CPU");
        let mut differ = 0;
        for y in 0..64 {
            for x in 0..64 {
                let (a, b) = (cpu.px(x, y), gpu.px(x, y));
                let far = [0, 8, 16, 24].iter().any(|s| ((a >> s) & 0xff).abs_diff((b >> s) & 0xff) > tolerance);
                if far {
                    differ += 1;
                    if differ <= 8 {
                        eprintln!("({x}, {y}): cpu {a:08x} metal {b:08x}");
                    }
                }
            }
        }
        assert!(differ <= edges, "{differ} pixels differ");
        assert!(cpu.count(0) < 64 * 64, "the scene drew nothing");
    }

    #[test]
    fn quads_and_shared_edges_match() {
        same(|g| g.draw(13, &quad(2.0, 3.0, 10.0, 7.0, RED)), 0, 0);
        same(
            |g| {
                g.w(RB3D_BLENDCNTL, 1 | 33 << 16 | 33 << 24);
                let c = [0.25, 0.0, 0.0, 0.0];
                let (a, b, d, e) = (v(0.0, 0.0, c), v(37.3, 5.1, c), v(11.7, 40.9, c), v(50.2, 44.4, c));
                g.draw(4, &[a, b, d, b, e, d]);
            },
            1,
            40,
        );
    }

    #[test]
    fn interpolated_colours_match() {
        same(|g| g.draw(4, &[v(0.0, 0.0, RED), v(64.0, 0.0, BLUE), v(0.0, 64.0, RED)]), 2, 64);
    }

    #[test]
    fn vertex_programs_culling_and_viewports_match() {
        same(
            |g| {
                g.constant(0, [2.0, 2.0, 1.0, 1.0]);
                g.constant(1, [16.0, 8.0, 0.0, 0.0]);
                g.vertex_program(&[pvs(4, 2, 0, [src(1, 0), src(2, 0), src(2, 1)]), pvs(3, 2, 1, [src(1, 1), ZERO, ZERO])]);
                g.draw(13, &quad(0.0, 0.0, 4.0, 4.0, RED));
            },
            0,
            0,
        );
        same(wrapped_program, 0, 0);
        same(
            |g| {
                let tri = [v(0.0, 0.0, RED), v(0.0, 20.0, RED), v(20.0, 0.0, RED)];
                g.w(SU_CULL_MODE, 1);
                g.draw(4, &tri);
                g.w(SU_CULL_MODE, 2);
                g.draw(4, &tri.map(|mut v| {
                    v[4] = 0.0;
                    v[6] = 1.0;
                    v
                }));
            },
            0,
            40,
        );
        same(
            |g| {
                g.w(VAP_VTE_CNTL, 0xf | 1 << 10);
                for (reg, val) in [(VAP_VPORT_XSCALE, 32.0), (VAP_VPORT_XOFFSET, 32.0), (VAP_VPORT_YSCALE, 32.0), (VAP_VPORT_YOFFSET, 32.0)] {
                    g.w(reg, f(val));
                }
                let at = |x: f32, y: f32, w: f32| [x, y, 0.0, w, 1.0, 0.0, 0.0, 1.0];
                g.draw(4, &[at(-0.5, -0.5, 1.0), at(0.5, -0.5, 1.0), at(0.0, 1.5, -1.0)]);
            },
            0,
            40,
        );
    }

    #[test]
    fn fog_matches() {
        for function in 0..4 {
            same(|g| {
                fogged(g, function);
                g.w(FG_FOG_FACTOR, 300);
                g.draw(4, &[v(0.0, 20.0, RED), v(64.0, 20.0, RED), v(0.0, 64.0, BLUE)].map(|mut v| {
                    v[3] = 0.3 + v[0] / 64.0;
                    v
                }));
            }, 2, 64);
        }
    }

    fn depth(g: &mut Gpu, z16: bool) {
        g.w(ZB_CNTL, 6);
        g.w(ZB_ZSTENCILCNTL, 1);
        g.w(ZB_FORMAT, if z16 { 0 } else { 2 });
        g.w(ZB_DEPTHOFFSET, VRAM + DEPTH);
        g.w(ZB_DEPTHPITCH, PITCH as u32);
        g.w(SU_DEPTH_SCALE, f(16_777_215.0));
        for i in 0..PITCH * 64 {
            g.put(DEPTH as usize + 4 * i, 0xffff_ffff);
        }
    }

    #[test]
    fn depth_tests_match_in_both_depth_formats() {
        for z16 in [false, true] {
            same(
                |g| {
                    depth(g, z16);
                    let at = |z0: f32, z1: f32, c: [f32; 4]| {
                        let mut q = quad(0.0, 0.0, 40.0, 40.0, c);
                        for (i, v) in q.iter_mut().enumerate() {
                            v[2] = if i % 2 == 0 { z0 } else { z1 };
                        }
                        q
                    };
                    g.draw(13, &at(0.2, 0.8, RED));
                    g.draw(13, &at(0.8, 0.2, BLUE));
                },
                0,
                64,
            );
        }
    }

    #[test]
    fn stencil_and_alpha_tests_match() {
        same(
            |g| {
                depth(g, false);
                g.w(ZB_CNTL, 1);
                g.w(ZB_ZSTENCILCNTL, 7 << 3 | 2 << 9);
                g.w(ZB_STENCILREFMASK, 5 | 0xff << 8 | 0xff << 16);
                g.draw(13, &quad(0.0, 0.0, 20.0, 20.0, RED));
                g.w(ZB_ZSTENCILCNTL, 3 << 3);
                g.draw(13, &quad(10.0, 10.0, 40.0, 40.0, BLUE));
                g.w(ZB_CNTL, 0);
                g.w(FG_ALPHA_FUNC, 1 << 11 | 4 << 8 | 0x80);
                g.draw(13, &quad(30.0, 0.0, 60.0, 20.0, [0.0, 1.0, 0.0, 0.25]));
                g.draw(13, &quad(30.0, 30.0, 60.0, 60.0, [0.0, 1.0, 0.0, 0.75]));
            },
            0,
            0,
        );
    }

    #[test]
    fn blending_rops_masks_and_clip_rules_match() {
        same(
            |g| {
                g.draw(13, &quad(0.0, 0.0, 64.0, 64.0, [0.2, 0.4, 0.6, 0.5]));
                g.w(RB3D_BLENDCNTL, 1 | 5 << 16 | 6 << 24);
                g.draw(13, &quad(8.0, 8.0, 40.0, 40.0, [0.9, 0.1, 0.3, 0.7]));
                g.w(RB3D_BLENDCNTL, 1 | 2 << 12 | 2 << 16 | 2 << 24);
                g.draw(13, &quad(20.0, 20.0, 60.0, 30.0, [0.1, 0.1, 0.1, 0.1]));
                g.w(RB3D_BLENDCNTL, 0);
                g.w(RB3D_ROPCNTL, 4 | 6 << 8);
                g.draw(13, &quad(0.0, 44.0, 64.0, 52.0, [1.0, 1.0, 0.0, 1.0]));
                g.w(RB3D_ROPCNTL, 0);
                g.w(RB3D_COLOR_CHANNEL_MASK, 0b0101);
                g.draw(13, &quad(0.0, 56.0, 64.0, 64.0, [1.0, 1.0, 1.0, 1.0]));
                g.w(RB3D_COLOR_CHANNEL_MASK, 0xf);
                g.w(SC_CLIP_0_A, (SC + 4) | (SC + 4) << 13);
                g.w(SC_CLIP_0_A + 4, (SC + 12) | (SC + 9) << 13);
                g.w(SC_CLIP_RULE, 0xaaaa);
                g.draw(13, &quad(0.0, 0.0, 64.0, 64.0, BLUE));
            },
            1,
            0,
        );
    }

    #[test]
    fn colour_buffer_formats_match() {
        for (format, pitch) in [(4, PITCH as u32 * 2), (3, PITCH as u32 * 2), (15, PITCH as u32 * 2), (13, PITCH as u32 * 2), (9, PITCH as u32 * 4)] {
            same(
                |g| {
                    g.w(RB3D_COLORPITCH0, pitch | format << 21);
                    g.draw(4, &[v(0.0, 0.0, RED), v(64.0, 0.0, [0.0, 1.0, 0.5, 0.25]), v(0.0, 64.0, BLUE)]);
                },
                0,
                64,
            );
        }
    }

    fn texture(g: &mut Gpu, format: u32, size: u32, levels: u32, filter: u32, texels: &[u8]) {
        for (i, b) in texels.iter().enumerate() {
            g.r.vram_mut()[TEXTURE as usize + i] = *b;
        }
        g.w(VAP_PROG_STREAM_CNTL_0, 3 | 3 << 16 | 1 << 24);
        g.w(VAP_PROG_STREAM_CNTL_0 + 4, 1 | 2 << 8 | 1 << 13);
        g.w(VAP_PROG_STREAM_CNTL_EXT_0 + 4, 0xfb08);
        g.w(VAP_VTX_SIZE, 10);
        g.w(VAP_OUT_VTX_FMT_0, 3);
        g.w(VAP_OUT_VTX_FMT_1, 2);
        g.vertex_program(&[pvs(3, 2, 0, [src(1, 0), ZERO, ZERO]), pvs(3, 2, 1, [src(1, 1), ZERO, ZERO]), pvs(3, 2, 2, [src(1, 2), ZERO, ZERO])]);
        g.w(RS_IP_0, 1 << 16 | 2 << 19 | 4 << 22);
        g.w(RS_INST_COUNT, 0);
        g.w(RS_INST_0, 1 << 3 | 1 << 14 | 1 << 17);
        g.w(TX_FILTER0_0, filter);
        g.w(TX_FORMAT0_0, (size - 1) | (size - 1) << 11 | levels << 26);
        g.w(TX_FORMAT1_0, format | 0xa60 << 4);
        g.w(TX_OFFSET_0, VRAM + TEXTURE);
        g.w(TX_ENABLE, 1);
        g.w(US_TEX_INST_0, 1 << 15);
        let modulate = [1 << 6 | 7 << 26, 4 << 7 | 20 << 14, 1 << 6 | 1 << 24, 9 | 10 << 7 | 16 << 14];
        g.fragment(&[modulate], 1);
    }

    fn draw_uv(g: &mut Gpu, pts: [(f32, f32, f32, f32); 4]) {
        let verts: Vec<f32> = pts.iter().flat_map(|&(x, y, s, t)| [x, y, 0.5, 1.0, 1.0, 1.0, 1.0, 1.0, s, t]).collect();
        let mut p = vec![pkt3(0x35, 1 + verts.len()), 13 | 3 << 4 | 4 << 16];
        p.extend(verts.iter().map(|&v| f(v)));
        g.r.execute(&mut g.ram, &p);
    }

    fn checker(size: u32, bpp: usize, a: &[u8], b: &[u8], levels: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for l in 0..=levels {
            let s = (size >> l).max(1) as usize;
            let pitch = (s * bpp).next_multiple_of(32);
            let rows = s;
            let mut level = vec![0u8; pitch * rows];
            for y in 0..s {
                for x in 0..s {
                    let c = if ((x >> 1) ^ (y >> 1)) & 1 == 0 { a } else { b };
                    level[y * pitch + x * bpp..][..bpp].copy_from_slice(c);
                }
            }
            out.extend(level);
            out.resize(out.len().next_multiple_of(32), 0);
        }
        out
    }

    #[test]
    fn nearest_bilinear_and_mipmapped_textures_match() {
        let tex = checker(16, 4, &[0x10, 0x80, 0xf0, 0xff], &[0xf0, 0x20, 0x40, 0x80], 4);
        for filter in [0, 2 << 9 | 2 << 11, 2 << 9 | 2 << 11 | 1 << 13] {
            same(
                |g| {
                    texture(g, 12, 16, 4, filter, &tex);
                    draw_uv(g, [(0.0, 0.0, 0.0, 0.0), (64.0, 0.0, 1.0, 0.0), (64.0, 64.0, 1.0, 1.0), (0.0, 64.0, 0.0, 1.0)]);
                    draw_uv(g, [(4.0, 4.0, 0.0, 0.0), (12.0, 4.0, 3.0, 0.0), (12.0, 12.0, 3.0, 3.0), (4.0, 12.0, 0.0, 3.0)]);
                },
                2,
                0,
            );
        }
    }

    #[test]
    fn texture_formats_match() {
        let dxt1 = [0x00, 0xf8, 0x1f, 0x00, 0b11100100, 0b00011011, 0b10101010, 0b01010101];
        let tex16 = checker(8, 2, &[0x1f, 0xf8], &[0xe0, 0x07], 0);
        for (format, data) in [(0xf, dxt1.repeat(4)), (6, tex16.clone()), (11, tex16.clone()), (10, tex16)] {
            same(
                |g| {
                    texture(g, format, 8, 0, 0, &data);
                    draw_uv(g, [(0.0, 0.0, 0.0, 0.0), (64.0, 0.0, 1.0, 0.0), (64.0, 64.0, 1.0, 1.0), (0.0, 64.0, 0.0, 1.0)]);
                },
                1,
                0,
            );
        }
    }

    #[test]
    fn the_fragment_alu_matches() {
        same(
            |g| {
                let rgb_inst = 21 | 20 << 7 | 1 << 14 | 8 << 23;
                g.fragment(&[[32 | 7 << 26, rgb_inst, 1 << 24, 17 | 17 << 7 | 16 << 14]], 0);
                g.w(US_ALU_CONST_R_0, 0xbf_0000);
                g.draw(13, &quad(0.0, 0.0, 30.0, 30.0, RED));
                g.w(US_ALU_CONST_R_0, 0x3f_0000);
                g.draw(13, &quad(30.0, 30.0, 60.0, 60.0, RED));
            },
            0,
            0,
        );
    }

    #[test]
    fn point_sprites_and_lines_match() {
        same(|g| textured_sprite(g, 3, 0), 0, 0);
        same(
            |g| {
                g.w(GA_LINE_CNTL, 32);
                g.draw(3, &[v(2.0, 2.0, RED), v(60.0, 20.0, BLUE), v(10.0, 60.0, RED)]);
            },
            2,
            40,
        );
    }

    #[test]
    fn occlusion_counts_match() {
        let mut counts = Vec::new();
        for metal in [false, true] {
            let mut g = Gpu::new();
            if metal && !g.r.use_metal() {
                return;
            }
            depth(&mut g, false);
            g.w(ZB_ZPASS_DATA, 0);
            g.draw(13, &quad(0.0, 0.0, 10.0, 10.0, RED));
            g.draw(13, &quad(5.0, 5.0, 20.0, 20.0, RED));
            g.w(ZB_ZPASS_ADDR, 0x9000);
            counts.push(u32::from_le_bytes(g.ram.0[0x9000..0x9004].try_into().unwrap()));
        }
        assert_eq!(counts[0], counts[1]);
    }

    fn scratch_in_memory(g: &Gpu) -> u32 {
        u32::from_le_bytes(g.ram.0[0x9000..0x9004].try_into().unwrap())
    }

    #[test]
    fn a_fence_after_draws_lands_only_once_their_pixels_have() {
        let mut g = Gpu::new();
        if !g.r.use_metal() {
            return;
        }
        g.w(SCRATCH_ADDR, 0x9000);
        g.w(SCRATCH_UMSK, 1);
        g.draw(13, &quad(0.0, 0.0, 64.0, 64.0, RED));
        g.w(SCRATCH_REG0, 7);
        assert_eq!(scratch_in_memory(&g), 0);
        let start = std::time::Instant::now();
        while scratch_in_memory(&g) != 7 {
            assert!(start.elapsed().as_secs() < 5, "the fence never arrived");
            g.r.poll_fences(&mut g.ram);
        }
        assert_eq!(g.r.mmio_read(SCRATCH_REG0, 4), 7);
        assert_eq!(g.px(63, 63), 0xffff_0000);
    }

    #[test]
    fn a_fence_with_nothing_queued_lands_at_once() {
        let mut g = Gpu::new();
        if !g.r.use_metal() {
            return;
        }
        g.w(SCRATCH_ADDR, 0x9000);
        g.w(SCRATCH_UMSK, 1);
        g.w(SCRATCH_REG0, 9);
        assert_eq!(scratch_in_memory(&g), 9);
    }
}


fn stale_depth_with_a_cleared_zmask(g: &mut Gpu, fast_fill: bool) {
    g.w(ZB_CNTL, 6);
    g.w(ZB_ZSTENCILCNTL, 1);
    g.w(ZB_FORMAT, 2);
    g.w(ZB_DEPTHOFFSET, VRAM + DEPTH);
    g.w(ZB_DEPTHPITCH, PITCH as u32);
    g.w(SU_DEPTH_SCALE, f(16_777_215.0));
    g.w(ZB_ZMASK_PITCH, PITCH as u32);
    g.w(ZB_DEPTHCLEARVALUE, 0xffff_ff00);
    g.w(ZB_BW_CNTL, if fast_fill { FAST_FILL | 3 << 3 } else { 0 });
    let p = [pkt3(0x32, 3), 0, 8, 0];
    g.r.execute(&mut g.ram, &p);
}

fn at_depth(z: f32, c: [f32; 4]) -> [[f32; 8]; 4] {
    quad(0.0, 0.0, 16.0, 16.0, c).map(|mut v| {
        v[2] = z;
        v
    })
}

#[test]
fn a_zeroed_zmask_reads_as_the_depth_clear_value_with_fast_fill() {
    let mut g = Gpu::new();
    stale_depth_with_a_cleared_zmask(&mut g, true);
    g.draw(13, &at_depth(0.5, RED));
    assert_eq!(g.count(0xffff_0000), 256);
    g.draw(13, &at_depth(0.75, BLUE));
    assert_eq!(g.count(0xffff_0000), 256, "the clear applies once, then Z holds the red quad");
}

#[test]
fn the_zmask_is_ignored_without_fast_fill() {
    let mut g = Gpu::new();
    stale_depth_with_a_cleared_zmask(&mut g, false);
    g.draw(13, &at_depth(0.5, RED));
    assert_eq!(g.count(0xffff_0000), 0);
}

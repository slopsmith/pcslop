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

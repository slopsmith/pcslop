//! The R300 3D engine, run on the CPU: the 3D_DRAW packets, vertex fetch
//! through the programmable stream control (PSC), the vertex shader, the
//! viewport transform, primitive assembly and the rasterizer and back end.
//!
//! Drawing happens while the packet executes, so fences, scratch writebacks
//! and WAIT_UNTIL that follow it always see finished pixels. Like the 2D
//! engine, the 3D engine treats every surface as linear and ignores the
//! tiling bits, which keeps what it draws consistent with the CRTC and the
//! 2D engine.
//!
//! Packet layouts follow AMD's R5xx Acceleration guide (v1.5, section 6.2.3)
//! and registers its R3xx 3D register reference.

mod pvs;
mod raster;
pub(crate) mod regs;
mod tex;
mod us;

#[cfg(test)]
mod tests;

use crate::Radeon;
use crate::mem::BusMemory;
use raster::{ColourBuffer, DepthBuffer, Raster, Vertex};
use regs::*;
use tex::{TexMem, Texture};

pub(crate) type V4 = [f32; 4];

pub(crate) mod op {
    pub const DRAW_VBUF: u8 = 0x28;
    pub const DRAW_IMMD: u8 = 0x29;
    pub const DRAW_INDX: u8 = 0x2a;
    pub const LOAD_VBPNTR: u8 = 0x2f;
    pub const INDX_BUFFER: u8 = 0x33;
    pub const DRAW_VBUF_2: u8 = 0x34;
    pub const DRAW_IMMD_2: u8 = 0x35;
    pub const DRAW_INDX_2: u8 = 0x36;
    pub const CLEAR_HIZ: u8 = 0x37;
    pub const DRAW_128: u8 = 0x39;
}

const VECMEM_DWORDS: usize = 2048 * 4;
const MAX_VERTICES: usize = 1 << 20;

#[derive(snapshot::Snap)]
pub(crate) struct ThreeD {
    /// The VAP's vector memory: vertex program code and constants.
    vecmem: Vec<u32>,
    /// Next dword VAP_PVS_VECTOR_DATA_REG writes.
    vec_ptr: u32,
    /// VAP_VF_CNTL of a 3D_DRAW_INDX that waits for INDX_BUFFER's indices.
    pending_indx: Option<u32>,
    pub draws: u64,
}

impl Default for ThreeD {
    fn default() -> Self {
        ThreeD { vecmem: vec![0; VECMEM_DWORDS], vec_ptr: 0, pending_indx: None, draws: 0 }
    }
}

enum Verts<'a> {
    Immediate(&'a [u32]),
    List,
    Indices(Vec<u32>),
}

fn bits(v: u32, shift: u32, n: u32) -> u32 {
    (v >> shift) & ((1 << n) - 1)
}

/// Converts a fixed-point vertex component as the PSC's SIGNED and
/// NORMALIZE flags say.
fn fixed(raw: u32, n: u32, signed: bool, normalize: bool) -> f32 {
    let max = ((1u64 << n) - 1) as f32;
    if signed {
        let v = ((raw << (32 - n)) as i32 >> (32 - n)) as f32;
        if normalize { (v / ((1u64 << (n - 1)) - 1) as f32).max(-1.0) } else { v }
    } else {
        let v = (raw & ((1u64 << n) - 1) as u32) as f32;
        if normalize { v / max } else { v }
    }
}

impl Radeon {
    pub(crate) fn r3d_register_written(&mut self, reg: u32, v: u32) -> bool {
        match reg {
            VAP_VPORT_ALIAS..=VAP_VPORT_ALIAS_END => {
                self.regs[(reg - VAP_VPORT_ALIAS + VAP_VPORT_XSCALE) as usize / 4] = v;
            }
            VAP_VPORT_XSCALE..=VAP_VPORT_ZOFFSET => {
                self.regs[(reg - VAP_VPORT_XSCALE + VAP_VPORT_ALIAS) as usize / 4] = v;
            }
            VAP_PVS_VECTOR_INDX_REG => self.r3d.vec_ptr = (v & 0x7ff) * 4,
            VAP_PVS_VECTOR_DATA_REG | VAP_PVS_VECTOR_DATA_REG_128 => {
                let p = self.r3d.vec_ptr as usize % VECMEM_DWORDS;
                self.r3d.vecmem[p] = v;
                self.r3d.vec_ptr = (p as u32 + 1) % VECMEM_DWORDS as u32;
            }
            _ => return false,
        }
        true
    }

    /// Writes the occlusion counter to memory when ZB_ZPASS_ADDR is written.
    pub(crate) fn zpass_written(&mut self, reg: u32, v: u32, mem: &mut dyn BusMemory) -> bool {
        if reg != ZB_ZPASS_ADDR {
            return false;
        }
        let count = self.reg(ZB_ZPASS_DATA);
        self.mc_write_dword(mem, v & !3, count);
        true
    }

    /// Runs a 3D packet; false if `opcode` is not one.
    pub(crate) fn packet_3d(&mut self, mem: &mut dyn BusMemory, opcode: u8, body: &[u32]) -> bool {
        let get = |i: usize| body.get(i).copied().unwrap_or(0);
        match opcode {
            op::DRAW_IMMD => self.draw_3d(mem, get(1), Verts::Immediate(body.get(2..).unwrap_or(&[]))),
            op::DRAW_IMMD_2 | op::DRAW_128 => self.draw_3d(mem, get(0), Verts::Immediate(body.get(1..).unwrap_or(&[]))),
            op::DRAW_VBUF => self.draw_3d(mem, get(1), Verts::List),
            op::DRAW_VBUF_2 => self.draw_3d(mem, get(0), Verts::List),
            op::DRAW_INDX | op::DRAW_INDX_2 => {
                let skip = if opcode == op::DRAW_INDX { 1 } else { 0 };
                let vf = get(skip);
                let words = body.get(skip + 1..).unwrap_or(&[]);
                if words.is_empty() {
                    self.r3d.pending_indx = Some(vf);
                } else {
                    let idx = self.indices(vf, words);
                    self.draw_3d(mem, vf, Verts::Indices(idx));
                }
            }
            op::INDX_BUFFER => {
                let skip = bits(get(0), 16, 3) as usize;
                let size = (get(2) & 0x7f_ffff) as usize;
                let words = self.mc_read_dwords(mem, get(1) & !3, size.min(1 << 22));
                if let Some(vf) = self.r3d.pending_indx.take() {
                    let idx = self.indices(vf, words.get(skip..).unwrap_or(&[]));
                    self.draw_3d(mem, vf, Verts::Indices(idx));
                }
            }
            op::LOAD_VBPNTR => {
                let n = (get(0) & 31) as usize;
                self.regs[VAP_VTX_NUM_ARRAYS as usize / 4] = get(0);
                let mut i = 1;
                for pair in 0..n.div_ceil(2) {
                    let base = (VAP_VTX_AOS_ATTR01 + 12 * pair as u32) as usize / 4;
                    self.regs[base] = get(i);
                    self.regs[base + 1] = get(i + 1);
                    let second = 2 * pair + 1 < n;
                    if second {
                        self.regs[base + 2] = get(i + 2);
                    }
                    i += if second { 3 } else { 2 };
                }
            }
            op::CLEAR_HIZ => {}
            _ => return false,
        }
        true
    }

    fn indices(&self, vf: u32, words: &[u32]) -> Vec<u32> {
        let count = (vf >> 16) as usize;
        let wide = vf & (1 << 11) != 0;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let v = if wide {
                words.get(i).copied()
            } else {
                words.get(i / 2).map(|w| if i % 2 == 0 { w & 0xffff } else { w >> 16 })
            };
            match v {
                Some(v) => out.push(v),
                None => break,
            }
        }
        out
    }

    /// The dwords of each vertex, as the PSC sees them: in immediate mode
    /// straight from the packet, otherwise each array of structures in turn.
    fn vertex_dwords(&mut self, mem: &mut dyn BusMemory, vf: u32, verts: &Verts<'_>) -> Vec<Vec<u32>> {
        let count = ((vf >> 16) as usize).min(MAX_VERTICES);
        match verts {
            Verts::Immediate(data) => {
                let size = (self.reg(VAP_VTX_SIZE) & 0x7f).max(1) as usize;
                data.chunks(size).take(count).map(|c| c.to_vec()).collect()
            }
            Verts::List | Verts::Indices(_) => {
                let index: Vec<u32> = match verts {
                    Verts::Indices(i) => {
                        let (lo, hi) = (self.reg(VAP_VF_MIN_VTX_INDX) & 0xff_ffff, self.reg(VAP_VF_MAX_VTX_INDX) & 0xff_ffff);
                        let hi = if hi == 0 { 0xff_ffff } else { hi };
                        i.iter().map(|&x| x.clamp(lo, hi.max(lo))).collect()
                    }
                    _ => (0..count as u32).collect(),
                };
                let arrays = (self.reg(VAP_VTX_NUM_ARRAYS) & 31) as usize;
                let max = index.iter().copied().max().unwrap_or(0) as usize;
                let mut fetched = Vec::with_capacity(arrays);
                for a in 0..arrays.min(16) {
                    let pair = a / 2;
                    let attr_reg = VAP_VTX_AOS_ATTR01 + 12 * pair as u32;
                    let attr = self.reg(attr_reg) >> (16 * (a % 2) as u32);
                    let addr = self.reg(attr_reg + 4 + 4 * (a % 2) as u32) & !3;
                    let (n, stride) = (bits(attr, 0, 7) as usize, bits(attr, 8, 7) as usize);
                    let total = (max * stride + n).min(1 << 22);
                    fetched.push((self.mc_read_dwords(mem, addr, total), n, stride));
                }
                index
                    .iter()
                    .map(|&i| {
                        let mut v = Vec::new();
                        for (data, n, stride) in &fetched {
                            let at = i as usize * stride;
                            v.extend((0..*n).map(|k| data.get(at + k).copied().unwrap_or(0)));
                        }
                        v
                    })
                    .collect()
            }
        }
    }

    /// Splits a vertex's dwords into input vectors through the PSC.
    fn psc(&self, dwords: &[u32]) -> [V4; pvs::INPUTS] {
        let mut inputs = [[0.0, 0.0, 0.0, 1.0]; pvs::INPUTS];
        let mut at = 0;
        let norm_cntl = self.reg(VAP_PSC_SGN_NORM_CNTL);
        for e in 0..16u32 {
            let cntl = self.reg(VAP_PROG_STREAM_CNTL_0 + 4 * (e / 2)) >> (16 * (e % 2));
            let ext = self.reg(VAP_PROG_STREAM_CNTL_EXT_0 + 4 * (e / 2)) >> (16 * (e % 2));
            let kind = cntl & 15;
            let signed = cntl & (1 << 14) != 0;
            let normalize = cntl & (1 << 15) != 0;
            let d = |i: usize| dwords.get(at + i).copied().unwrap_or(0);
            let _ = norm_cntl;
            let (raw, used): (V4, usize) = match kind {
                0..=3 => {
                    let n = kind as usize + 1;
                    let mut v = [0.0, 0.0, 0.0, 1.0];
                    for (c, x) in v.iter_mut().enumerate().take(n) {
                        *x = f32::from_bits(d(c));
                    }
                    (v, n)
                }
                4 | 5 => {
                    let w = d(0);
                    let mut v = [0, 8, 16, 24].map(|s| fixed(w >> s, 8, signed, normalize));
                    if kind == 5 {
                        v.swap(0, 2);
                    }
                    (v, 1)
                }
                6 => {
                    let w = d(0);
                    ([fixed(w, 16, signed, normalize), fixed(w >> 16, 16, signed, normalize), 0.0, 1.0], 1)
                }
                7 => {
                    let (w0, w1) = (d(0), d(1));
                    ([w0, w0 >> 16, w1, w1 >> 16].map(|x| fixed(x, 16, signed, normalize)), 2)
                }
                8 => {
                    let w = d(0);
                    ([fixed(w, 10, signed, normalize), fixed(w >> 10, 10, signed, normalize), fixed(w >> 20, 10, signed, normalize), 1.0], 1)
                }
                _ => {
                    let w = d(0);
                    ([fixed(w, 11, signed, normalize), fixed(w >> 11, 11, signed, normalize), fixed(w >> 22, 10, signed, normalize), 1.0], 1)
                }
            };
            let dst = bits(cntl, 8, 5) as usize;
            let mask = bits(ext, 12, 4);
            for c in 0..4 {
                if mask & (1 << c) != 0 {
                    inputs[dst][c] = match bits(ext, 3 * c as u32, 3) {
                        s @ 0..=3 => raw[s as usize],
                        5 => 1.0,
                        _ => 0.0,
                    };
                }
            }
            at += used + bits(cntl, 4, 4) as usize;
            if cntl & (1 << 13) != 0 {
                break;
            }
        }
        inputs
    }

    /// The VAP's output vector for each vertex attribute the GA receives.
    fn setup_vertex(&self, out: &[V4; pvs::OUTPUTS]) -> Vertex {
        let fmt0 = self.reg(VAP_OUT_VTX_FMT_0);
        let fmt1 = self.reg(VAP_OUT_VTX_FMT_1);
        let mut slot = 0;
        let mut next = || {
            let v = out.get(slot).copied().unwrap_or([0.0; 4]);
            slot += 1;
            v
        };
        let mut v = Vertex { pos: next(), ..Default::default() };
        if fmt0 & (1 << 16) != 0 {
            v.psize = Some(next()[0]);
        }
        for c in 0..4 {
            if fmt0 & (2 << c) != 0 {
                v.colours[c] = next();
            }
        }
        for t in 0..8 {
            if bits(fmt1, 3 * t, 3) != 0 {
                v.tex[t as usize] = next();
            }
        }
        self.viewport(&mut v);
        v
    }

    fn viewport(&self, v: &mut Vertex) {
        let vte = self.reg(VAP_VTE_CNTL);
        let f = |r: u32| f32::from_bits(self.reg(r));
        let w = v.pos[3];
        let rcp = if vte & (1 << 10) != 0 { if w != 0.0 { 1.0 / w } else { 0.0 } } else { w };
        let [mut x, mut y, mut z, _] = v.pos;
        if vte & (1 << 8) == 0 {
            x *= rcp;
            y *= rcp;
        }
        if vte & (1 << 9) == 0 {
            z *= rcp;
        }
        if vte & 1 != 0 {
            x *= f(VAP_VPORT_XSCALE);
        }
        if vte & 2 != 0 {
            x += f(VAP_VPORT_XOFFSET);
        }
        if vte & 4 != 0 {
            y *= f(VAP_VPORT_YSCALE);
        }
        if vte & 8 != 0 {
            y += f(VAP_VPORT_YOFFSET);
        }
        if vte & 16 != 0 {
            z *= f(VAP_VPORT_ZSCALE);
        }
        if vte & 32 != 0 {
            z += f(VAP_VPORT_ZOFFSET);
        }
        let ga = self.reg(GA_OFFSET);
        let sub = self.subpixels();
        x += ((ga & 0xffff) as i16) as f32 / sub;
        y += ((ga >> 16) as i16) as f32 / sub;
        v.pos = [x, y, z, rcp];
    }

    fn subpixels(&self) -> f32 {
        if self.reg(GB_TILE_CONFIG) & (1 << 16) != 0 { 16.0 } else { 12.0 }
    }

    fn texture(&mut self, mem: &mut dyn BusMemory, unit: u32) -> Option<Texture> {
        if self.reg(TX_ENABLE) & (1 << unit) == 0 {
            return None;
        }
        let r = |base: u32| self.reg(base + 4 * unit);
        let (f0, f1, f2, off) = (r(TX_FORMAT0_0), r(TX_FORMAT1_0), r(TX_FORMAT2_0), r(TX_OFFSET_0));
        let (filter, border) = (r(TX_FILTER0_0), r(TX_BORDER_COLOR_0));
        let width = bits(f0, 0, 11) + 1;
        let height = bits(f0, 11, 11) + 1;
        let levels = bits(f0, 26, 4);
        let pitch = (f0 & (1 << 31) != 0).then(|| bits(f2, 0, 14) + 1);
        let format = f1 & 31;
        let base = off & !31;
        let mut t = Texture::new(TexMem::Vram(0), width, height, format, levels, pitch);
        t.mem = match self.surface_vram(base) {
            Some(o) => TexMem::Vram(o),
            None => {
                let mut buf = vec![0; t.span().min(64 << 20)];
                self.mc_read(mem, base, &mut buf);
                TexMem::Copy(buf)
            }
        };
        t.sel = [bits(f1, 9, 3), bits(f1, 12, 3), bits(f1, 15, 3), bits(f1, 18, 3)];
        t.filter = filter;
        t.border = border;
        Some(t)
    }

    fn fragment_program(&self) -> us::Program {
        let config = self.reg(US_CONFIG);
        let levels = (config & 7) as usize + 1;
        let offset = self.reg(US_CODE_OFFSET);
        let (alu_off, tex_off) = (bits(offset, 0, 6) as usize, bits(offset, 13, 5) as usize);
        let mut nodes = Vec::new();
        for (k, n) in (4 - levels.min(4)..4).enumerate() {
            let a = self.reg(US_CODE_ADDR_0 + 4 * n as u32);
            let alu = (alu_off + bits(a, 0, 6) as usize, bits(a, 6, 6) as usize + 1);
            let has_tex = k > 0 || config & (1 << 3) != 0;
            let tex = has_tex.then(|| (tex_off + bits(a, 12, 5) as usize, bits(a, 17, 5) as usize + 1));
            nodes.push(us::Node { alu, tex });
        }
        us::Program {
            nodes,
            tex: (0..32).map(|i| self.reg(US_TEX_INST_0 + 4 * i)).collect(),
            alu: (0..64)
                .map(|i| {
                    [
                        self.reg(US_ALU_RGB_ADDR_0 + 4 * i),
                        self.reg(US_ALU_RGB_INST_0 + 4 * i),
                        self.reg(US_ALU_ALPHA_ADDR_0 + 4 * i),
                        self.reg(US_ALU_ALPHA_INST_0 + 4 * i),
                    ]
                })
                .collect(),
            consts: (0..32)
                .map(|i| [0, 1, 2, 3].map(|c| us::s16e7(self.reg(US_ALU_CONST_R_0 + 16 * i + 4 * c))))
                .collect(),
        }
    }

    fn rect(&self, a: u32, b: u32, offset: i32) -> [i32; 4] {
        let (va, vb) = (self.reg(a), self.reg(b));
        [
            bits(va, 0, 13) as i32 - offset,
            bits(va, 13, 13) as i32 - offset,
            bits(vb, 0, 13) as i32 - offset,
            bits(vb, 13, 13) as i32 - offset,
        ]
    }

    fn raster_state(&mut self, mem: &mut dyn BusMemory) -> Raster {
        let textures = std::array::from_fn(|u| self.texture(mem, u as u32));
        let offset = if self.reg(GB_TILE_CONFIG) & (1 << 16) != 0 { 1088 } else { 1440 };
        let f = |r: u32| f32::from_bits(self.reg(r));
        let multi = bits(self.reg(RB3D_CCTL), 5, 2) as usize + 1;
        let colour = (0..multi as u32)
            .filter_map(|i| {
                let p = self.reg(RB3D_COLORPITCH0 + 4 * i);
                let pitch = (p & 0x3ffe) as usize;
                let format = bits(p, 21, 4);
                let offset = self.surface_vram(self.reg(RB3D_COLOROFFSET0 + 4 * i) & !31)?;
                (pitch > 0).then_some(ColourBuffer { offset, pitch, format })
            })
            .collect();
        let zb_cntl = self.reg(ZB_CNTL);
        let depth = (zb_cntl & 3 != 0)
            .then(|| {
                let offset = self.surface_vram(self.reg(ZB_DEPTHOFFSET) & !31)?;
                let pitch = (self.reg(ZB_DEPTHPITCH) & 0x3ffc) as usize;
                Some(DepthBuffer { offset, pitch, z16: self.reg(ZB_FORMAT) & 15 < 2 })
            })
            .flatten();
        let fmt1 = self.reg(VAP_OUT_VTX_FMT_1);
        let rs_count = (self.reg(RS_INST_COUNT) & 15) as u32 + 1;
        let solid = {
            let (rg, ba) = (self.reg(GA_SOLID_RG), self.reg(GA_SOLID_BA));
            let c = |v: u32| (v as u16 as i16) as f32 / 4096.0;
            [c(rg >> 16), c(rg), c(ba >> 16), c(ba)]
        };
        Raster {
            colour,
            depth,
            zb_cntl,
            zs_cntl: self.reg(ZB_ZSTENCILCNTL),
            stencil_ref: self.reg(ZB_STENCILREFMASK),
            depth_scale: f(SU_DEPTH_SCALE),
            depth_offset: f(SU_DEPTH_OFFSET),
            poly_offset: (
                self.reg(SU_POLY_OFFSET_ENABLE),
                [f(SU_POLY_OFFSET_FRONT_SCALE), f(SU_POLY_OFFSET_FRONT_OFFSET), f(SU_POLY_OFFSET_BACK_SCALE), f(SU_POLY_OFFSET_BACK_OFFSET)],
            ),
            scissor: self.rect(SC_SCISSOR0, SC_SCISSOR1, offset),
            clips: [0, 1, 2, 3].map(|k| self.rect(SC_CLIP_0_A + 8 * k, SC_CLIP_0_A + 8 * k + 4, offset)),
            clip_rule: self.reg(SC_CLIP_RULE) & 0xffff,
            blend: self.reg(RB3D_BLENDCNTL),
            ablend: self.reg(RB3D_ABLENDCNTL),
            channel_mask: self.reg(RB3D_COLOR_CHANNEL_MASK),
            constant: self.reg(RB3D_CONSTANT_COLOR),
            rop: self.reg(RB3D_ROPCNTL),
            alpha_func: self.reg(FG_ALPHA_FUNC),
            cull: self.reg(SU_CULL_MODE),
            colour_control: self.reg(GA_COLOR_CONTROL),
            clamp_colours: self.reg(GA_ROUND_MODE) & (1 << 4) == 0,
            solid,
            out_fmt: self.reg(US_OUT_FMT_0),
            rs_inst: (0..rs_count).map(|i| self.reg(RS_INST_0 + 4 * i)).collect(),
            rs_ip: std::array::from_fn(|i| self.reg(RS_IP_0 + 4 * i as u32)),
            tex_counts: std::array::from_fn(|t| bits(fmt1, 3 * t as u32, 3)),
            perspective: self.reg(GB_SELECT) & (1 << 4) == 0,
            us: self.fragment_program(),
            textures,
            zpass: self.reg(ZB_ZPASS_DATA),
            pixels: 0,
        }
    }

    fn draw_3d(&mut self, mem: &mut dyn BusMemory, vf: u32, verts: Verts<'_>) {
        self.r3d.draws += 1;
        let prim = vf & 15;
        let dwords = self.vertex_dwords(mem, vf, &verts);
        if self.trace.is_some() {
            let first: Vec<String> = dwords.iter().take(4).map(|d| format!("{:?}", d.iter().map(|w| f32::from_bits(*w)).collect::<Vec<_>>())).collect();
            self.note(&format!("3d: draw vf {vf:#x}, {} vertices {}", dwords.len(), first.join(" ")));
        }
        let bypass = self.reg(VAP_CNTL_STATUS) & PVS_BYPASS != 0;
        let code = self.reg(VAP_PVS_CODE_CNTL_0);
        let program = pvs::Program {
            mem: &self.r3d.vecmem,
            first: bits(code, 0, 10) as usize,
            last: bits(code, 20, 10) as usize,
            const_base: bits(self.reg(VAP_PVS_CONST_CNTL), 0, 8) as usize,
        };
        let vertices: Vec<Vertex> = dwords
            .iter()
            .map(|d| {
                let inputs = self.psc(d);
                let out = if bypass {
                    std::array::from_fn(|i| inputs[i])
                } else {
                    program.run(&inputs)
                };
                self.setup_vertex(&out)
            })
            .collect();
        let mut r = self.raster_state(mem);
        let counts = r.tex_counts;
        let gb = self.reg(GB_ENABLE);
        let vram = self.vga.vram_mut();
        let n = vertices.len();
        let tri = |r: &mut Raster, vram: &mut [u8], a: usize, b: usize, c: usize| {
            r.triangle(vram, [vertices[a], vertices[b], vertices[c]], &counts, true);
        };
        match prim {
            1 => {
                let size = self.regs[GA_POINT_SIZE as usize / 4];
                let sub = if self.regs[GB_TILE_CONFIG as usize / 4] & (1 << 16) != 0 { 16.0 } else { 12.0 };
                let half = ((size >> 16) as f32 / sub, (size & 0xffff) as f32 / sub);
                let st = [GA_POINT_S0, GA_POINT_T0, GA_POINT_S1, GA_POINT_T1].map(|reg| f32::from_bits(self.regs[reg as usize / 4]));
                let mut stuff = Vec::new();
                let mut pcounts = counts;
                if gb & 1 != 0 {
                    for t in 0..8 {
                        if bits(gb, 16 + 2 * t, 2) != 0 {
                            stuff.push((t as usize, st));
                            pcounts[t as usize] = 2;
                        }
                    }
                }
                for v in &vertices {
                    let half = match v.psize {
                        Some(s) if s > 0.0 => (s / 2.0, s / 2.0),
                        _ => half,
                    };
                    r.point(vram, *v, half, &stuff, &pcounts);
                }
            }
            2 | 3 | 12 => {
                let width = (self.regs[GA_LINE_CNTL as usize / 4] & 0xffff) as f32 / 16.0;
                let half = width.max(0.5);
                let pairs: Vec<(usize, usize)> = match prim {
                    2 => (0..n / 2).map(|i| (2 * i, 2 * i + 1)).collect(),
                    3 => (1..n).map(|i| (i - 1, i)).collect(),
                    _ => (0..n).map(|i| (i, (i + 1) % n)).collect(),
                };
                for (a, b) in pairs {
                    r.line(vram, vertices[a], vertices[b], half, &counts);
                }
            }
            4 => (0..n / 3).for_each(|i| tri(&mut r, vram, 3 * i, 3 * i + 1, 3 * i + 2)),
            5 | 15 => (2..n).for_each(|i| tri(&mut r, vram, 0, i - 1, i)),
            6 => (2..n).for_each(|i| {
                if i % 2 == 0 {
                    tri(&mut r, vram, i - 2, i - 1, i)
                } else {
                    tri(&mut r, vram, i - 1, i - 2, i)
                }
            }),
            13 => (0..n / 4).for_each(|i| {
                tri(&mut r, vram, 4 * i, 4 * i + 1, 4 * i + 2);
                tri(&mut r, vram, 4 * i, 4 * i + 2, 4 * i + 3);
            }),
            14 => (1..n / 2).for_each(|i| {
                let (a, b, c, d) = (2 * i - 2, 2 * i - 1, 2 * i + 1, 2 * i);
                tri(&mut r, vram, a, b, c);
                tri(&mut r, vram, a, c, d);
            }),
            _ => {}
        }
        self.regs[ZB_ZPASS_DATA as usize / 4] = r.zpass;
        if r.pixels > 0 {
            self.vga.touch();
        }
    }
}

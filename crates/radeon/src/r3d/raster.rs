//! Setup, scan conversion and the back end: the rasterizer (RS) feeding the
//! fragment shader, the alpha test, the Z and stencil buffer (ZB) and the
//! colour buffer's blender and ROP (RB3D). Registers as in AMD's R3xx 3D
//! register reference.

use super::V4;
use super::tex::Texture;
use super::us::{self, Quad, TEMPS};
use crate::engine::rop3;

/// A vertex after the vertex shader and viewport transform: window x, y and
/// z, 1/w for perspective-correct interpolation, and the GA's fog value.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Vertex {
    pub pos: V4,
    pub colours: [V4; 4],
    pub tex: [V4; 8],
    pub psize: Option<f32>,
    pub fog: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ColourBuffer {
    pub offset: usize,
    pub pitch: usize,
    pub format: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DepthBuffer {
    pub offset: usize,
    pub pitch: usize,
    pub z16: bool,
}

const FOG: usize = 16 + 32;
const ATTRS: usize = FOG + 1;

pub(crate) struct Raster {
    pub colour: Vec<ColourBuffer>,
    pub depth: Option<DepthBuffer>,
    pub zb_cntl: u32,
    /// ZB_DEPTHCLEARVALUE while ZB_BW_CNTL.ZB_CB_CLEAR is set.
    pub cb_clear: Option<u32>,
    pub zs_cntl: u32,
    pub stencil_ref: u32,
    pub depth_scale: f32,
    pub depth_offset: f32,
    pub poly_offset: (u32, [f32; 4]),
    pub scissor: [i32; 4],
    pub clips: [[i32; 4]; 4],
    pub clip_rule: u32,
    pub blend: u32,
    pub ablend: u32,
    pub channel_mask: u32,
    pub constant: u32,
    pub rop: u32,
    pub alpha_func: u32,
    /// FG_FOG_BLEND, the fog colour and FG_FOG_FACTOR.
    pub fog: u32,
    pub fog_colour: [f32; 3],
    pub fog_factor: f32,
    pub cull: u32,
    pub colour_control: u32,
    pub clamp_colours: bool,
    pub solid: V4,
    pub out_fmt: u32,
    pub rs_inst: Vec<u32>,
    pub rs_ip: [u32; 8],
    pub tex_counts: [u32; 8],
    pub perspective: bool,
    pub us: us::Program,
    pub textures: [Option<Texture>; 16],
    pub zpass: u32,
    pub pixels: u64,
    /// When set, triangles are set up and appended here instead of drawn.
    pub capture: Option<Captured>,
}

/// Triangles set up for another rasterizer: per vertex, window x, y, z
/// (after polygon offset) and the 1/w the CPU path interpolates with, the
/// face and the fog value, and the pixel stack registers in `slots` as the
/// RS fills them.
#[derive(Default)]
pub(crate) struct Captured {
    pub slots: Vec<usize>,
    pub floats: Vec<f32>,
    pub vertices: usize,
}

impl Captured {
    pub fn new(rs_inst: &[u32]) -> Self {
        let mut slots = Vec::new();
        for &inst in rs_inst {
            if (inst >> 3) & 7 != 0 {
                slots.push(((inst >> 6) & 31) as usize);
            }
            if (inst >> 14) & 7 != 0 {
                slots.push(((inst >> 17) & 31) as usize);
            }
        }
        slots.sort_unstable();
        slots.dedup();
        Captured { slots, floats: Vec::new(), vertices: 0 }
    }

    /// Floats per vertex.
    pub fn stride(&self) -> usize {
        8 + 4 * self.slots.len()
    }
}

fn factor(f: u32, k: usize, s: V4, d: V4, cc: V4, alpha: bool) -> f32 {
    let (sa, da) = (s[3], d[3]);
    match f {
        1 | 32 => 0.0,
        2 | 33 => 1.0,
        3 | 34 => s[k],
        4 | 35 => 1.0 - s[k],
        5 | 38 | 12 => sa,
        6 | 39 | 13 => 1.0 - sa,
        7 | 40 => da,
        8 | 41 => 1.0 - da,
        9 | 36 => d[k],
        10 | 37 => 1.0 - d[k],
        11 | 42 => if alpha { 1.0 } else { sa.min(1.0 - da) },
        43 => cc[k],
        44 => 1.0 - cc[k],
        45 => cc[3],
        46 => 1.0 - cc[3],
        _ => 1.0,
    }
}

/// The destination factor that D3D_BOTHSRCALPHA and D3D_BOTHINVSRCALPHA
/// imply.
fn both_dst_factor(f_src: u32, s: V4) -> Option<f32> {
    match f_src {
        12 => Some(1.0 - s[3]),
        13 => Some(s[3]),
        _ => None,
    }
}

fn combine(cntl: u32, k: usize, s: V4, d: V4, cc: V4) -> f32 {
    let comb = (cntl >> 12) & 7;
    let (fs, fd) = ((cntl >> 16) & 63, (cntl >> 24) & 63);
    if comb == 4 {
        return s[k].min(d[k]);
    }
    if comb == 5 {
        return s[k].max(d[k]);
    }
    let alpha = k == 3;
    let a = s[k] * factor(fs, k, s, d, cc, alpha);
    let b = d[k] * both_dst_factor(fs, s).unwrap_or_else(|| factor(fd, k, s, d, cc, alpha));
    let v = match comb {
        0 | 1 => a + b,
        2 | 3 => a - b,
        _ => b - a,
    };
    if comb % 2 == 0 { v.clamp(0.0, 1.0) } else { v }
}

/// FG_ALPHA_FUNC orders its functions as OpenGL does (never, less, equal,
/// less or equal, greater, not equal, greater or equal, always), unlike the
/// Z and stencil functions `test` takes; AMD's R3xx 3D register reference.
const ALPHA_TO_Z_FUNC: [u32; 8] = [0, 1, 3, 2, 5, 6, 4, 7];

fn test(func: u32, v: u32, reference: u32) -> bool {
    match func {
        0 => false,
        1 => v < reference,
        2 => v <= reference,
        3 => v == reference,
        4 => v >= reference,
        5 => v > reference,
        6 => v != reference,
        _ => true,
    }
}

fn stencil_op(op: u32, s: u32, reference: u32) -> u32 {
    match op {
        1 => 0,
        2 => reference,
        3 => (s + 1).min(255),
        4 => s.saturating_sub(1),
        5 => !s & 0xff,
        6 => (s + 1) & 0xff,
        7 => s.wrapping_sub(1) & 0xff,
        _ => s,
    }
}

/// Bits of each component of a colour buffer format, C0 first.
fn widths(format: u32) -> &'static [u32] {
    match format {
        3 => &[5, 5, 5, 1],
        4 => &[5, 6, 5],
        9 => &[8],
        13 => &[8, 8],
        15 => &[4, 4, 4, 4],
        _ => &[8, 8, 8, 8],
    }
}

fn bytes_per_pixel(format: u32) -> usize {
    match format {
        9 => 1,
        3 | 4 | 13 | 15 => 2,
        _ => 4,
    }
}

fn read_px(vram: &[u8], a: usize, bpp: usize) -> u32 {
    let mut v = 0;
    for i in 0..bpp {
        v |= (*vram.get(a + i).unwrap_or(&0) as u32) << (8 * i);
    }
    v
}

fn write_px(vram: &mut [u8], a: usize, bpp: usize, v: u32) {
    for i in 0..bpp {
        if let Some(b) = vram.get_mut(a + i) {
            *b = (v >> (8 * i)) as u8;
        }
    }
}

/// The FG's fog blend. The GA's value is the share of fog colour for the
/// linear function and the exponent for the others, as the scale and offset
/// Mesa's classic r300 driver (Nicolai Hähnle, Jerome Glisse and others, MIT
/// license) programs for OpenGL's fog modes imply.
fn blend_fog(cntl: u32, colour: [f32; 3], factor: f32, c: V4, value: f32) -> V4 {
    if cntl & 1 == 0 {
        return c;
    }
    let amount = match (cntl >> 1) & 3 {
        0 => value,
        1 => 1.0 - (-16.0 * value).exp2(),
        2 => 1.0 - (-16.0 * value * value).exp2(),
        _ => factor,
    }
    .clamp(0.0, 1.0);
    [c[0] + (colour[0] - c[0]) * amount, c[1] + (colour[1] - c[1]) * amount, c[2] + (colour[2] - c[2]) * amount, c[3]]
}

impl Raster {
    fn inside_clip(&self, x: i32, y: i32) -> bool {
        let [x0, y0, x1, y1] = self.scissor;
        if x < x0 || x > x1 || y < y0 || y > y1 || x < 0 || y < 0 {
            return false;
        }
        if self.clip_rule == 0xffff {
            return true;
        }
        let mut n = 0;
        for (k, c) in self.clips.iter().enumerate() {
            if x >= c[0] && x <= c[2] && y >= c[1] && y <= c[3] {
                n |= 1 << k;
            }
        }
        self.clip_rule >> n & 1 != 0
    }

    pub fn bounds(&self) -> [i32; 4] {
        let [x0, y0, x1, y1] = self.scissor;
        [x0.max(0), y0.max(0), x1.min(4095), y1.min(4095)]
    }

    /// The rasterizer's packed texture coordinates for a vertex.
    fn pack(&self, v: &Vertex, counts: &[u32; 8]) -> [f32; 32] {
        let mut out = [0.0; 32];
        let mut at = 0;
        for (t, &n) in counts.iter().enumerate() {
            for c in 0..n.min(4) as usize {
                if at < 32 {
                    out[at] = v.tex[t][c];
                    at += 1;
                }
            }
        }
        out
    }

    fn attrs(&self, v: &Vertex, counts: &[u32; 8]) -> [f32; ATTRS] {
        let mut a = [0.0; ATTRS];
        for (i, c) in v.colours.iter().enumerate() {
            let c = if self.clamp_colours { c.map(|x| x.clamp(0.0, 1.0)) } else { *c };
            a[4 * i..4 * i + 4].copy_from_slice(&c);
        }
        a[16..FOG].copy_from_slice(&self.pack(v, counts));
        a[FOG] = v.fog;
        a
    }

    /// The pixel stack frame the RS instructions build from interpolated
    /// colours and texture coordinates.
    fn rs(&self, a: &[f32; ATTRS]) -> [V4; TEMPS] {
        let mut t = [[0.0; 4]; TEMPS];
        for &inst in &self.rs_inst {
            if (inst >> 3) & 7 != 0 {
                let ip = self.rs_ip[(inst & 7) as usize];
                let ptr = (ip & 63) as usize;
                let addr = ((inst >> 6) & 31) as usize;
                t[addr] = [0, 1, 2, 3].map(|c| match (ip >> (13 + 3 * c)) & 7 {
                    s @ 0..=3 => a.get(16 + ptr + s as usize).copied().unwrap_or(0.0),
                    5 => 1.0,
                    _ => 0.0,
                });
            }
            if (inst >> 14) & 7 != 0 {
                let ip = self.rs_ip[((inst >> 11) & 7) as usize];
                let col = ((ip >> 6) & 7).min(3) as usize;
                let c = [a[4 * col], a[4 * col + 1], a[4 * col + 2], a[4 * col + 3]];
                let addr = ((inst >> 17) & 31) as usize;
                t[addr] = match (ip >> 9) & 15 {
                    0 => c,
                    1 => [c[0], c[1], c[2], 0.0],
                    2 => [c[0], c[1], c[2], 1.0],
                    4 => [0.0, 0.0, 0.0, c[3]],
                    5 => [0.0; 4],
                    6 => [0.0, 0.0, 0.0, 1.0],
                    8 => [1.0, 1.0, 1.0, c[3]],
                    9 => [1.0, 1.0, 1.0, 0.0],
                    _ => [1.0; 4],
                };
            }
        }
        t
    }

    /// Fills a triangle. `counts` gives the texture components each vertex
    /// carries.
    pub fn triangle(&mut self, vram: &mut [u8], v: [Vertex; 3], counts: &[u32; 8], is_poly: bool) {
        let sub = 16.0;
        let p: [(i64, i64); 3] = v.map(|v| ((v.pos[0] * sub).round() as i64, (v.pos[1] * sub).round() as i64));
        let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
        if area == 0 || v.iter().any(|v| v.pos.iter().any(|c| !c.is_finite())) {
            return;
        }
        let visually_ccw = area < 0;
        let front = visually_ccw != (self.cull & 4 != 0);
        if is_poly && ((front && self.cull & 1 != 0) || (!front && self.cull & 2 != 0)) {
            return;
        }
        let mut attrs = v.map(|v| self.attrs(&v, counts));
        self.flat_shade(&mut attrs);
        let (v, p) = if area < 0 {
            attrs.swap(1, 2);
            ([v[0], v[2], v[1]], [p[0], p[2], p[1]])
        } else {
            (v, p)
        };
        let area = area.abs() as f32;
        let q = v.map(|v| if self.perspective { v.pos[3] } else { 1.0 });
        let z = v.map(|v| v.pos[2]);
        let z = self.offset_depth(z, &p, area, front);
        if self.capture.is_some() {
            let temps = attrs.map(|a| self.rs(&a));
            let cap = self.capture.as_mut().unwrap();
            for k in 0..3 {
                cap.floats.extend_from_slice(&[p[k].0 as f32 / 16.0, p[k].1 as f32 / 16.0, z[k], q[k]]);
                cap.floats.extend_from_slice(&[front as u32 as f32, attrs[k][FOG], 0.0, 0.0]);
                for &s in &cap.slots {
                    cap.floats.extend_from_slice(&temps[k][s]);
                }
            }
            cap.vertices += 3;
            return;
        }

        let [bx0, by0, bx1, by1] = self.bounds();
        let minx = (p.iter().map(|p| p.0).min().unwrap() >> 4).max(bx0 as i64) as i32 & !1;
        let miny = (p.iter().map(|p| p.1).min().unwrap() >> 4).max(by0 as i64) as i32 & !1;
        let maxx = (p.iter().map(|p| p.0).max().unwrap() >> 4).min(bx1 as i64) as i32;
        let maxy = (p.iter().map(|p| p.1).max().unwrap() >> 4).min(by1 as i64) as i32;
        if minx > maxx || miny > maxy {
            return;
        }
        let edge = |a: (i64, i64), b: (i64, i64), x: i64, y: i64| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
        let tie = |a: (i64, i64), b: (i64, i64)| {
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            dy < 0 || (dy == 0 && dx > 0)
        };
        let ties = [tie(p[1], p[2]), tie(p[2], p[0]), tie(p[0], p[1])];
        let mut y = miny;
        while y <= maxy {
            let mut x = minx;
            while x <= maxx {
                let mut cover = [false; 4];
                let mut bary = [[0.0f32; 3]; 4];
                for l in 0..4 {
                    let (px, py) = (x + (l & 1) as i32, y + (l >> 1) as i32);
                    let (sx, sy) = (px as i64 * 16 + 8, py as i64 * 16 + 8);
                    let e = [edge(p[1], p[2], sx, sy), edge(p[2], p[0], sx, sy), edge(p[0], p[1], sx, sy)];
                    let inside = (0..3).all(|i| e[i] > 0 || (e[i] == 0 && ties[i]));
                    cover[l] = inside && px <= maxx && py <= maxy && self.inside_clip(px, py);
                    bary[l] = e.map(|e| e as f32 / area);
                }
                if cover.iter().any(|&c| c) {
                    self.shade_quad(vram, x, y, cover, &bary, &attrs, &q, &z, front);
                }
                x += 2;
            }
            y += 2;
        }
    }

    fn flat_shade(&self, attrs: &mut [[f32; ATTRS]; 3]) {
        let provoking = match (self.colour_control >> 16) & 3 {
            0 => 0,
            1 => 1,
            _ => 2,
        };
        for c in 0..4 {
            for (part, range) in [(0, 0..3), (1, 3..4)] {
                let mode = (self.colour_control >> (4 * c + 2 * part)) & 3;
                for k in range {
                    let i = 4 * c + k;
                    match mode {
                        0 => {
                            for a in attrs.iter_mut() {
                                a[i] = self.solid[k];
                            }
                        }
                        1 => {
                            let v = attrs[provoking][i];
                            for a in attrs.iter_mut() {
                                a[i] = v;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn offset_depth(&self, z: [f32; 3], p: &[(i64, i64); 3], area: f32, front: bool) -> [f32; 3] {
        let (enable, [fs, fo, bs, bo]) = self.poly_offset;
        let on = if front { enable & 1 != 0 } else { enable & 2 != 0 };
        if !on {
            return z;
        }
        let (scale, offset) = if front { (fs, fo) } else { (bs, bo) };
        let (x1, y1) = ((p[1].0 - p[0].0) as f32, (p[1].1 - p[0].1) as f32);
        let (x2, y2) = ((p[2].0 - p[0].0) as f32, (p[2].1 - p[0].1) as f32);
        let (z1, z2) = (z[1] - z[0], z[2] - z[0]);
        let dzdx = ((z1 * y2 - z2 * y1) / area).abs();
        let dzdy = ((z2 * x1 - z1 * x2) / area).abs();
        let ds = if self.depth_scale != 0.0 { self.depth_scale } else { 16_777_215.0 };
        let delta = (scale * dzdx.max(dzdy) * ds + offset) / ds;
        z.map(|z| z + delta)
    }

    #[allow(clippy::too_many_arguments)]
    fn shade_quad(
        &mut self,
        vram: &mut [u8],
        x: i32,
        y: i32,
        cover: [bool; 4],
        bary: &[[f32; 3]; 4],
        attrs: &[[f32; ATTRS]; 3],
        q: &[f32; 3],
        z: &[f32; 3],
        front: bool,
    ) {
        let mut temps = [[[0.0; 4]; TEMPS]; 4];
        let mut depth = [0.0f32; 4];
        let mut fog = [0.0f32; 4];
        for l in 0..4 {
            let b = bary[l];
            let wsum = b[0] * q[0] + b[1] * q[1] + b[2] * q[2];
            let mut a = [0.0; ATTRS];
            for (i, v) in a.iter_mut().enumerate() {
                let n = b[0] * q[0] * attrs[0][i] + b[1] * q[1] * attrs[1][i] + b[2] * q[2] * attrs[2][i];
                *v = if wsum != 0.0 { n / wsum } else { 0.0 };
            }
            temps[l] = self.rs(&a);
            fog[l] = a[FOG];
            depth[l] = b[0] * z[0] + b[1] * z[1] + b[2] * z[2];
        }
        let out = {
            let mut quad = Quad { temps, textures: &self.textures, vram };
            self.us.run(&mut quad, false)
        };
        for l in 0..4 {
            if !cover[l] || out.killed[l] {
                continue;
            }
            let (px, py) = (x + (l & 1) as i32, y + (l >> 1) as i32);
            let z = out.depth[l].unwrap_or(depth[l]);
            let colour = blend_fog(self.fog, self.fog_colour, self.fog_factor, out.colour[l], fog[l]);
            self.backend(vram, px, py, colour, z, front);
        }
    }

    fn depth_stencil(&mut self, vram: &mut [u8], x: i32, y: i32, z: f32, front: bool) -> bool {
        let Some(db) = self.depth else { return true };
        if self.zb_cntl & 3 == 0 {
            return true;
        }
        let (bpp, a) = if db.z16 { (2, db.offset + (y as usize * db.pitch + x as usize) * 2) } else { (4, db.offset + (y as usize * db.pitch + x as usize) * 4) };
        if let Some(v) = self.cb_clear {
            write_px(vram, a, bpp, v);
            return true;
        }
        let old = read_px(vram, a, bpp);
        let ds = if self.depth_scale != 0.0 { self.depth_scale } else { 16_777_215.0 };
        let z24 = (z * ds + self.depth_offset).round().clamp(0.0, 16_777_215.0) as u32;
        let (zval, zold) = if db.z16 { (z24 >> 8, old & 0xffff) } else { (z24, old >> 8) };
        let z_on = self.zb_cntl & 2 != 0;
        let z_pass = !z_on || test(self.zs_cntl & 7, zval, zold);
        let stencil_on = self.zb_cntl & 1 != 0 && !db.z16;
        let mut new = old;
        let mut pass = z_pass;
        if stencil_on {
            let back = self.zb_cntl & (1 << 4) != 0 && !front;
            let shift = if back { 15 } else { 3 };
            let func = (self.zs_cntl >> shift) & 7;
            let (fail_op, zpass_op, zfail_op) = if back {
                ((self.zs_cntl >> 18) & 7, (self.zs_cntl >> 21) & 7, (self.zs_cntl >> 24) & 7)
            } else {
                ((self.zs_cntl >> 6) & 7, (self.zs_cntl >> 9) & 7, (self.zs_cntl >> 12) & 7)
            };
            let reference = self.stencil_ref & 0xff;
            let mask = (self.stencil_ref >> 8) & 0xff;
            let wmask = (self.stencil_ref >> 16) & 0xff;
            let s = old & 0xff;
            let s_pass = test(func, reference & mask, s & mask);
            let op = if !s_pass { fail_op } else if !z_pass { zfail_op } else { zpass_op };
            let ns = stencil_op(op, s, reference);
            new = (new & !wmask) | (ns & wmask);
            pass = s_pass && z_pass;
        }
        if pass && z_on && self.zb_cntl & 4 != 0 {
            new = if db.z16 { zval } else { (zval << 8) | (new & 0xff) };
        }
        if new != old {
            write_px(vram, a, bpp, new);
        }
        pass
    }

    fn backend(&mut self, vram: &mut [u8], x: i32, y: i32, colour: V4, z: f32, front: bool) {
        self.pixels += 1;
        if self.alpha_func & (1 << 11) != 0 {
            let a = (colour[3].clamp(0.0, 1.0) * 255.0).round() as u32;
            if !test(ALPHA_TO_Z_FUNC[((self.alpha_func >> 8) & 7) as usize], a, self.alpha_func & 0xff) {
                return;
            }
        }
        if !self.depth_stencil(vram, x, y, z, front) {
            return;
        }
        self.zpass = self.zpass.wrapping_add(1);
        let sel = [0, 1, 2, 3].map(|k| match (self.out_fmt >> (8 + 2 * k)) & 3 {
            0 => 3,
            s => s as usize - 1,
        });
        let src = sel.map(|i| colour[i].clamp(0.0, 1.0));
        let cc = {
            let c = self.constant;
            [0, 8, 16, 24].map(|s| ((c >> s) & 0xff) as f32 / 255.0)
        };
        for cb in self.colour.clone() {
            let bpp = bytes_per_pixel(cb.format);
            let a = cb.offset + (y as usize * cb.pitch + x as usize) * bpp;
            let w = widths(cb.format);
            let old = read_px(vram, a, bpp);
            let mut dst = [0.0, 0.0, 0.0, 1.0];
            let mut shift = 0;
            for (k, &bits) in w.iter().enumerate() {
                dst[k] = ((old >> shift) & ((1 << bits) - 1)) as f32 / ((1u32 << bits) - 1) as f32;
                shift += bits;
            }
            let mut out = src;
            if self.blend & 1 != 0 {
                for (k, o) in out.iter_mut().enumerate() {
                    let cntl = if k == 3 && self.blend & 2 != 0 { self.ablend } else { self.blend };
                    *o = combine(cntl, k, src, dst, cc);
                }
            }
            let mut packed = 0;
            let mut mask = 0;
            let mut shift = 0;
            for (k, &bits) in w.iter().enumerate() {
                let max = (1u32 << bits) - 1;
                let v = (out[k].clamp(0.0, 1.0) * max as f32).round() as u32;
                packed |= v << shift;
                if self.channel_mask & (1 << k) != 0 {
                    mask |= max << shift;
                }
                shift += bits;
            }
            if self.rop & 4 != 0 {
                let rop2 = ((self.rop >> 8) & 15) as u8;
                packed = rop3(rop2 | rop2 << 4, 0, packed, old);
            }
            let new = (old & !mask) | (packed & mask);
            if new != old {
                write_px(vram, a, bpp, new);
            }
        }
    }

    /// A point sprite or a wide point: a screen-aligned rectangle
    /// `half` pixels from the centre each way, with stuffed texture
    /// coordinates for the units in `stuff`.
    pub fn point(&mut self, vram: &mut [u8], v: Vertex, half: (f32, f32), stuff: &[(usize, [f32; 4])], counts: &[u32; 8]) {
        let (cx, cy) = (v.pos[0], v.pos[1]);
        let corner = |dx: f32, dy: f32, right: bool, bottom: bool| {
            let mut c = v;
            c.pos[0] = cx + dx;
            c.pos[1] = cy + dy;
            for &(unit, [s0, t0, s1, t1]) in stuff {
                c.tex[unit] = [if right { s1 } else { s0 }, if bottom { t0 } else { t1 }, 0.0, 1.0];
            }
            c
        };
        let tl = corner(-half.0, -half.1, false, false);
        let tr = corner(half.0, -half.1, true, false);
        let bl = corner(-half.0, half.1, false, true);
        let br = corner(half.0, half.1, true, true);
        self.triangle(vram, [tl, tr, br], counts, false);
        self.triangle(vram, [tl, br, bl], counts, false);
    }

    /// A line `half` pixels wide on each side of its axis.
    pub fn line(&mut self, vram: &mut [u8], a: Vertex, b: Vertex, half: f32, counts: &[u32; 8]) {
        let (dx, dy) = (b.pos[0] - a.pos[0], b.pos[1] - a.pos[1]);
        let (ox, oy) = if dx.abs() >= dy.abs() { (0.0, half) } else { (half, 0.0) };
        let shift = |v: Vertex, s: f32| {
            let mut v = v;
            v.pos[0] += ox * s;
            v.pos[1] += oy * s;
            v
        };
        let (a0, a1, b0, b1) = (shift(a, -1.0), shift(a, 1.0), shift(b, -1.0), shift(b, 1.0));
        self.triangle(vram, [a0, b0, b1], counts, false);
        self.triangle(vram, [a0, b1, a1], counts, false);
    }
}

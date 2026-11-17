//! The fragment shader unit (US), interpreted on 2x2 pixel quads so texture
//! lookups can take their level of detail from neighbouring pixels.
//!
//! The instruction words are described in AMD's R3xx 3D register reference.
//! How RGB and alpha operands pair up, which swizzles read the alpha sources,
//! and how nodes are laid out follow Mesa's r300 fragment program compiler
//! (r300_fragprog_emit.c, r300_fragprog_swizzle.c, radeon_pair_translate.c;
//! MIT license; Nicolai Hähnle, Jerome Glisse, Tom Stellard and others).

use super::V4;
use super::tex::Texture;

pub(crate) const TEMPS: usize = 32;
pub(crate) type Lanes<T> = [T; 4];

#[derive(Clone, Copy, Default)]
pub(crate) struct Node {
    pub alu: (usize, usize),
    pub tex: Option<(usize, usize)>,
}

#[derive(Clone, Default)]
pub(crate) struct Program {
    pub nodes: Vec<Node>,
    pub tex: Vec<u32>,
    /// RGB address, RGB instruction, alpha address, alpha instruction.
    pub alu: Vec<[u32; 4]>,
    pub consts: Vec<V4>,
}

/// What one quad leaves for the colour and depth buffers.
pub(crate) struct Output {
    pub colour: Lanes<V4>,
    pub depth: Lanes<Option<f32>>,
    pub killed: [bool; 4],
}

/// An S16E7 shader constant component.
pub(crate) fn s16e7(v: u32) -> f32 {
    let m = (v & 0xffff) as f32 / 65536.0;
    let e = ((v >> 16) & 0x7f) as i32;
    let s = if v & (1 << 23) != 0 { -1.0 } else { 1.0 };
    if e == 0 && m == 0.0 {
        return 0.0;
    }
    s * (1.0 + m) * (2f32).powi(e - 63)
}

fn modifier(v: f32, m: u32) -> f32 {
    match m {
        1 => -v,
        2 => v.abs(),
        3 => -v.abs(),
        _ => v,
    }
}

fn omod(v: f32, m: u32) -> f32 {
    match m {
        1 => v * 2.0,
        2 => v * 4.0,
        3 => v * 8.0,
        4 => v / 2.0,
        5 => v / 4.0,
        6 => v / 8.0,
        _ => v,
    }
}

/// The quad's state while a program runs.
pub(crate) struct Quad<'a> {
    pub temps: Lanes<[V4; TEMPS]>,
    pub textures: &'a [Option<Texture>; 16],
    pub vram: &'a [u8],
}

impl Quad<'_> {
    fn sample(&mut self, inst: u32, lod_bias_all: bool) {
        let src = (inst & 31) as usize;
        let dst = ((inst >> 6) & 31) as usize;
        let id = ((inst >> 11) & 15) as usize;
        let op = (inst >> 15) & 7;
        let coords: Lanes<V4> = [0, 1, 2, 3].map(|l| self.temps[l][src]);
        let Some(tex) = &self.textures[id] else {
            for l in 0..4 {
                self.temps[l][dst] = [0.0, 0.0, 0.0, 1.0];
            }
            return;
        };
        let st: Lanes<(f32, f32)> = coords.map(|c| {
            if op == 3 && c[3] != 0.0 { (c[0] / c[3], c[1] / c[3]) } else { (c[0], c[1]) }
        });
        let (w, h) = (tex.width as f32, tex.height as f32);
        let dx = ((st[1].0 - st[0].0) * w, (st[1].1 - st[0].1) * h);
        let dy = ((st[2].0 - st[0].0) * w, (st[2].1 - st[0].1) * h);
        let rho = (dx.0 * dx.0 + dx.1 * dx.1).max(dy.0 * dy.0 + dy.1 * dy.1);
        let mut lod = if rho > 0.0 { 0.5 * rho.log2() } else { 0.0 };
        if op == 4 || lod_bias_all {
            lod += coords[0][3];
        }
        for l in 0..4 {
            self.temps[l][dst] = tex.sample(self.vram, st[l].0, st[l].1, lod);
        }
    }
}

/// Picks an RGB operand. `rgb` and `alpha` are the three RGB and alpha
/// sources, `p` the pre-subtracted RGB and `pa` the pre-subtracted alpha.
fn rgb_arg(sel: u32, rgb: &[V4; 3], alpha: &[V4; 3], p: [f32; 3], pa: f32) -> [f32; 3] {
    let s = |i: usize| [rgb[i][0], rgb[i][1], rgb[i][2]];
    let rep = |v: f32| [v; 3];
    match sel {
        0 | 4 | 8 => s(sel as usize / 4),
        1..=3 | 5..=7 | 9..=11 => rep(rgb[sel as usize / 4][(sel as usize % 4) - 1]),
        12..=14 => rep(alpha[sel as usize - 12][3]),
        15 => p,
        16..=18 => rep(p[sel as usize - 16]),
        19 => rep(pa),
        20 => rep(0.0),
        21 => rep(1.0),
        22 => rep(0.5),
        23..=25 => {
            let v = s(sel as usize - 23);
            [v[1], v[2], v[0]]
        }
        26..=28 => {
            let v = s(sel as usize - 26);
            [v[2], v[0], v[1]]
        }
        _ => {
            let i = sel as usize - 29;
            [alpha[i][3], rgb[i][2], rgb[i][1]]
        }
    }
}

fn alpha_arg(sel: u32, rgb: &[V4; 3], alpha: &[V4; 3], p: [f32; 3], pa: f32) -> f32 {
    match sel {
        0..=8 => rgb[sel as usize / 3][sel as usize % 3],
        9..=11 => alpha[sel as usize - 9][3],
        12..=14 => p[sel as usize - 12],
        15 => pa,
        16 => 0.0,
        17 => 1.0,
        18 => 0.5,
        _ => 0.0,
    }
}

fn presub(op: u32, a: f32, b: f32) -> f32 {
    match op {
        0 => 1.0 - 2.0 * a,
        1 => b - a,
        2 => b + a,
        _ => 1.0 - a,
    }
}

struct AluResult {
    rgb: [f32; 3],
    alpha: f32,
}

fn alu(word: &[u32; 4], read: impl Fn(u32) -> V4) -> AluResult {
    let [rgb_addr, rgb_inst, a_addr, a_inst] = *word;
    let rgb: [V4; 3] = [0, 1, 2].map(|j| read((rgb_addr >> (6 * j)) & 63));
    let alpha: [V4; 3] = [0, 1, 2].map(|j| read((a_addr >> (6 * j)) & 63));
    let rp = (rgb_inst >> 21) & 3;
    let p = [0, 1, 2].map(|c| presub(rp, rgb[0][c], rgb[1][c]));
    let pa = presub((a_inst >> 21) & 3, alpha[0][3], alpha[1][3]);
    let args: [[f32; 3]; 3] = [0, 1, 2].map(|j| {
        let a = (rgb_inst >> (7 * j)) & 0x7f;
        rgb_arg(a & 31, &rgb, &alpha, p, pa).map(|v| modifier(v, a >> 5))
    });
    let aargs: [f32; 3] = [0, 1, 2].map(|j| {
        let a = (a_inst >> (7 * j)) & 0x7f;
        modifier(alpha_arg(a & 31, &rgb, &alpha, p, pa), a >> 5)
    });
    let [a, b, c] = args;
    let [aa, ab, ac] = aargs;
    let dp3 = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let rgb_op = (rgb_inst >> 23) & 15;
    let dp = match rgb_op {
        2 => dp3 + aa * ab,
        3 => a[0] * b[0] + a[1] * b[1] + c[2],
        _ => dp3,
    };
    let alpha = match (a_inst >> 23) & 15 {
        0 => aa * ab + ac,
        1 => dp,
        2 => aa.min(ab),
        3 => aa.max(ab),
        5 => if ac > 0.5 { aa } else { ab },
        6 => if ac >= 0.0 { aa } else { ab },
        7 => aa - aa.floor(),
        8 => aa.exp2(),
        9 => aa.log2(),
        10 => 1.0 / aa,
        11 => 1.0 / aa.abs().sqrt(),
        _ => 0.0,
    };
    let alpha = omod(alpha, (a_inst >> 27) & 7);
    let alpha = if a_inst & (1 << 30) != 0 { alpha.clamp(0.0, 1.0) } else { alpha };
    let rgb: [f32; 3] = match rgb_op {
        0 => [0, 1, 2].map(|i| a[i] * b[i] + c[i]),
        1..=3 => [dp; 3],
        4 => [0, 1, 2].map(|i| a[i].min(b[i])),
        5 => [0, 1, 2].map(|i| a[i].max(b[i])),
        7 => [0, 1, 2].map(|i| if c[i] > 0.5 { a[i] } else { b[i] }),
        8 => [0, 1, 2].map(|i| if c[i] >= 0.0 { a[i] } else { b[i] }),
        9 => a.map(|v| v - v.floor()),
        10 => [alpha; 3],
        _ => [0.0; 3],
    };
    let rgb = rgb.map(|v| omod(v, (rgb_inst >> 27) & 7));
    let rgb = if rgb_inst & (1 << 30) != 0 { rgb.map(|v| v.clamp(0.0, 1.0)) } else { rgb };
    AluResult { rgb, alpha }
}

impl Program {
    /// Runs the program on a quad and returns its colour, depth and kills.
    pub fn run(&self, q: &mut Quad<'_>, tex_lod_bias: bool) -> Output {
        let mut out = Output { colour: [[0.0; 4]; 4], depth: [None; 4], killed: [false; 4] };
        for node in &self.nodes {
            if let Some((start, len)) = node.tex {
                for i in start..start + len {
                    let Some(&inst) = self.tex.get(i) else { break };
                    if (inst >> 15) & 7 == 2 {
                        let src = (inst & 31) as usize;
                        for l in 0..4 {
                            if q.temps[l][src].iter().any(|&v| v < 0.0) {
                                out.killed[l] = true;
                            }
                        }
                        continue;
                    }
                    q.sample(inst, tex_lod_bias);
                }
            }
            let (start, len) = node.alu;
            for i in start..start + len {
                let Some(word) = self.alu.get(i) else { break };
                for l in 0..4 {
                    let temps = &q.temps[l];
                    let r = alu(word, |addr| {
                        if addr < 32 {
                            temps[addr as usize]
                        } else {
                            self.consts.get(addr as usize - 32).copied().unwrap_or([0.0; 4])
                        }
                    });
                    let [rgb_addr, _, a_addr, _] = *word;
                    let dst = ((rgb_addr >> 18) & 31) as usize;
                    let wmask = (rgb_addr >> 23) & 7;
                    let omask = (rgb_addr >> 26) & 7;
                    for c in 0..3 {
                        if wmask & (1 << c) != 0 {
                            q.temps[l][dst][c] = r.rgb[c];
                        }
                        if omask & (1 << c) != 0 {
                            out.colour[l][c] = r.rgb[c];
                        }
                    }
                    let adst = ((a_addr >> 18) & 31) as usize;
                    if a_addr & (1 << 23) != 0 {
                        q.temps[l][adst][3] = r.alpha;
                    }
                    if a_addr & (1 << 24) != 0 {
                        out.colour[l][3] = r.alpha;
                    }
                    if a_addr & (1 << 27) != 0 {
                        out.depth[l] = Some(r.alpha);
                    }
                }
            }
        }
        out
    }
}

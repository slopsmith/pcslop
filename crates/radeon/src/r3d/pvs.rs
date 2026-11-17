//! The programmable vertex shader (PVS), interpreted one vertex at a time.
//!
//! The instruction encoding and the meaning of each opcode follow Mesa's
//! r300 driver: r300_reg.h and compiler/r3xx_vertprog.c (MIT license; Nicolai
//! Hähnle, Ben Skeggs, Jerome Glisse, Corbin Simpson and others), and the
//! older classic r300_vertprog.c for LIT.
//!
//! Dual-math instructions, which XP's ATI driver emits and Mesa does not,
//! are decoded from that driver's programs: a vector instruction with
//! PVS_DST_DUAL_MATH_OP set also runs a math opcode on the third operand's
//! x. The opcode is DUAL_MATH_OP above the third operand's bits 25-26, and
//! bits 27-28 pick the component of alternate temporary 0 it writes.

use super::V4;

/// Where the 2.6.24 DRM and Mesa upload constants in vector memory.
pub(crate) const CONST_START: usize = 512;
pub(crate) const TEMPS: usize = 64;
pub(crate) const INPUTS: usize = 32;
pub(crate) const OUTPUTS: usize = 16;

pub(crate) struct Program<'a> {
    /// The VAP's vector memory, four dwords per vector.
    pub mem: &'a [u32],
    pub first: usize,
    pub last: usize,
    pub const_base: usize,
}

#[derive(Clone, Copy)]
struct Src(u32);

impl Src {
    fn class(self) -> u32 {
        self.0 & 3
    }
    fn index(self, a0: &[i32; 4]) -> usize {
        let base = ((self.0 >> 5) & 0xff) as i32;
        let rel = if self.0 & (1 << 4) != 0 { a0[(self.0 >> 29 & 3) as usize] } else { 0 };
        (base + rel).max(0) as usize
    }
}

struct Machine<'a> {
    prog: &'a Program<'a>,
    temps: [V4; TEMPS],
    alt: [V4; TEMPS],
    inputs: [V4; INPUTS],
    out: [V4; OUTPUTS],
    a0: [i32; 4],
}

impl Machine<'_> {
    fn constant(&self, i: usize) -> V4 {
        let v = (CONST_START + self.prog.const_base + i) * 4;
        match self.prog.mem.get(v..v + 4) {
            Some(d) => [0, 1, 2, 3].map(|c| f32::from_bits(d[c])),
            None => [0.0; 4],
        }
    }

    fn read(&self, s: Src) -> V4 {
        let i = s.index(&self.a0);
        let reg = match s.class() {
            0 => self.temps.get(i).copied(),
            1 => self.inputs.get(i).copied(),
            2 => Some(self.constant(i)),
            _ => self.alt.get(i).copied(),
        }
        .unwrap_or([0.0; 4]);
        let abs = s.0 & (1 << 3) != 0;
        [0, 1, 2, 3].map(|c| {
            let mut v = match (s.0 >> (13 + 3 * c)) & 7 {
                sw @ 0..=3 => reg[sw as usize],
                5 => 1.0,
                _ => 0.0,
            };
            if abs {
                v = v.abs();
            }
            if s.0 & (1 << (25 + c)) != 0 {
                v = -v;
            }
            v
        })
    }

    fn write(&mut self, d0: u32, v: V4) {
        let mut index = ((d0 >> 13) & 0x7f) as i32;
        if d0 & (1 << 31) != 0 {
            index += self.a0[(d0 >> 29 & 3) as usize];
        }
        let index = index.max(0) as usize;
        let mask = (d0 >> 20) & 0xf;
        let sat = if d0 & (1 << 6) != 0 { d0 & (1 << 25) != 0 } else { d0 & (1 << 24) != 0 };
        let v = if sat { v.map(|x| x.clamp(0.0, 1.0)) } else { v };
        let kind = (d0 >> 8) & 0xf;
        let reg = match kind {
            0 => self.temps.get_mut(index),
            1 => {
                for c in 0..4 {
                    if mask & (1 << c) != 0 {
                        self.a0[c] = v[c].floor() as i32;
                    }
                }
                return;
            }
            2 | 3 => self.out.get_mut(index),
            4 => self.alt.get_mut(index),
            5 => self.inputs.get_mut(index),
            _ => None,
        };
        let Some(reg) = reg else { return };
        for c in 0..4 {
            if mask & (1 << c) != 0 {
                reg[c] = if kind == 3 { v[0] } else { v[c] };
            }
        }
    }

    fn step(&mut self, d: &[u32]) {
        let d0 = d[0];
        let (a, b, c) = (self.read(Src(d[1])), self.read(Src(d[2])), self.read(Src(d[3])));
        let op = d0 & 0x3f;
        let dual = (d0 >> 27) & 3;
        let vector_op = d0 & (3 << 6) == 0;
        let side = (vector_op && dual != 0).then(|| {
            let x = self.read(Src(d[3] & !(0xf << 25)))[0];
            let me_op = dual << 2 | (d[3] >> 25) & 3;
            (math(me_op, [x; 4], [0.0; 4], [0.0; 4]), (d[3] >> 27) & 3)
        });
        let v = if d0 & (1 << 6) != 0 {
            math(op, a, b, c)
        } else if d0 & (1 << 7) != 0 {
            Some(if op == 1 { mad2x(a, b, c) } else { mad(a, b, c) })
        } else {
            vector(op, a, b, c)
        };
        if let Some(v) = v {
            self.write(d0, v);
        }
        if let Some((Some(m), comp)) = side {
            self.alt[0][comp as usize] = m[0];
        }
    }
}

fn zip(a: V4, b: V4, f: impl Fn(f32, f32) -> f32) -> V4 {
    [0, 1, 2, 3].map(|i| f(a[i], b[i]))
}

fn mad(a: V4, b: V4, c: V4) -> V4 {
    [0, 1, 2, 3].map(|i| a[i] * b[i] + c[i])
}

fn mad2x(a: V4, b: V4, c: V4) -> V4 {
    [0, 1, 2, 3].map(|i| 2.0 * a[i] * b[i] + c[i])
}

fn set(cond: bool) -> f32 {
    if cond { 1.0 } else { 0.0 }
}

fn vector(op: u32, a: V4, b: V4, c: V4) -> Option<V4> {
    Some(match op {
        1 => [a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3]; 4],
        2 => zip(a, b, |x, y| x * y),
        3 => zip(a, b, |x, y| x + y),
        4 => mad(a, b, c),
        5 => [1.0, a[1] * b[1], a[2], b[3]],
        6 => a.map(|x| x - x.floor()),
        7 => zip(a, b, f32::max),
        8 => zip(a, b, f32::min),
        9 => zip(a, b, |x, y| set(x >= y)),
        10 => zip(a, b, |x, y| set(x < y)),
        11 => mad2x(a, b, c),
        12 => zip(a, b, |x, y| (x * y).clamp(0.0, 1.0)),
        13 => a.map(f32::floor),
        14 => a.map(|x| (x + 0.5).floor()),
        25 => [0, 1, 2, 3].map(|i| if a[i] >= 0.0 { c[i] } else { b[i] }),
        26 => zip(a, b, |x, y| set(x > y)),
        27 => zip(a, b, |x, y| set(x == y)),
        28 => zip(a, b, |x, y| set(x != y)),
        _ => return None,
    })
}

/// LIT takes N.H from the first operand's w, N.L from the second's and the
/// exponent from the third's. Mesa's classic driver rotated one vector into
/// the three operands; XP's ATI driver passes three separate scalars.
fn lit(a: V4, b: V4, c: V4) -> V4 {
    let (ndoth, ndotl, exponent) = (a[3], b[3], c[3].clamp(-128.0, 128.0));
    let spec = if ndotl > 0.0 { ndoth.max(0.0).powf(exponent) } else { 0.0 };
    [1.0, ndotl.max(0.0), spec, 1.0]
}

fn math(op: u32, a: V4, b: V4, c: V4) -> Option<V4> {
    let x = a[0];
    let s = match op {
        1 => {
            let f = x.floor();
            return Some([f.exp2(), x - f, x.exp2(), 1.0]);
        }
        2 => {
            let l = x.abs().log2();
            let f = l.floor();
            return Some([f, x.abs() / f.exp2(), l, 1.0]);
        }
        3 => x.exp(),
        4 => return Some(lit(a, b, c)),
        5 | 13 | 14 | 15 => x.powf(c[0]),
        6 | 7 | 19 => {
            if op == 7 && x == 0.0 { f32::MAX } else { 1.0 / x }
        }
        8 | 9 | 20 => {
            let r = 1.0 / x.abs().sqrt();
            if op == 9 && r.is_infinite() { f32::MAX } else { r }
        }
        10 => x * b[0],
        11 => x.exp2(),
        12 | 18 => x.abs().log2(),
        16 => x.sin(),
        17 => x.cos(),
        _ => return None,
    };
    Some([s; 4])
}

impl Program<'_> {
    /// Runs the program on one vertex's inputs and returns its outputs.
    pub fn run(&self, inputs: &[V4; INPUTS]) -> [V4; OUTPUTS] {
        let mut m = Machine {
            prog: self,
            temps: [[0.0; 4]; TEMPS],
            alt: [[0.0; 4]; TEMPS],
            inputs: *inputs,
            out: [[0.0, 0.0, 0.0, 1.0]; OUTPUTS],
            a0: [0; 4],
        };
        let last = self.last.min(self.mem.len() / 4 - 1);
        for i in self.first..=last {
            m.step(&self.mem[4 * i..4 * i + 4]);
        }
        m.out
    }
}

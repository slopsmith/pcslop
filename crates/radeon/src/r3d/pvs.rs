//! The programmable vertex shader (PVS), interpreted one vertex at a time.
//!
//! The instruction encoding and the meaning of each opcode follow Mesa's
//! r300 driver: r300_reg.h and compiler/r3xx_vertprog.c (MIT license; Nicolai
//! Hähnle, Ben Skeggs, Jerome Glisse, Corbin Simpson and others), and the
//! older classic r300_vertprog.c for LIT.
//!
//! Dual-math instructions, which XP's ATI driver emits and Mesa does not,
//! follow AMD's R5xx Acceleration guide (7.5.6 and the "Dual Math
//! Instruction" table): a vector instruction with PVS_DST_DUAL_MATH_OP set
//! also runs a math opcode, which the third operand describes. Its two
//! scalar sources are the X and Y swizzles of that operand's register, and
//! it writes one component of alternate temporary 0 to 3.

use super::V4;

/// Where the 2.6.24 DRM and Mesa upload constants in vector memory.
pub(crate) const CONST_START: usize = 512;
/// R300's code memory is a ring of 256 instructions: XP's ATI driver
/// uploads a program that does not fit before slot 255 in two pieces, the
/// rest from slot 0, and sets PVS_FIRST_INST past PVS_LAST_INST.
pub(crate) const CODE_SLOTS: usize = 256;
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
        let vector_op = d0 & (3 << 6) == 0;
        let side = (vector_op && d0 & (1 << 28) != 0).then(|| self.dual_math(d0, d[3]));
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
        if let Some(Some((index, comp, m))) = side {
            self.alt[index][comp] = m;
        }
    }

    /// The math half of a dual-math instruction: its alternate temporary,
    /// the component it writes and the value.
    fn dual_math(&self, d0: u32, s: u32) -> Option<(usize, usize, f32)> {
        let src = Src(s);
        let i = src.index(&self.a0);
        let reg = match src.class() {
            0 => self.temps.get(i).copied(),
            1 => self.inputs.get(i).copied(),
            2 => Some(self.constant(i)),
            _ => self.alt.get(i).copied(),
        }
        .unwrap_or([0.0; 4]);
        let scalar = |k: u32| {
            let mut v = match (s >> (13 + 3 * k)) & 7 {
                sw @ 0..=3 => reg[sw as usize],
                5 => 1.0,
                _ => 0.0,
            };
            if s & (1 << 3) != 0 {
                v = v.abs();
            }
            if s & (1 << (25 + k)) != 0 {
                v = -v;
            }
            v
        };
        let (x, y) = (scalar(0), scalar(1));
        let op = (s >> 21) & 0xf | ((s >> 2) & 1) << 4;
        let mut m = math(op, [x; 4], [y; 4], [y; 4])?[0];
        if d0 & (1 << 25) != 0 {
            m = m.clamp(0.0, 1.0);
        }
        Some((((s >> 19) & 3) as usize, ((s >> 27) & 3) as usize, m))
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
        let (first, last) = (self.first % CODE_SLOTS, self.last % CODE_SLOTS);
        let mut i = first;
        loop {
            let Some(inst) = self.mem.get(4 * i..4 * i + 4) else { break };
            m.step(inst);
            if i == last {
                break;
            }
            i = (i + 1) % CODE_SLOTS;
        }
        m.out
    }
}

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

/// A source operand with its fields pulled out of the instruction word.
#[derive(Clone, Copy)]
pub(crate) struct Src {
    pub class: u8,
    pub base: i32,
    /// The address register component added to `base`, if relative.
    pub rel: Option<u8>,
    /// Per component: 0-3 a register component, 4 zero, 5 one.
    pub swizzle: [u8; 4],
    pub negate: [bool; 4],
    pub abs: bool,
}

impl Src {
    fn new(s: u32) -> Self {
        Src {
            class: (s & 3) as u8,
            base: ((s >> 5) & 0xff) as i32,
            rel: (s & (1 << 4) != 0).then_some(((s >> 29) & 3) as u8),
            swizzle: [0, 1, 2, 3].map(|c| match (s >> (13 + 3 * c)) & 7 {
                sw @ 0..=3 => sw as u8,
                5 => 5,
                _ => 4,
            }),
            negate: [0, 1, 2, 3].map(|c| s & (1 << (25 + c)) != 0),
            abs: s & (1 << 3) != 0,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Dst {
    /// PVS_DST_REG_TYPE: 0 temporary, 1 A0, 2 output, 3 output with x
    /// replicated, 4 alternate temporary, 5 input.
    pub kind: u8,
    pub base: i32,
    pub rel: Option<u8>,
    pub mask: u8,
    pub sat: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Engine {
    Vector(u32),
    Math(u32),
    Macro(u32),
}

/// The math half of a dual-math instruction: two scalars from `src`'s X
/// and Y swizzles, written to one component of an alternate temporary.
#[derive(Clone, Copy)]
pub(crate) struct DualMath {
    pub src: Src,
    pub op: u32,
    pub index: usize,
    pub comp: usize,
    pub sat: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct Inst {
    pub engine: Engine,
    pub src: [Src; 3],
    pub dst: Dst,
    pub dual: Option<DualMath>,
}

pub(crate) fn decode(d: &[u32]) -> Inst {
    let d0 = d[0];
    let op = d0 & 0x3f;
    let engine = if d0 & (1 << 6) != 0 {
        Engine::Math(op)
    } else if d0 & (1 << 7) != 0 {
        Engine::Macro(op)
    } else {
        Engine::Vector(op)
    };
    let dst = Dst {
        kind: ((d0 >> 8) & 0xf) as u8,
        base: ((d0 >> 13) & 0x7f) as i32,
        rel: (d0 & (1 << 31) != 0).then_some(((d0 >> 29) & 3) as u8),
        mask: ((d0 >> 20) & 0xf) as u8,
        sat: if d0 & (1 << 6) != 0 { d0 & (1 << 25) != 0 } else { d0 & (1 << 24) != 0 },
    };
    let s = d[3];
    let dual = (matches!(engine, Engine::Vector(_)) && d0 & (1 << 28) != 0).then(|| DualMath {
        src: Src::new(s),
        op: (s >> 21) & 0xf | ((s >> 2) & 1) << 4,
        index: ((s >> 19) & 3) as usize,
        comp: ((s >> 27) & 3) as usize,
        sat: d0 & (1 << 25) != 0,
    });
    Inst { engine, src: [Src::new(d[1]), Src::new(d[2]), Src::new(d[3])], dst, dual }
}

/// A program decoded once for a draw, with its constants as floats.
pub(crate) struct Decoded {
    pub insts: Vec<Inst>,
    pub consts: Vec<V4>,
}

struct Machine<'a> {
    prog: &'a Decoded,
    temps: [V4; TEMPS],
    alt: [V4; TEMPS],
    inputs: [V4; INPUTS],
    out: [V4; OUTPUTS],
    a0: [i32; 4],
}

impl Machine<'_> {
    fn index(&self, base: i32, rel: Option<u8>) -> usize {
        let rel = rel.map_or(0, |c| self.a0[c as usize]);
        base.saturating_add(rel).max(0) as usize
    }

    #[inline(always)]
    fn register(&self, s: &Src) -> V4 {
        let i = self.index(s.base, s.rel);
        match s.class {
            0 => self.temps.get(i).copied(),
            1 => self.inputs.get(i).copied(),
            2 => self.prog.consts.get(i).copied(),
            _ => self.alt.get(i).copied(),
        }
        .unwrap_or([0.0; 4])
    }

    #[inline(always)]
    fn component(s: &Src, reg: &V4, c: usize) -> f32 {
        let mut v = match s.swizzle[c] {
            sw @ 0..=3 => reg[sw as usize],
            5 => 1.0,
            _ => 0.0,
        };
        if s.abs {
            v = v.abs();
        }
        if s.negate[c] {
            v = -v;
        }
        v
    }

    #[inline(always)]
    fn read(&self, s: &Src) -> V4 {
        let reg = self.register(s);
        let mut v = [0.0; 4];
        for (c, x) in v.iter_mut().enumerate() {
            *x = Self::component(s, &reg, c);
        }
        v
    }

    fn write(&mut self, d: &Dst, v: V4) {
        let index = self.index(d.base, d.rel);
        let v = if d.sat { v.map(|x| x.clamp(0.0, 1.0)) } else { v };
        let reg = match d.kind {
            0 => self.temps.get_mut(index),
            1 => {
                for c in 0..4 {
                    if d.mask & (1 << c) != 0 {
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
            if d.mask & (1 << c) != 0 {
                reg[c] = if d.kind == 3 { v[0] } else { v[c] };
            }
        }
    }

    fn dual_math(&self, m: &DualMath) -> Option<f32> {
        let reg = self.register(&m.src);
        let (x, y) = (Self::component(&m.src, &reg, 0), Self::component(&m.src, &reg, 1));
        let r = math(m.op, [x; 4], [y; 4], [y; 4])?[0];
        Some(if m.sat { r.clamp(0.0, 1.0) } else { r })
    }

    fn step(&mut self, inst: &Inst) {
        let (a, b, c) = (self.read(&inst.src[0]), self.read(&inst.src[1]), self.read(&inst.src[2]));
        let side = inst.dual.and_then(|m| self.dual_math(&m).map(|v| (m.index, m.comp, v)));
        let v = match inst.engine {
            Engine::Math(op) => math(op, a, b, c),
            Engine::Macro(op) => Some(if op == 1 { mad2x(a, b, c) } else { mad(a, b, c) }),
            Engine::Vector(op) => vector(op, a, b, c),
        };
        if let Some(v) = v {
            self.write(&inst.dst, v);
        }
        if let Some((index, comp, m)) = side {
            self.alt[index][comp] = m;
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
    /// Decodes the instructions from PVS_FIRST_INST to PVS_LAST_INST, in the
    /// order they run, and the constants from the constant base on.
    pub fn decode(&self) -> Decoded {
        let (first, last) = (self.first % CODE_SLOTS, self.last % CODE_SLOTS);
        let mut insts = Vec::new();
        let mut i = first;
        loop {
            let Some(inst) = self.mem.get(4 * i..4 * i + 4) else { break };
            insts.push(decode(inst));
            if i == last {
                break;
            }
            i = (i + 1) % CODE_SLOTS;
        }
        let start = (CONST_START + self.const_base) * 4;
        let consts = self.mem.get(start..).unwrap_or(&[]).chunks_exact(4).map(|d| [0, 1, 2, 3].map(|c| f32::from_bits(d[c]))).collect();
        Decoded { insts, consts }
    }
}

impl Decoded {
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
        for inst in &self.insts {
            m.step(inst);
        }
        m.out
    }

    /// Like `run`, also describing what each instruction wrote.
    pub fn run_logged(&self, inputs: &[V4; INPUTS], log: &mut Vec<String>) -> [V4; OUTPUTS] {
        let mut m = Machine {
            prog: self,
            temps: [[0.0; 4]; TEMPS],
            alt: [[0.0; 4]; TEMPS],
            inputs: *inputs,
            out: [[0.0, 0.0, 0.0, 1.0]; OUTPUTS],
            a0: [0; 4],
        };
        for (i, inst) in self.insts.iter().enumerate() {
            let srcs: Vec<String> = inst
                .src
                .iter()
                .map(|s| format!("{}{}{}={:?}", ["t", "in", "c", "alt"][s.class as usize], s.base, s.rel.map_or(String::new(), |c| format!("+a0.{c}")), m.read(s)))
                .collect();
            m.step(inst);
            let d = &inst.dst;
            let now = match d.kind {
                0 => m.temps.get(m.index(d.base, d.rel)).copied(),
                2 | 3 => m.out.get(m.index(d.base, d.rel)).copied(),
                4 => m.alt.get(m.index(d.base, d.rel)).copied(),
                _ => None,
            };
            let engine = match inst.engine {
                Engine::Vector(o) => format!("vec{o}"),
                Engine::Math(o) => format!("math{o}"),
                Engine::Macro(o) => format!("macro{o}"),
            };
            log.push(format!("{i} {engine} dst kind{} {}.{:04b} = {now:?} a0 {:?} <- {}", d.kind, d.base, d.mask, m.a0, srcs.join(" ")));
        }
        m.out
    }
}

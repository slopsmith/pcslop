//! The second tier's IR: a region as a control-flow graph of blocks in SSA
//! form. Every block takes the eight guest registers and the x86 flags as
//! parameters and hands their values at its end to its successors.

use std::fmt;

pub type Val = u32;
pub type Blk = u32;

/// Number of block parameters: EAX..EDI, then the flags.
pub const PARAMS: usize = 9;
pub const FLAGS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Alu {
    Add,
    Or,
    Adc,
    Sbb,
    And,
    Sub,
    Xor,
    Shl,
    Shr,
    Sar,
}

impl Alu {
    /// The x86 group-1 operation with ModRM reg field `n`.
    pub fn group1(n: usize) -> Alu {
        [Alu::Add, Alu::Or, Alu::Adc, Alu::Sbb, Alu::And, Alu::Sub, Alu::Xor, Alu::Sub][n]
    }

    pub fn logic(self) -> bool {
        matches!(self, Alu::Or | Alu::And | Alu::Xor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    B,
    H,
    W,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Const(u32),
    /// Block parameter `n`.
    Param(u8),
    /// A 32-bit operation. ADC and SBB take the carry from the flags value `c`.
    Bin(Alu, Val, Val, Option<Val>),
    /// The x86 flags of `op` on `a` and `b`, with `c` as in `Bin` and the
    /// result `r`. 8- and 16-bit operations work on operands shifted left by
    /// `shift`, so NZCV come out right; `bits` are the flags anything reads.
    Flags { op: Alu, a: Val, b: Val, c: Option<Val>, r: Val, shift: u8, bits: u8 },
    /// Flags of INC or DEC: CF stays as in `old`.
    IncDecFlags { dec: bool, a: Val, r: Val, old: Val },
    /// 1 when x86 condition `cc` (the low nibble of a Jcc) holds for the flags.
    Cond(u8, Val),
    /// `v` with bits `lsb..lsb + width` sign- or zero-extended.
    Extract { v: Val, lsb: u8, width: u8, signed: bool },
    /// `dst` with bits `lsb..lsb + width` replaced by the low bits of `src`.
    Insert { dst: Val, src: Val, lsb: u8, width: u8 },
    Load { w: Width, addr: Val },
    Store { w: Width, addr: Val, val: Val },
    /// The FS segment base.
    FsBase,
}

impl Op {
    pub fn args(&self) -> Vec<Val> {
        match *self {
            Op::Const(_) | Op::Param(_) | Op::FsBase => vec![],
            Op::Bin(_, a, b, c) => [Some(a), Some(b), c].into_iter().flatten().collect(),
            Op::Flags { a, b, c, r, .. } => [Some(a), Some(b), c, Some(r)].into_iter().flatten().collect(),
            Op::IncDecFlags { a, r, old, .. } => vec![a, r, old],
            Op::Cond(_, f) => vec![f],
            Op::Extract { v, .. } => vec![v],
            Op::Insert { dst, src, .. } => vec![dst, src],
            Op::Load { addr, .. } => vec![addr],
            Op::Store { addr, val, .. } => vec![addr, val],
        }
    }

    pub fn has_effect(&self) -> bool {
        matches!(self, Op::Store { .. } | Op::Load { .. })
    }

    pub fn is_flags(&self) -> bool {
        matches!(self, Op::Flags { .. } | Op::IncDecFlags { .. })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Inst {
    pub op: Op,
    /// The x86 instruction it came from.
    pub eip: u32,
}

#[derive(Clone, Debug)]
pub enum Term {
    Jump(Blk),
    /// To `t` when `cond` (a `Cond` value) is 1, else to `f`.
    Branch { cond: Val, t: Blk, f: Blk },
    /// Leaves the region for tier 1 at `eip`.
    Exit(u32),
}

#[derive(Clone, Debug)]
pub struct Block {
    /// EIP of the first x86 instruction.
    pub eip: u32,
    /// x86 instructions in the block, for ICOUNT.
    pub insns: u32,
    pub params: [Val; PARAMS],
    pub insts: Vec<Val>,
    /// Values of the guest registers and flags at the end.
    pub outs: [Val; PARAMS],
    pub term: Term,
}

#[derive(Clone, Debug, Default)]
pub struct Func {
    pub insts: Vec<Inst>,
    pub blocks: Vec<Block>,
    /// For each guest memory access, the guest registers and flags as its x86
    /// instruction found them: what a fault there must leave in place.
    pub snaps: std::collections::HashMap<Val, [Val; PARAMS]>,
}

impl Func {
    pub fn push(&mut self, op: Op, eip: u32) -> Val {
        self.insts.push(Inst { op, eip });
        (self.insts.len() - 1) as Val
    }

    pub fn op(&self, v: Val) -> Op {
        self.insts[v as usize].op
    }

    pub fn succs(&self, b: Blk) -> Vec<Blk> {
        match self.blocks[b as usize].term {
            Term::Jump(t) => vec![t],
            Term::Branch { t, f, .. } => vec![t, f],
            Term::Exit(_) => vec![],
        }
    }
}

impl fmt::Display for Func {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.blocks.iter().enumerate() {
            writeln!(f, "b{i} @{:08x} params {:?}", b.eip, b.params)?;
            for &v in &b.insts {
                writeln!(f, "  v{v} = {:?}", self.op(v))?;
            }
            writeln!(f, "  outs {:?}", b.outs)?;
            writeln!(f, "  {:?}", b.term)?;
        }
        Ok(())
    }
}

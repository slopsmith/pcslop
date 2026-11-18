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

pub(crate) use super::super::x87::Src as FSrc;

/// The second operand of x87 arithmetic and compares: a stack slot or memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FOperand {
    Slot(u8),
    Mem(FSrc, Val),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Width {
    B,
    H,
    W,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    // x87 operations name stack slots relative to TOP at the region's entry.
    FLoad { slot: u8, src: FSrc, addr: Val },
    /// Stores a slot as a float or, for I32 and I16, rounded to an integer.
    FStore { slot: u8, dst: FSrc, addr: Val },
    FCopy { dst: u8, src: u8 },
    FConst { slot: u8, one: bool },
    /// Slot `dst` gets slot `a` and `b` combined by the ModRM operation `reg`.
    FArith { reg: u8, dst: u8, a: u8, b: FOperand },
    /// FCHS (0), FABS (1) or FSQRT (2) on a slot.
    FUnary { op: u8, slot: u8 },
    FXch { a: u8, b: u8 },
    /// Sets C0, C2 and C3 of the status word from comparing slot `a` with `b`.
    FCom { a: u8, b: FOperand },
    /// The x86 flags FCOMI sets from comparing slot `a` with slot `b`.
    FComi { a: u8, b: u8 },
    /// The status word.
    FSw,
    /// TOP is now `d` slots from where it was at entry; `set` and `clear`
    /// are slots that became full or empty.
    FTop { d: u8, set: u8, clear: u8 },
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
            Op::FLoad { addr, .. } | Op::FStore { addr, .. } => vec![addr],
            Op::FArith { b: FOperand::Mem(_, addr), .. } | Op::FCom { b: FOperand::Mem(_, addr), .. } => vec![addr],
            Op::FCopy { .. } | Op::FConst { .. } | Op::FArith { .. } | Op::FUnary { .. } | Op::FXch { .. } | Op::FCom { .. } | Op::FComi { .. } | Op::FSw | Op::FTop { .. } => vec![],
        }
    }

    /// Replaces every operand with `f(operand)`.
    pub fn map_args(&mut self, f: impl Fn(Val) -> Val) {
        match self {
            Op::Const(_) | Op::Param(_) | Op::FsBase => {}
            Op::Bin(_, a, b, c) => {
                *a = f(*a);
                *b = f(*b);
                if let Some(c) = c {
                    *c = f(*c);
                }
            }
            Op::Flags { a, b, c, r, .. } => {
                *a = f(*a);
                *b = f(*b);
                *r = f(*r);
                if let Some(c) = c {
                    *c = f(*c);
                }
            }
            Op::IncDecFlags { a, r, old, .. } => {
                *a = f(*a);
                *r = f(*r);
                *old = f(*old);
            }
            Op::Cond(_, fl) => *fl = f(*fl),
            Op::Extract { v, .. } => *v = f(*v),
            Op::Insert { dst, src, .. } => {
                *dst = f(*dst);
                *src = f(*src);
            }
            Op::Load { addr, .. } => *addr = f(*addr),
            Op::Store { addr, val, .. } => {
                *addr = f(*addr);
                *val = f(*val);
            }
            Op::FLoad { addr, .. } | Op::FStore { addr, .. } => *addr = f(*addr),
            Op::FArith { b: FOperand::Mem(_, addr), .. } | Op::FCom { b: FOperand::Mem(_, addr), .. } => *addr = f(*addr),
            _ => {}
        }
    }

    /// Whether two values with this operation and operands are always equal.
    pub fn pure(&self) -> bool {
        matches!(self, Op::Const(_) | Op::Bin(_, _, _, None) | Op::Extract { .. } | Op::Insert { .. } | Op::FsBase)
    }

    pub fn has_effect(&self) -> bool {
        matches!(self, Op::Store { .. } | Op::Load { .. }) || self.is_x87()
    }

    pub fn is_x87(&self) -> bool {
        matches!(
            self,
            Op::FLoad { .. } | Op::FStore { .. } | Op::FCopy { .. } | Op::FConst { .. } | Op::FArith { .. } | Op::FUnary { .. } | Op::FXch { .. } | Op::FCom { .. } | Op::FComi { .. } | Op::FSw | Op::FTop { .. }
        )
    }

    pub fn is_flags(&self) -> bool {
        matches!(self, Op::Flags { .. } | Op::IncDecFlags { .. } | Op::FComi { .. })
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
    /// x87 TOP relative to the entry's and the full slots, at the block's start.
    pub x87_in: (u8, u8),
    /// For an exit, the flags the code at its EIP may read before writing.
    pub exit_flags: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Func {
    pub insts: Vec<Inst>,
    pub blocks: Vec<Block>,
    /// For each guest memory access, the guest registers and flags as its x86
    /// instruction found them: what a fault there must leave in place.
    pub snaps: std::collections::HashMap<Val, [Val; PARAMS]>,
    pub x87: Option<X87Info>,
}

/// What a region with x87 code assumes and touches.
#[derive(Clone, Copy, Debug)]
pub struct X87Info {
    /// Slots full at entry, relative to TOP.
    pub entry_full: u8,
    /// Slots the region reads or writes.
    pub touched: u8,
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

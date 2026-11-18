//! Builds the IR of a region from x86 code: the blocks reachable from the
//! entry on its page, each ending at a branch, at an instruction the front
//! end does not handle (an exit to tier 1 there) or at the page end.

use std::collections::HashMap;

use a64::Width as AW;
use x86::decode::{Insn, MemOperand, Rep, decode};

use super::super::x87::{K, classify};
use super::ir::*;

const NO_REG: u8 = 0xff;
/// Limits on what one region takes in.
const MAX_BLOCKS: usize = 32;
const MAX_INSNS: usize = 512;
const MAX_BLOCK_INSNS: u32 = 64;

/// The region starting at `entry`, or None when its first instruction is
/// not one the front end handles.
/// `x87` is the control word and the slots full (relative to TOP) as the
/// region is entered, when x87 instructions may be compiled.
pub fn build(entry: u32, byte: &mut dyn FnMut(u32) -> Option<u8>, x87: Option<(u16, u8)>) -> Option<Func> {
    let page = entry & !0xfff;
    let mut f = Func::default();
    // Exceptions masked, as the region's entry checks.
    let x87 = x87.filter(|&(cw, _)| cw & 0x3f == 0x3f);
    f.x87 = x87.map(|(_, entry_full)| X87Info { entry_full, touched: 0 });
    let mut index: HashMap<u32, Blk> = HashMap::new();
    let mut work = vec![entry];
    let mut total = 0;
    index.insert(entry, 0);
    let mut first = empty(&mut f, entry);
    first.x87_in = (0, x87.map_or(0, |x| x.1));
    f.blocks.push(first);
    while let Some(eip) = work.pop() {
        let b = index[&eip];
        let (xd, xfull) = f.blocks[b as usize].x87_in;
        let mut sb = Builder { f: &mut f, cur: [0; PARAMS], blk: b, xd, xfull };
        sb.cur = sb.f.blocks[b as usize].params;
        let mut pc = eip;
        let mut n = 0u32;
        let term = loop {
            if n >= MAX_BLOCK_INSNS || total >= MAX_INSNS || pc & !0xfff != page {
                break Term::Exit(pc);
            }
            let Some(insn) = fetch(pc, page, byte) else { break Term::Exit(pc) };
            let next = pc.wrapping_add(insn.len as u32);
            if next.wrapping_sub(1) & !0xfff != page {
                break Term::Exit(pc);
            }
            match sb.insn(&insn, pc, next) {
                Flow::Next => {
                    n += 1;
                    total += 1;
                    pc = next;
                }
                Flow::Jump(t) => {
                    n += 1;
                    total += 1;
                    break sb.target(t, page, &mut index, &mut work).map_or(Term::Exit(t), Term::Jump);
                }
                Flow::Branch(cond, t) => {
                    n += 1;
                    total += 1;
                    let taken = sb.target(t, page, &mut index, &mut work);
                    let fall = sb.target(next, page, &mut index, &mut work);
                    break match (taken, fall) {
                        (Some(t), Some(fb)) => Term::Branch { cond, t, f: fb },
                        _ => {
                            // A side exit tier 1 handles: end here, before the branch.
                            n -= 1;
                            total -= 1;
                            sb.undo_to_insn_start();
                            Term::Exit(pc)
                        }
                    };
                }
                Flow::Unsupported => break Term::Exit(pc),
            }
        };
        let cur = sb.cur;
        let blk = &mut f.blocks[b as usize];
        blk.outs = cur;
        blk.insns = n;
        blk.term = term;
    }
    if f.blocks[0].insns == 0 {
        return None;
    }
    for b in &mut f.blocks {
        if let Term::Exit(t) = b.term {
            b.exit_flags = super::super::live_in(t, &mut |a| byte(a), page);
        }
    }
    Some(f)
}

fn empty(f: &mut Func, eip: u32) -> Block {
    let mut params = [0; PARAMS];
    for (i, p) in params.iter_mut().enumerate() {
        *p = f.push(Op::Param(i as u8), eip);
    }
    Block { eip, insns: 0, params, insts: Vec::new(), outs: params, term: Term::Exit(eip), x87_in: (0, 0), exit_flags: super::opt::ALL }
}

fn fetch(pc: u32, page: u32, byte: &mut dyn FnMut(u32) -> Option<u8>) -> Option<Insn> {
    let insn = decode::<()>(true, |i| {
        let a = pc.wrapping_add(i);
        if a & !0xfff != page {
            return Err(());
        }
        byte(a).ok_or(())
    })
    .ok()?;
    Some(insn)
}

enum Flow {
    Next,
    Jump(u32),
    /// A conditional branch to the target when the `Cond` value is 1.
    Branch(Val, u32),
    Unsupported,
}

struct Builder<'a> {
    f: &'a mut Func,
    blk: Blk,
    cur: [Val; PARAMS],
    /// x87 TOP relative to the entry's, and the full slots.
    xd: u8,
    xfull: u8,
}

impl Builder<'_> {
    fn push(&mut self, op: Op, eip: u32) -> Val {
        let v = self.f.push(op, eip);
        self.f.blocks[self.blk as usize].insts.push(v);
        v
    }

    /// Drops the values the current instruction added: a branch that turned
    /// out to leave the region adds only a `Cond`.
    fn undo_to_insn_start(&mut self) {
        let insts = &mut self.f.blocks[self.blk as usize].insts;
        if let Some(&v) = insts.last()
            && matches!(self.f.insts[v as usize].op, Op::Cond(..))
        {
            insts.pop();
        }
    }

    fn target(&mut self, t: u32, page: u32, index: &mut HashMap<u32, Blk>, work: &mut Vec<u32>) -> Option<Blk> {
        if t & !0xfff != page {
            return None;
        }
        if let Some(&b) = index.get(&t) {
            return (self.f.blocks[b as usize].x87_in == (self.xd, self.xfull)).then_some(b);
        }
        if self.f.blocks.len() >= MAX_BLOCKS {
            return None;
        }
        let b = self.f.blocks.len() as Blk;
        let mut blk = empty(self.f, t);
        blk.x87_in = (self.xd, self.xfull);
        self.f.blocks.push(blk);
        index.insert(t, b);
        work.push(t);
        Some(b)
    }

    fn k(&mut self, v: u32, eip: u32) -> Val {
        self.push(Op::Const(v), eip)
    }

    fn reg(&mut self, n: usize, size: u8, eip: u32) -> Val {
        let full = self.cur[n & if size == 1 { 3 } else { 7 }];
        match size {
            4 => full,
            2 => self.push(Op::Extract { v: full, lsb: 0, width: 16, signed: false }, eip),
            _ => self.push(Op::Extract { v: full, lsb: if n < 4 { 0 } else { 8 }, width: 8, signed: false }, eip),
        }
    }

    fn set_reg(&mut self, n: usize, size: u8, v: Val, eip: u32) {
        match size {
            4 => self.cur[n] = v,
            2 => self.cur[n] = self.push(Op::Insert { dst: self.cur[n], src: v, lsb: 0, width: 16 }, eip),
            _ => {
                let r = n & 3;
                self.cur[r] = self.push(Op::Insert { dst: self.cur[r], src: v, lsb: if n < 4 { 0 } else { 8 }, width: 8 }, eip);
            }
        }
    }

    fn ea(&mut self, insn: &Insn, m: &MemOperand, eip: u32) -> Option<Val> {
        if !insn.addr32 {
            return None;
        }
        let fs = match insn.seg_override {
            None | Some(0..=3) => false,
            Some(4) => true,
            _ => return None,
        };
        let mut a = None;
        if m.base != NO_REG {
            a = Some(self.cur[m.base as usize]);
        }
        if m.index != NO_REG {
            let mut i = self.cur[m.index as usize];
            if m.scale > 0 {
                let s = self.k(m.scale as u32, eip);
                i = self.push(Op::Bin(Alu::Shl, i, s, None), eip);
            }
            a = Some(match a {
                Some(b) => self.push(Op::Bin(Alu::Add, b, i, None), eip),
                None => i,
            });
        }
        let disp = self.k(m.disp, eip);
        let mut addr = match a {
            Some(b) if m.disp != 0 => self.push(Op::Bin(Alu::Add, b, disp, None), eip),
            Some(b) => b,
            None => disp,
        };
        if fs {
            let base = self.push(Op::FsBase, eip);
            addr = self.push(Op::Bin(Alu::Add, addr, base, None), eip);
        }
        Some(addr)
    }

    fn width(size: u8) -> Width {
        match size {
            1 => Width::B,
            2 => Width::H,
            _ => Width::W,
        }
    }

    fn load(&mut self, size: u8, addr: Val, eip: u32) -> Val {
        let v = self.push(Op::Load { w: Self::width(size), addr }, eip);
        self.f.snaps.insert(v, self.cur);
        v
    }

    fn store(&mut self, size: u8, addr: Val, val: Val, eip: u32) {
        let v = self.push(Op::Store { w: Self::width(size), addr, val }, eip);
        self.f.snaps.insert(v, self.cur);
    }

    /// The r/m operand of `insn`: a register, or a memory address.
    fn rm(&mut self, insn: &Insn, eip: u32) -> Option<Result<usize, Val>> {
        match insn.mem {
            None => Some(Ok(insn.rm())),
            Some(m) => self.ea(insn, &m, eip).map(Err),
        }
    }

    fn read_rm(&mut self, loc: Result<usize, Val>, size: u8, eip: u32) -> Val {
        match loc {
            Ok(r) => self.reg(r, size, eip),
            Err(addr) => self.load(size, addr, eip),
        }
    }

    fn write_rm(&mut self, loc: Result<usize, Val>, size: u8, v: Val, eip: u32) {
        match loc {
            Ok(r) => self.set_reg(r, size, v, eip),
            Err(addr) => self.store(size, addr, v, eip),
        }
    }

    /// `op` on `a` and `b` of `size` bytes: the result (narrow results are in
    /// the low bits) and new flags, with the result written by `write`.
    fn alu(&mut self, op: Alu, a: Val, b: Val, size: u8, write: impl FnOnce(&mut Self, Val), eip: u32) -> Option<()> {
        let c = matches!(op, Alu::Adc | Alu::Sbb).then_some(self.cur[FLAGS]);
        if c.is_some() && size != 4 {
            return None;
        }
        let (x, y) = if size == 4 {
            (a, b)
        } else {
            let sh = self.k(32 - 8 * size as u32, eip);
            (self.push(Op::Bin(Alu::Shl, a, sh, None), eip), self.push(Op::Bin(Alu::Shl, b, sh, None), eip))
        };
        let wide = self.push(Op::Bin(op, x, y, c), eip);
        let r = if size == 4 {
            wide
        } else {
            let sh = self.k(32 - 8 * size as u32, eip);
            self.push(Op::Bin(Alu::Shr, wide, sh, None), eip)
        };
        write(self, r);
        let shift = 32 - 8 * size;
        self.cur[FLAGS] = self.push(Op::Flags { op, a: x, b: y, c, r: wide, shift, bits: 0x3f }, eip);
        Some(())
    }

    fn insn(&mut self, insn: &Insn, eip: u32, next: u32) -> Flow {
        if insn.lock || insn.rep != Rep::None {
            return Flow::Unsupported;
        }
        match self.insn_inner(insn, eip, next) {
            Some(flow) => flow,
            None => Flow::Unsupported,
        }
    }

    fn insn_inner(&mut self, insn: &Insn, eip: u32, next: u32) -> Option<Flow> {
        let op = insn.op;
        if (0xd8..=0xdf).contains(&op) {
            return self.x87(insn, eip).map(|_| Flow::Next);
        }
        let size = |byte: bool| if byte { 1 } else if insn.op32 { 4 } else { 2 };
        match op {
            0x00..=0x3f if op & 7 < 6 => {
                let alu = Alu::group1((op >> 3) as usize & 7);
                let cmp = (op >> 3) & 7 == 7;
                let s = size(op & 1 == 0);
                match op & 7 {
                    0 | 1 | 2 | 3 => {
                        let loc = self.rm(insn, eip)?;
                        let regv = self.reg(insn.reg(), s, eip);
                        let rmv = self.read_rm(loc, s, eip);
                        let to_rm = op & 2 == 0;
                        let (a, b) = if to_rm { (rmv, regv) } else { (regv, rmv) };
                        let reg = insn.reg();
                        self.alu(alu, a, b, s, |e, r| {
                            if !cmp {
                                if to_rm { e.write_rm(loc, s, r, eip) } else { e.set_reg(reg, s, r, eip) }
                            }
                        }, eip)?;
                    }
                    _ => {
                        let a = self.reg(0, s, eip);
                        let b = self.k(if s == 1 { insn.imm & 0xff } else { insn.imm }, eip);
                        self.alu(alu, a, b, s, |e, r| if !cmp { e.set_reg(0, s, r, eip) }, eip)?;
                    }
                }
            }
            0x80 | 0x81 | 0x83 => {
                let s = size(op == 0x80);
                let imm = if op == 0x83 { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                let imm = if s == 2 { imm & 0xffff } else if s == 1 { imm & 0xff } else { imm };
                let loc = self.rm(insn, eip)?;
                let a = self.read_rm(loc, s, eip);
                let b = self.k(imm, eip);
                let cmp = insn.reg() == 7;
                self.alu(Alu::group1(insn.reg()), a, b, s, |e, r| if !cmp { e.write_rm(loc, s, r, eip) }, eip)?;
            }
            0x84 | 0x85 => {
                let s = size(op == 0x84);
                let loc = self.rm(insn, eip)?;
                let a = self.read_rm(loc, s, eip);
                let b = self.reg(insn.reg(), s, eip);
                self.alu(Alu::And, a, b, s, |_, _| {}, eip)?;
            }
            0xa8 | 0xa9 => {
                let s = size(op == 0xa8);
                let a = self.reg(0, s, eip);
                let b = self.k(if s == 1 { insn.imm & 0xff } else { insn.imm }, eip);
                self.alu(Alu::And, a, b, s, |_, _| {}, eip)?;
            }
            0x88..=0x8b => {
                let s = size(op & 1 == 0);
                let loc = self.rm(insn, eip)?;
                if op & 2 == 0 {
                    let v = self.reg(insn.reg(), s, eip);
                    self.write_rm(loc, s, v, eip);
                } else {
                    let v = self.read_rm(loc, s, eip);
                    self.set_reg(insn.reg(), s, v, eip);
                }
            }
            0xb0..=0xb7 => {
                let v = self.k(insn.imm & 0xff, eip);
                self.set_reg((op & 7) as usize, 1, v, eip);
            }
            0xb8..=0xbf if insn.op32 => {
                let v = self.k(insn.imm, eip);
                self.set_reg((op & 7) as usize, 4, v, eip);
            }
            0xc6 | 0xc7 => {
                if insn.reg() != 0 {
                    return None;
                }
                let s = size(op == 0xc6);
                let loc = self.rm(insn, eip)?;
                let v = self.k(insn.imm, eip);
                self.write_rm(loc, s, v, eip);
            }
            0x8d if insn.op32 => {
                let m = insn.mem?;
                if insn.seg_override.is_some() {
                    return None;
                }
                let a = self.ea(insn, &m, eip)?;
                self.set_reg(insn.reg(), 4, a, eip);
            }
            0x40..=0x4f if insn.op32 => {
                let r = (op & 7) as usize;
                let a = self.cur[r];
                self.inc_dec(op >= 0x48, a, |e, v| e.set_reg(r, 4, v, eip), eip);
            }
            0xfe | 0xff if insn.reg() < 2 => {
                let s = size(op == 0xfe);
                if s != 4 {
                    return None;
                }
                let loc = self.rm(insn, eip)?;
                let a = self.read_rm(loc, s, eip);
                self.inc_dec(insn.reg() == 1, a, |e, v| e.write_rm(loc, s, v, eip), eip);
            }
            0x50..=0x57 if insn.op32 => {
                let v = self.cur[(op & 7) as usize];
                self.push_val(v, eip);
            }
            0x68 | 0x6a if insn.op32 => {
                let imm = if op == 0x6a { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                let v = self.k(imm, eip);
                self.push_val(v, eip);
            }
            0x58..=0x5f if insn.op32 => {
                let esp = self.cur[4];
                let v = self.load(4, esp, eip);
                let four = self.k(4, eip);
                self.cur[4] = self.push(Op::Bin(Alu::Add, esp, four, None), eip);
                self.set_reg((op & 7) as usize, 4, v, eip);
            }
            0x1b6 | 0x1b7 | 0x1be | 0x1bf if insn.op32 => {
                let s = if op & 1 == 0 { 1 } else { 2 };
                let loc = self.rm(insn, eip)?;
                let v = self.read_rm(loc, s, eip);
                let v = if op >= 0x1be { self.push(Op::Extract { v, lsb: 0, width: 8 * s, signed: true }, eip) } else { v };
                self.set_reg(insn.reg(), 4, v, eip);
            }
            0x99 if insn.op32 => {
                let sh = self.k(31, eip);
                self.cur[2] = self.push(Op::Bin(Alu::Sar, self.cur[0], sh, None), eip);
            }
            0xf7 if insn.op32 && matches!(insn.reg(), 0 | 2 | 3) => {
                let loc = self.rm(insn, eip)?;
                let a = self.read_rm(loc, 4, eip);
                match insn.reg() {
                    0 => {
                        let b = self.k(insn.imm, eip);
                        self.alu(Alu::And, a, b, 4, |_, _| {}, eip)?;
                    }
                    2 => {
                        let ones = self.k(u32::MAX, eip);
                        let v = self.push(Op::Bin(Alu::Xor, a, ones, None), eip);
                        self.write_rm(loc, 4, v, eip);
                    }
                    _ => {
                        let zero = self.k(0, eip);
                        self.alu(Alu::Sub, zero, a, 4, |e, r| e.write_rm(loc, 4, r, eip), eip)?;
                    }
                }
            }
            0x190..=0x19f => {
                let c = self.push(Op::Cond((op & 15) as u8, self.cur[FLAGS]), eip);
                let loc = self.rm(insn, eip)?;
                self.write_rm(loc, 1, c, eip);
            }
            0x90 => {}
            0x11f => {}
            0x70..=0x7f | 0x180..=0x18f if insn.op32 => {
                let rel = if op < 0x100 { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                let c = self.push(Op::Cond((op & 15) as u8, self.cur[FLAGS]), eip);
                return Some(Flow::Branch(c, next.wrapping_add(rel)));
            }
            0xeb | 0xe9 if insn.op32 => {
                let rel = if op == 0xeb { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                return Some(Flow::Jump(next.wrapping_add(rel)));
            }
            _ => return None,
        }
        Some(Flow::Next)
    }

    fn slot(&self, i: u8) -> u8 {
        (self.xd + i) & 7
    }

    fn full(&self, i: u8) -> bool {
        self.xfull & 1 << self.slot(i) != 0
    }

    fn touch(&mut self, slots: &[u8]) {
        if let Some(x) = &mut self.f.x87 {
            for &s in slots {
                x.touched |= 1 << s;
            }
        }
    }

    fn x87_push(&mut self, eip: u32) -> u8 {
        self.xd = (self.xd + 7) & 7;
        let s = self.slot(0);
        self.xfull |= 1 << s;
        self.push(Op::FTop { d: self.xd, set: 1 << s, clear: 0 }, eip);
        s
    }

    fn x87_pop(&mut self, eip: u32) {
        let s = self.slot(0);
        self.xfull &= !(1 << s);
        self.xd = (self.xd + 1) & 7;
        self.push(Op::FTop { d: self.xd, set: 0, clear: 1 << s }, eip);
    }

    /// An x87 instruction whose stack state is known here: anything that
    /// would overflow or underflow the stack is left to tier 1.
    fn x87(&mut self, insn: &Insn, eip: u32) -> Option<()> {
        self.f.x87?;
        let k = classify(insn)?;
        let mem = |e: &mut Self| -> Option<Val> {
            let m = insn.mem?;
            e.ea(insn, &m, eip)
        };
        let snap = |e: &mut Self, v: Val| {
            e.f.snaps.insert(v, e.cur);
        };
        match k {
            K::LdMem(_) | K::Ild(_) => {
                let src = match k {
                    K::LdMem(src) => src,
                    K::Ild(AW::W) => FSrc::I32,
                    K::Ild(AW::H) => FSrc::I16,
                    _ => return None,
                };
                if self.full(7) {
                    return None;
                }
                let addr = mem(self)?;
                let s = self.slot(7);
                let v = self.push(Op::FLoad { slot: s, src, addr }, eip);
                snap(self, v);
                self.touch(&[s]);
                self.x87_push(eip);
            }
            K::StMem { double, pop } => {
                if !self.full(0) {
                    return None;
                }
                let addr = mem(self)?;
                let s = self.slot(0);
                let v = self.push(Op::FStore { slot: s, dst: if double { FSrc::F64 } else { FSrc::F32 }, addr }, eip);
                snap(self, v);
                self.touch(&[s]);
                if pop {
                    self.x87_pop(eip);
                }
            }
            K::Ist { w, pop } => {
                let dst = match w {
                    AW::W => FSrc::I32,
                    AW::H => FSrc::I16,
                    _ => return None,
                };
                if !self.full(0) {
                    return None;
                }
                let addr = mem(self)?;
                let s = self.slot(0);
                let v = self.push(Op::FStore { slot: s, dst, addr }, eip);
                snap(self, v);
                self.touch(&[s]);
                if pop {
                    self.x87_pop(eip);
                }
            }
            K::LdReg(i) => {
                let i = i as u8;
                if !self.full(i) || self.full(7) {
                    return None;
                }
                let (src, dst) = (self.slot(i), self.slot(7));
                self.push(Op::FCopy { dst, src }, eip);
                self.touch(&[src, dst]);
                self.x87_push(eip);
            }
            K::StReg { i, pop } => {
                let i = i as u8;
                if !self.full(0) {
                    return None;
                }
                let (src, dst) = (self.slot(0), self.slot(i));
                if src != dst {
                    self.push(Op::FCopy { dst, src }, eip);
                }
                self.touch(&[src, dst]);
                if !self.full(i) {
                    self.xfull |= 1 << dst;
                    self.push(Op::FTop { d: self.xd, set: 1 << dst, clear: 0 }, eip);
                }
                if pop {
                    self.x87_pop(eip);
                }
            }
            K::Xch(i) => {
                let i = i as u8;
                if !self.full(0) || !self.full(i) {
                    return None;
                }
                let (a, b) = (self.slot(0), self.slot(i));
                if a != b {
                    self.push(Op::FXch { a, b }, eip);
                }
                self.touch(&[a, b]);
            }
            K::Chs | K::Abs | K::Sqrt => {
                if !self.full(0) {
                    return None;
                }
                let s = self.slot(0);
                let op = match k {
                    K::Chs => 0,
                    K::Abs => 1,
                    _ => 2,
                };
                self.push(Op::FUnary { op, slot: s }, eip);
                self.touch(&[s]);
            }
            K::LdConst { mant } => {
                if self.full(7) {
                    return None;
                }
                let s = self.slot(7);
                self.push(Op::FConst { slot: s, one: mant != 0 }, eip);
                self.touch(&[s]);
                self.x87_push(eip);
            }
            K::Arith { reg, i, dst, pop } => {
                let (i, dst) = (i as u8, dst as u8);
                if !self.full(0) || !self.full(i) {
                    return None;
                }
                let (a, b, d) = (self.slot(0), self.slot(i), self.slot(dst));
                self.push(Op::FArith { reg: reg as u8, dst: d, a, b: FOperand::Slot(b) }, eip);
                self.touch(&[a, b, d]);
                if pop {
                    self.x87_pop(eip);
                }
            }
            K::ArithMem { reg, src } => {
                if !self.full(0) {
                    return None;
                }
                let addr = mem(self)?;
                let a = self.slot(0);
                let v = self.push(Op::FArith { reg: reg as u8, dst: a, a, b: FOperand::Mem(src, addr) }, eip);
                snap(self, v);
                self.touch(&[a]);
            }
            K::Com { i, pops } => {
                let i = i as u8;
                if !self.full(0) || !self.full(i) {
                    return None;
                }
                let (a, b) = (self.slot(0), self.slot(i));
                self.push(Op::FCom { a, b: FOperand::Slot(b) }, eip);
                self.touch(&[a, b]);
                for _ in 0..pops {
                    self.x87_pop(eip);
                }
            }
            K::ComMem { src, pop } => {
                if !self.full(0) {
                    return None;
                }
                let addr = mem(self)?;
                let a = self.slot(0);
                let v = self.push(Op::FCom { a, b: FOperand::Mem(src, addr) }, eip);
                snap(self, v);
                self.touch(&[a]);
                if pop {
                    self.x87_pop(eip);
                }
            }
            K::Comi { i, pop } => {
                let i = i as u8;
                if !self.full(0) || !self.full(i) {
                    return None;
                }
                let (a, b) = (self.slot(0), self.slot(i));
                self.cur[FLAGS] = self.push(Op::FComi { a, b }, eip);
                self.touch(&[a, b]);
                if pop {
                    self.x87_pop(eip);
                }
            }
            K::StswAx => {
                let v = self.push(Op::FSw, eip);
                self.set_reg(0, 2, v, eip);
            }
            _ => return None,
        }
        Some(())
    }

    fn inc_dec(&mut self, dec: bool, a: Val, write: impl FnOnce(&mut Self, Val), eip: u32) {
        let one = self.k(1, eip);
        let r = self.push(Op::Bin(if dec { Alu::Sub } else { Alu::Add }, a, one, None), eip);
        write(self, r);
        self.cur[FLAGS] = self.push(Op::IncDecFlags { dec, a, r, old: self.cur[FLAGS] }, eip);
    }

    fn push_val(&mut self, v: Val, eip: u32) {
        let esp = self.cur[4];
        let four = self.k(4, eip);
        let new = self.push(Op::Bin(Alu::Sub, esp, four, None), eip);
        self.store(4, new, v, eip);
        self.cur[4] = new;
    }
}

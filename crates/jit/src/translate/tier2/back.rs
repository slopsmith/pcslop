//! Lowers a region's IR to A64 inside a tier-1 `Emit`, whose exits, chains
//! and fault bookkeeping it reuses.
//!
//! At every block boundary and at every guest memory access the guest
//! registers are in their home registers (w19..w26) and live flags are in
//! tier 1's form (NZCV with CF inverted, PF and AF from w27 and w18), so a
//! fault anywhere is handled exactly as in a tier-1 block. Between those
//! points values live in temporaries, allocated block by block.

use std::collections::HashMap;

use a64::{Cond, Label, Logic, Reg, Size, Width as AW};

use super::super::{Emit, ICOUNT, x86_cond};
use super::ir::*;
use super::opt::{AF, CF, OF, PF, SF, ZF, cond_reads};
use crate::runtime::{AF_AUX, PF_RESULT, STATE, guest_reg};
use crate::state;

const NZCV_BITS: u8 = CF | ZF | SF | OF;
/// Temporaries the allocator hands out; x9 and x15..x16 stay free for
/// addresses and for tier 1's exit code.
const TEMPS: [u8; 13] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13];
const SCRATCH: Reg = Reg(9);
const SCRATCH2: Reg = Reg(14);

pub struct Lowered {
    pub boundaries: Vec<(u32, u32)>,
    pub insns: usize,
}

/// Emits `f` into `e`, or None when the allocator ran out of registers.
pub fn lower(e: &mut Emit, f: &Func) -> Option<Lowered> {
    let labels: Vec<Label> = (0..f.blocks.len()).map(|_| e.a.label()).collect();
    let mut boundaries = Vec::new();
    let mut insns = 0;
    for b in 0..f.blocks.len() {
        e.a.bind(labels[b]);
        let next = labels.get(b + 1).copied();
        BlockLower::new(e, f, b, &labels, next, &mut boundaries).run()?;
        insns += f.blocks[b].insns as usize;
    }
    Some(Lowered { boundaries, insns })
}

struct BlockLower<'a> {
    e: &'a mut Emit,
    f: &'a Func,
    b: usize,
    labels: &'a [Label],
    next: Option<Label>,
    boundaries: &'a mut Vec<(u32, u32)>,
    /// Index in the block of each value's last use; `END` for live-out.
    last: HashMap<Val, usize>,
    temp_of: HashMap<Val, Reg>,
    free: Vec<Reg>,
    home: [Val; 8],
    /// `Cond` values the branch at the end consumes from NZCV directly.
    fused_cond: Option<Val>,
    /// Bin results whose flags were set by the instruction computing them.
    flags_done: Vec<Val>,
    /// `base + constant` values used only as addresses: each access adds the
    /// constant itself, so they are never computed.
    folded: HashMap<Val, (Val, i32)>,
}

const END: usize = usize::MAX;

impl<'a> BlockLower<'a> {
    fn new(e: &'a mut Emit, f: &'a Func, b: usize, labels: &'a [Label], next: Option<Label>, boundaries: &'a mut Vec<(u32, u32)>) -> Self {
        let blk = &f.blocks[b];
        let mut home = [0; 8];
        home.copy_from_slice(&blk.params[..8]);
        BlockLower {
            e,
            f,
            b,
            labels,
            next,
            boundaries,
            last: HashMap::new(),
            temp_of: HashMap::new(),
            free: TEMPS.iter().rev().map(|&n| Reg(n)).collect(),
            home,
            fused_cond: None,
            flags_done: Vec::new(),
            folded: HashMap::new(),
        }
    }

    fn blk(&self) -> &'a Block {
        &self.f.blocks[self.b]
    }

    fn op(&self, v: Val) -> Op {
        self.f.op(v)
    }

    /// Finds address arithmetic the accesses can absorb (see `folded`).
    fn find_folds(&mut self) {
        let blk = self.blk();
        let mut uses: HashMap<Val, (usize, usize)> = HashMap::new();
        for &v in &blk.insts {
            let op = self.op(v);
            let addr = match op {
                Op::Load { addr, .. } | Op::Store { addr, .. } => Some(addr),
                _ => None,
            };
            for a in op.args() {
                let e = uses.entry(a).or_default();
                e.0 += 1;
                if Some(a) == addr && !matches!(op, Op::Store { val, .. } if val == a) {
                    e.1 += 1;
                }
            }
        }
        let escapes = |v: Val| blk.outs.contains(&v) || self.f.snaps.values().any(|s| s.contains(&v)) || matches!(blk.term, Term::Branch { cond, .. } if cond == v);
        for &v in &blk.insts {
            if let Op::Bin(Alu::Add, x, y, None) = self.op(v)
                && let Some(d) = self.konst(y)
                && (-256..4096).contains(&(d as i32))
                && uses.get(&v).is_some_and(|&(all, addr)| all == addr && all > 0)
                && !escapes(v)
                && self.konst(x).is_none()
            {
                self.folded.insert(v, (x, d as i32));
            }
        }
    }

    fn liveness(&mut self) {
        self.find_folds();
        let blk = self.blk();
        for (i, &v) in blk.insts.iter().enumerate() {
            for a in self.op(v).args() {
                let a = self.folded.get(&a).map_or(a, |&(x, _)| x);
                self.last.insert(a, i);
            }
            if let Some(snap) = self.f.snaps.get(&v) {
                for &s in &snap[..8] {
                    self.last.insert(s, i);
                }
            }
        }
        for &v in &blk.outs[..8] {
            self.last.insert(v, END);
        }
        if let Term::Branch { cond, .. } = blk.term {
            self.last.insert(cond, END);
            if let Some(&lastv) = blk.insts.last()
                && lastv == cond
                && x86_cond(self.cond_cc(cond) as u16).is_some()
                && blk.insts.iter().filter(|&&v| self.op(v).args().contains(&cond)).count() == 0
            {
                self.fused_cond = Some(cond);
            }
        }
    }

    fn cond_cc(&self, v: Val) -> u8 {
        match self.op(v) {
            Op::Cond(cc, _) => cc,
            _ => unreachable!("branch on a value that is not a condition"),
        }
    }

    fn run(mut self) -> Option<()> {
        self.liveness();
        let blk = self.blk();
        if blk.insns > 0 {
            self.e.a.add_imm(Size::X, ICOUNT, ICOUNT, blk.insns);
        }
        let mut eip = None;
        for (i, &v) in blk.insts.iter().enumerate() {
            let at = self.f.insts[v as usize].eip;
            if eip != Some(at) {
                eip = Some(at);
                self.boundaries.push((self.e.a.pos() as u32, at));
                self.e.at = at;
            }
            if let Some(snap) = self.f.snaps.get(&v) {
                let snap = *snap;
                for (r, &s) in snap[..8].iter().enumerate() {
                    self.commit(r, s, i)?;
                }
            }
            self.inst(v, i)?;
            self.release(i);
        }
        if eip.is_none() {
            self.boundaries.push((self.e.a.pos() as u32, blk.eip));
        }
        self.term()
    }

    fn release(&mut self, i: usize) {
        let dead: Vec<Val> = self.temp_of.keys().copied().filter(|v| self.last.get(v).is_none_or(|&l| l != END && l <= i)).collect();
        for v in dead {
            let r = self.temp_of.remove(&v).unwrap();
            self.free.push(r);
        }
    }

    fn alloc(&mut self, v: Val) -> Option<Reg> {
        let r = self.free.pop()?;
        self.temp_of.insert(v, r);
        Some(r)
    }

    /// The register holding `v`, materialising a constant into SCRATCH if
    /// `scratch` is given (it must not be reused before the use).
    fn get(&mut self, v: Val, scratch: Reg) -> Reg {
        if let Some(&r) = self.temp_of.get(&v) {
            return r;
        }
        if let Some(h) = self.home.iter().position(|&x| x == v) {
            return guest_reg(h);
        }
        match self.op(v) {
            Op::Const(c) => {
                self.e.a.mov_imm(Size::W, scratch, c as u64);
                scratch
            }
            Op::FsBase => {
                self.e.a.ldr(AW::W, scratch, STATE, state::FS_BASE as i32);
                scratch
            }
            other => panic!("tier 2 lost the value v{v} = {other:?}"),
        }
    }

    fn konst(&self, v: Val) -> Option<u32> {
        match self.op(v) {
            Op::Const(c) => Some(c),
            _ => None,
        }
    }

    /// Puts `v` into guest register `r`'s home before instruction `i`.
    fn commit(&mut self, r: usize, v: Val, i: usize) -> Option<()> {
        let old = self.home[r];
        if old == v {
            return Some(());
        }
        let elsewhere = self.temp_of.contains_key(&old) || self.home.iter().enumerate().any(|(h, &x)| h != r && x == old);
        let needed = self.last.get(&old).is_some_and(|&l| l == END || l >= i);
        if needed && !elsewhere && self.konst(old).is_none() {
            let t = self.alloc(old)?;
            self.e.a.mov(Size::W, t, guest_reg(r));
        }
        if let Some(c) = self.konst(v) {
            self.e.a.mov_imm(Size::W, guest_reg(r), c as u64);
        } else {
            let src = self.get(v, SCRATCH);
            if src != guest_reg(r) {
                self.e.a.mov(Size::W, guest_reg(r), src);
            }
        }
        self.home[r] = v;
        Some(())
    }

    fn commit_outs(&mut self) -> Option<()> {
        let outs = self.blk().outs;
        let n = self.blk().insts.len();
        for (r, &v) in outs[..8].iter().enumerate() {
            self.commit(r, v, n)?;
        }
        Some(())
    }

    /// The Flags value right after `bin` in the block that takes its result
    /// with NZCV demanded and nothing in between that forbids setting them early.
    fn fusable_flags(&self, bin: Val, i: usize) -> Option<Val> {
        let Op::Bin(bop, ba, bb, _) = self.op(bin) else { return None };
        let insts = &self.blk().insts;
        for (j, &v) in insts.iter().enumerate().skip(i + 1) {
            match self.op(v) {
                Op::Flags { r, op, a, b, .. } if r == bin && (op != bop || a != ba || b != bb) => return None,
                Op::Flags { r, bits, op, .. } if r == bin => {
                    let stores_between = insts[i + 1..j].iter().any(|&w| matches!(self.op(w), Op::Store { .. }));
                    return (bits & NZCV_BITS != 0 && !(stores_between && matches!(op, Alu::Adc | Alu::Sbb))).then_some(v);
                }
                op if op.is_flags() => return None,
                Op::Cond(..) => return None,
                _ => {}
            }
        }
        None
    }

    fn inst(&mut self, v: Val, i: usize) -> Option<()> {
        match self.op(v) {
            Op::Const(_) | Op::Param(_) | Op::FsBase => {}
            Op::Bin(op, a, b, c) => self.bin(v, i, op, a, b, c)?,
            Op::Flags { op, a, b, r, shift, bits, .. } => self.flags(v, op, a, b, r, shift, bits),
            Op::IncDecFlags { dec, a, r, .. } => self.inc_dec_flags(v, dec, a, r),
            Op::Cond(cc, _) => {
                if self.fused_cond != Some(v) {
                    let d = self.alloc(v)?;
                    self.cond_to_reg(cc, d);
                }
            }
            Op::Extract { v: src, lsb, width, signed } => {
                let s = self.get(src, SCRATCH);
                let d = self.alloc(v)?;
                if signed {
                    self.e.a.sbfx(Size::W, d, s, lsb as u32, width as u32);
                } else {
                    self.e.a.ubfx(Size::W, d, s, lsb as u32, width as u32);
                }
            }
            Op::Insert { dst, src, lsb, width } => {
                let s = self.get(src, SCRATCH2);
                let dreg = self.get(dst, SCRATCH);
                let d = self.alloc(v)?;
                self.e.a.mov(Size::W, d, dreg);
                self.e.a.bfi(Size::W, d, s, lsb as u32, width as u32);
            }
            Op::Load { w, addr } => {
                let (base, off) = self.address(addr);
                let d = self.alloc(v)?;
                self.e.a.ldst(true, aw(w), false, false, d, base, off);
            }
            Op::Store { w, addr, val } => {
                let s = self.get(val, SCRATCH2);
                let (base, off) = self.address(addr);
                self.e.a.ldst(false, aw(w), false, false, s, base, off);
            }
        }
        Some(())
    }

    /// Base register and offset for an access to `addr`, folding a small
    /// constant added to a register.
    fn address(&mut self, addr: Val) -> (Reg, i32) {
        if let Some(&(x, d)) = self.folded.get(&addr) {
            return (self.get(x, SCRATCH), d);
        }
        (self.get(addr, SCRATCH), 0)
    }

    fn bin(&mut self, v: Val, i: usize, op: Alu, a: Val, b: Val, c: Option<Val>) -> Option<()> {
        if self.folded.contains_key(&v) || (!self.last.contains_key(&v) && self.fusable_flags(v, i).is_none() && c.is_none()) {
            return Some(());
        }
        let fused = self.fusable_flags(v, i);
        let flags = fused.is_some();
        let cf = fused.is_some_and(|fl| matches!(self.op(fl), Op::Flags { bits, .. } if bits & CF != 0));
        if flags {
            self.flags_done.push(v);
        }
        let imm = self.konst(b);
        if let (Alu::Add | Alu::Sub, Some(k)) = (op, imm)
            && !flags
            && k > 0x8000_0000
            && k.wrapping_neg() < 4096
        {
            let x = self.get(a, SCRATCH);
            let d = self.alloc(v)?;
            if op == Alu::Add { self.e.a.sub_imm(Size::W, d, x, k.wrapping_neg()) } else { self.e.a.add_imm(Size::W, d, x, k.wrapping_neg()) }
            return Some(());
        }
        let x = self.get(a, SCRATCH);
        let d = self.alloc(v)?;
        let e = &mut self.e.a;
        match op {
            Alu::Add | Alu::Sub if imm.is_some_and(|k| k < 4096 || (k & 0xfff == 0 && k >> 12 < 4096)) => {
                let k = imm.unwrap();
                match (op, flags) {
                    (Alu::Add, true) => {
                        e.adds_imm(Size::W, d, x, k);
                        if cf {
                            e.cfinv();
                        }
                    }
                    (Alu::Add, false) => e.add_imm(Size::W, d, x, k),
                    (_, true) => e.subs_imm(Size::W, d, x, k),
                    _ => e.sub_imm(Size::W, d, x, k),
                }
            }
            Alu::And | Alu::Or | Alu::Xor if imm.is_some() && a64::encode_bitmask(imm.unwrap() as u64, Size::W).is_some() => {
                let kind = match op {
                    Alu::And => Logic::And,
                    Alu::Or => Logic::Orr,
                    _ => Logic::Eor,
                };
                let ok = e.logic_imm(kind, Size::W, d, x, imm.unwrap() as u64);
                assert!(ok, "an encodable logical immediate was refused");
                if flags {
                    e.tst(Size::W, d, d);
                    if cf {
                        e.cfinv();
                    }
                }
            }
            Alu::Shl | Alu::Shr | Alu::Sar if imm.is_some() => {
                let k = imm.unwrap() & 31;
                match op {
                    Alu::Shl => e.lsl_imm(Size::W, d, x, k),
                    Alu::Shr => e.lsr_imm(Size::W, d, x, k),
                    _ => e.asr_imm(Size::W, d, x, k),
                }
            }
            _ => {
                let y = self.get(b, SCRATCH2);
                let e = &mut self.e.a;
                match op {
                    Alu::Add if flags => {
                        e.adds(Size::W, d, x, y);
                        if cf {
                            e.cfinv();
                        }
                    }
                    Alu::Add => e.add(Size::W, d, x, y),
                    Alu::Sub if flags => e.subs(Size::W, d, x, y),
                    Alu::Sub => e.sub(Size::W, d, x, y),
                    Alu::And | Alu::Or | Alu::Xor => {
                        match op {
                            Alu::And => e.and(Size::W, d, x, y),
                            Alu::Or => e.orr(Size::W, d, x, y),
                            _ => e.eor(Size::W, d, x, y),
                        }
                        if flags {
                            e.tst(Size::W, d, d);
                            if cf {
                                e.cfinv();
                            }
                        }
                    }
                    Alu::Adc => {
                        e.cfinv();
                        if flags { e.adcs(Size::W, d, x, y) } else { e.adc(Size::W, d, x, y) }
                        e.cfinv();
                    }
                    Alu::Sbb => {
                        if flags { e.sbcs(Size::W, d, x, y) } else { e.sbc(Size::W, d, x, y) }
                    }
                    Alu::Shl => e.lslv(Size::W, d, x, y),
                    Alu::Shr => e.lsrv(Size::W, d, x, y),
                    Alu::Sar => e.asrv(Size::W, d, x, y),
                }
            }
        }
        Some(())
    }

    #[allow(clippy::too_many_arguments)]
    fn flags(&mut self, _v: Val, op: Alu, a: Val, b: Val, r: Val, shift: u8, bits: u8) {
        if bits == 0 {
            return;
        }
        if bits & NZCV_BITS != 0 && !self.flags_done.contains(&r) {
            // Set NZCV again from the operands, after a store that had to come first.
            let x = self.get(a, SCRATCH);
            let y = self.get(b, SCRATCH2);
            let e = &mut self.e.a;
            let cf = bits & CF != 0;
            match op {
                Alu::Add => {
                    e.adds(Size::W, Reg(31), x, y);
                    if cf {
                        e.cfinv();
                    }
                }
                Alu::Sub => e.subs(Size::W, Reg(31), x, y),
                Alu::Adc => {
                    e.cfinv();
                    e.adcs(Size::W, Reg(31), x, y);
                    e.cfinv();
                }
                Alu::Sbb => e.sbcs(Size::W, Reg(31), x, y),
                _ => {
                    let rr = self.get(r, SCRATCH);
                    let e = &mut self.e.a;
                    e.tst(Size::W, rr, rr);
                    if cf {
                        e.cfinv();
                    }
                }
            }
        }
        if bits & PF != 0 {
            let rr = self.get(r, SCRATCH);
            if shift > 0 {
                self.e.a.lsr_imm(Size::W, PF_RESULT, rr, shift as u32);
            } else {
                self.e.a.mov(Size::W, PF_RESULT, rr);
            }
        }
        if bits & AF != 0 {
            if op.logic() {
                self.e.a.mov(Size::W, AF_AUX, Reg(31));
            } else {
                let x = self.get(a, SCRATCH);
                let y = self.get(b, SCRATCH2);
                self.e.a.eor(Size::W, AF_AUX, x, y);
                let rr = self.get(r, SCRATCH);
                self.e.a.eor(Size::W, AF_AUX, AF_AUX, rr);
                if shift > 0 {
                    self.e.a.lsr_imm(Size::W, AF_AUX, AF_AUX, shift as u32);
                }
            }
        }
    }

    fn inc_dec_flags(&mut self, v: Val, dec: bool, a: Val, r: Val) {
        let bits = self.demanded(v);
        if bits == 0 {
            return;
        }
        if bits & NZCV_BITS != 0 {
            let x = self.get(a, SCRATCH);
            let e = &mut self.e.a;
            e.cset(Size::W, SCRATCH2, Cond::Hs);
            if dec { e.subs_imm(Size::W, Reg(31), x, 1) } else { e.adds_imm(Size::W, Reg(31), x, 1) }
            e.rmif(SCRATCH2, 63, 0b0010);
        }
        if bits & PF != 0 {
            let rr = self.get(r, SCRATCH);
            self.e.a.mov(Size::W, PF_RESULT, rr);
        }
        if bits & AF != 0 {
            let x = self.get(a, SCRATCH);
            let rr = self.get(r, SCRATCH2);
            self.e.a.eor(Size::W, AF_AUX, x, rr);
            let ok = self.e.a.logic_imm(Logic::Eor, Size::W, AF_AUX, AF_AUX, 1);
            assert!(ok, "eor with 1 must encode");
        }
    }

    /// Flags of INC/DEC values carry no `bits`; anything reading them counts.
    fn demanded(&self, v: Val) -> u8 {
        let mut bits = 0;
        for blk in &self.f.blocks {
            for &w in &blk.insts {
                match self.op(w) {
                    Op::Cond(cc, fl) if fl == v => bits |= cond_reads(cc),
                    Op::Bin(_, _, _, Some(c)) if c == v => bits |= CF,
                    Op::Flags { c: Some(c), bits: b, .. } if c == v && b != 0 => bits |= CF,
                    Op::IncDecFlags { old, .. } if old == v => bits |= CF,
                    _ => {}
                }
            }
            if blk.outs[FLAGS] == v {
                bits |= super::opt::ALL;
            }
        }
        bits
    }

    /// Sets `d` to 1 when x86 condition `cc` holds for the flags in tier 1's form.
    fn cond_to_reg(&mut self, cc: u8, d: Reg) {
        match x86_cond(cc as u16) {
            Some(c) => self.e.a.cset(Size::W, d, c),
            None => {
                self.e.parity_odd(d);
                self.e.a.and_imm1(d);
                if cc & 1 == 0 {
                    let ok = self.e.a.logic_imm(Logic::Eor, Size::W, d, d, 1);
                    assert!(ok, "eor with 1 must encode");
                }
            }
        }
    }

    /// Whether an edge to `t` must look for a pending interrupt first.
    fn backward(&self, t: Blk) -> bool {
        self.f.blocks[t as usize].eip <= self.blk().eip
    }

    /// Goes to block `t` from the end of this block.
    fn jump(&mut self, t: Blk) {
        if self.backward(t) {
            let slow = self.e.a.label();
            self.e.a.ldr(AW::W, Reg(16), STATE, state::PENDING as i32);
            self.e.a.cbnz(Size::W, Reg(16), slow);
            let eip = self.f.blocks[t as usize].eip;
            self.e.defer(move |e| {
                e.a.bind(slow);
                e.goto_dispatch(eip);
            });
        }
        if Some(self.labels[t as usize]) != self.next {
            self.e.a.b(self.labels[t as usize]);
        }
    }

    /// Branches to block `t` when `cond` (x86 condition `cc`, inverted with `flip`) holds.
    fn branch_to(&mut self, cond: Val, cc: u8, flip: bool, t: Blk) {
        let target = if self.backward(t) {
            let stub = self.e.a.label();
            let (label, eip) = (self.labels[t as usize], self.f.blocks[t as usize].eip);
            self.e.defer(move |e| {
                let slow = e.a.label();
                e.a.bind(stub);
                e.a.ldr(AW::W, Reg(16), STATE, state::PENDING as i32);
                e.a.cbnz(Size::W, Reg(16), slow);
                e.a.b(label);
                e.a.bind(slow);
                e.goto_dispatch(eip);
            });
            stub
        } else {
            self.labels[t as usize]
        };
        if self.fused_cond == Some(cond) {
            self.e.a.b_cond(x86_cond((cc ^ flip as u8) as u16).unwrap(), target);
        } else {
            let c = self.get(cond, SCRATCH);
            if flip { self.e.a.cbz(Size::W, c, target) } else { self.e.a.cbnz(Size::W, c, target) }
        }
    }

    fn term(&mut self) -> Option<()> {
        self.commit_outs()?;
        match self.blk().term.clone() {
            Term::Exit(eip) => self.e.goto(eip),
            Term::Jump(t) => self.jump(t),
            Term::Branch { cond, t, f } => {
                let cc = self.cond_cc(cond);
                if Some(self.labels[t as usize]) == self.next && !self.backward(t) {
                    self.branch_to(cond, cc, true, f);
                    self.jump(t);
                } else {
                    self.branch_to(cond, cc, false, t);
                    self.jump(f);
                }
            }
        }
        Some(())
    }
}

fn aw(w: Width) -> AW {
    match w {
        Width::B => AW::B,
        Width::H => AW::H,
        Width::W => AW::W,
    }
}

trait AndImm1 {
    fn and_imm1(&mut self, d: Reg);
}

impl AndImm1 for a64::Asm {
    fn and_imm1(&mut self, d: Reg) {
        let ok = self.logic_imm(Logic::And, Size::W, d, d, 1);
        assert!(ok, "and with 1 must encode");
    }
}

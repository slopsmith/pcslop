//! Translates basic blocks of 32-bit flat-mode x86 code to ARM64.
//!
//! Every x86 instruction finishes its memory accesses before it writes guest
//! registers, so a fault on any access leaves the x86 state of the
//! instruction's start (apart from NZCV, which a restart recomputes).

use a64::{Asm, Cond, Extend, Label, Logic, Narrow, Reg, Shift, Size, V, Width, ZR, r};
use x86::decode::{Insn, MemOperand, NO_REG, Rep, decode};

use crate::runtime::{AF_AUX, Entries, ICOUNT, PF_RESULT, STATE, guest_reg};
use crate::layout;
use crate::state::{self, reason};

mod flags;
mod sse;
mod system;
pub mod tier2;
mod x87;

pub use x87::Snap as X87Snap;

const MAX_INSNS: usize = 64;

const T_ADDR: Reg = Reg(9);
const T_A: Reg = Reg(10);
const T_RES: Reg = Reg(11);
const T_B: Reg = Reg(12);
const T_C: Reg = Reg(13);
const T_D: Reg = Reg(14);
const T_E: Reg = Reg(15);
const CHAIN_TMP: Reg = Reg(16);
/// The AArch64 link register: a guest CALL's `bl` leaves its return path here.
const RA: Reg = Reg(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ow {
    B,
    H,
    D,
}

impl Ow {
    fn of(op: u16, op32: bool) -> Ow {
        if op & 1 == 0 {
            Ow::B
        } else if op32 {
            Ow::D
        } else {
            Ow::H
        }
    }

    fn mem(self) -> Width {
        match self {
            Ow::B => Width::B,
            Ow::H => Width::H,
            Ow::D => Width::W,
        }
    }

    fn mask(self) -> u64 {
        match self {
            Ow::B => 0xff,
            Ow::H => 0xffff,
            Ow::D => 0xffff_ffff,
        }
    }
}

pub struct Compiled {
    pub code: Vec<u32>,
    /// (ARM word index, x86 EIP) at the start of each translated instruction.
    pub boundaries: Vec<(u32, u32)>,
    pub insns: usize,
    /// (ARM word index of a patchable `b`, target EIP on the same page).
    pub chains: Vec<(u32, u32)>,
    /// The block contains a REP string instruction.
    pub has_rep: bool,
    /// The block uses a translation from the latest batch (SLOPPER_JIT_VERIFY=new).
    pub recent: bool,
    /// The block leaves for the interpreter after this many instructions.
    pub interp_after: Option<usize>,
    /// Its first instruction runs onto a page that was not mapped; translate
    /// it again once the interpreter has run it.
    pub retry: bool,
    /// The x87 state held in registers at instruction boundaries (ARM word index).
    pub x87: Vec<(u32, X87Snap)>,
    /// Whether a jump back to the start keeps x87 values in registers.
    pub x87_loop: bool,
    /// Counted exits' targets; counter 0 counts entries, counter k+1 exit k.
    pub exits: Vec<u32>,
}

impl Compiled {
    /// Appends `tail`, translated for the address right after this code.
    pub fn append(mut self, tail: Compiled) -> Compiled {
        let off = self.code.len() as u32;
        self.code.extend(tail.code);
        self.boundaries.extend(tail.boundaries.into_iter().map(|(w, e)| (w + off, e)));
        self.chains.extend(tail.chains.into_iter().map(|(w, e)| (w + off, e)));
        self.x87.extend(tail.x87.into_iter().map(|(w, s)| (w + off, s)));
        self.has_rep |= tail.has_rep;
        self.x87_loop |= tail.x87_loop;
        self
    }
}

struct Emit {
    a: Asm,
    block_va: u64,
    dispatch_va: u64,
    exit_va: u64,
    unstale_va: u64,
    /// Linear page of the block start; jumps within it may be chained.
    page: Option<u32>,
    chains: Vec<(u32, u32)>,
    /// Translating CPL 3 code: privileged instructions go to the interpreter.
    user: bool,
    /// Set by `store`, so a self-checking block can see writes near its own code.
    stored: bool,
    /// The `flags` bits the current instruction must produce.
    need: u8,
    x87: x87::X87Block,
    /// Whether a conditional branch may leave through a side exit and go on.
    superblock: bool,
    /// The memory operand `load` and `store` use instead of T_ADDR, set by `ea_fold`.
    fold: Option<Fold>,
    /// Whether `ea_fold` may fold: self-checking blocks need T_ADDR after a store.
    fold_ok: bool,
    /// EIP of the instruction being translated.
    at: u32,
    lp: X87Loop,
    /// Address of the block's first execution counter, when counting.
    counters: Option<u64>,
    /// Targets of the exits counted after the entry counter, in counter order.
    exits: Vec<u32>,
    /// Rarely run code to emit after the block's hot path, with the code
    /// position and x87 state when it was deferred (see `defer`).
    cold: Vec<(usize, x87::X87Block, Cold)>,
    /// Each instruction's first code word and how much cold code preceded it.
    starts: Vec<(usize, usize)>,
}

type Cold = Box<dyn FnOnce(&mut Emit)>;

/// A block that jumps back to its own start with x87 values in registers.
#[derive(Default)]
struct X87Loop {
    start: u32,
    /// Where the body starts and the x87 state it was translated for.
    head: Option<(a64::Label, x87::X87Block)>,
    /// The state at the first balanced jump back to `start`.
    seen: Option<x87::X87Block>,
    /// ICOUNT adds on jumps to `head`, which skip the block's first one.
    counts: Vec<usize>,
}

/// `[base + disp]`. An index is not folded: x86 code often holds negative
/// offsets in it, which would pass 4 GiB every time.
#[derive(Clone, Copy)]
struct Fold(Reg, i32);

enum Step {
    Next,
    /// A conditional branch left through a side exit; the block goes on after it.
    Branch,
    End,
    Unsupported,
    /// STI: the next instruction must run before the dispatcher can take an interrupt.
    Sti,
}

fn x86_cond(cc: u16) -> Option<Cond> {
    Some(match cc & 0xf {
        0x0 => Cond::Vs,
        0x1 => Cond::Vc,
        0x2 => Cond::Lo,
        0x3 => Cond::Hs,
        0x4 => Cond::Eq,
        0x5 => Cond::Ne,
        0x6 => Cond::Ls,
        0x7 => Cond::Hi,
        0x8 => Cond::Mi,
        0x9 => Cond::Pl,
        0xc => Cond::Lt,
        0xd => Cond::Ge,
        0xe => Cond::Le,
        0xf => Cond::Gt,
        _ => return None,
    })
}

impl Emit {
    fn branch_offset(&self, target_va: u64) -> i32 {
        ((target_va as i64 - (self.block_va + self.a.pos() as u64 * 4) as i64) / 4) as i32
    }

    /// Adds one to execution counter `n` of the block, when counting.
    fn bump(&mut self, n: usize) {
        let Some(base) = self.counters else { return };
        let va = base + 4 * n as u64;
        let pc = self.block_va + 4 * self.a.pos() as u64;
        self.a.adrp_offset(CHAIN_TMP, (va >> 12) as i64 - (pc >> 12) as i64);
        let lo = (va & 0xfff) as i32;
        self.a.ldr(Width::W, T_E, CHAIN_TMP, lo);
        self.a.add_imm(Size::W, T_E, T_E, 1);
        self.a.str(Width::W, T_E, CHAIN_TMP, lo);
    }

    fn goto(&mut self, eip: u32) {
        if self.counters.is_some() {
            self.exits.push(eip);
            self.bump(self.exits.len());
        }
        if eip == self.lp.start && self.page.is_some() {
            if let Some((head, h)) = self.lp.head
                && self.x87.fits(&h)
            {
                let slow = self.a.label();
                self.a.ldr(Width::W, CHAIN_TMP, STATE, state::PENDING as i32);
                self.a.cbnz(Size::W, CHAIN_TMP, slow);
                self.x87_loop_back(&h);
                self.bump(0);
                self.lp.counts.push(self.a.pos());
                self.a.nop();
                self.a.b(head);
                self.a.bind(slow);
            } else if self.lp.seen.is_none() && self.x87.balanced() {
                self.lp.seen = Some(self.x87);
            }
        }
        self.x87_flush();
        if self.page == Some(eip & !0xfff) {
            let slow = self.a.label();
            // Only a backward jump can close a loop of chained blocks.
            if eip <= self.at {
                self.a.ldr(Width::W, CHAIN_TMP, STATE, state::PENDING as i32);
                self.a.cbnz(Size::W, CHAIN_TMP, slow);
            }
            self.chains.push((self.a.pos() as u32, eip));
            self.a.b(slow);
            self.a.bind(slow);
        } else if self.page.is_some() {
            // Another page cannot be chained, but a probe of its own predicts better than the dispatcher.
            return self.flushed(|e| {
                e.a.mov_imm(Size::W, r(0), eip as u64);
                e.goto_indirect(r(0));
            });
        }
        self.flushed(|e| e.goto_dispatch(eip));
    }

    /// Ends a CALL returning to `next`, pushing the prediction `ret_predicted` checks.
    fn call_predicted(&mut self, next: u32, jump: impl FnOnce(&mut Self)) {
        if self.page.is_none() {
            return jump(self);
        }
        self.x87_flush();
        self.flushed(|e| {
            let go = e.a.label();
            e.a.bl(go);
            e.goto(next);
            e.a.bind(go);
            let (top, slot, tag) = (T_B, T_D, T_E);
            e.a.ldr(Width::W, top, STATE, state::RS_TOP as i32);
            e.a.add_imm(Size::W, top, top, 16);
            let ok = e.a.logic_imm(Logic::And, Size::W, top, top, layout::RSTACK_BYTES as u64 - 1);
            assert!(ok);
            e.a.str(Width::W, top, STATE, state::RS_TOP as i32);
            e.a.add_imm(Size::X, slot, STATE, (layout::RSTACK - layout::STATE) as u32);
            e.a.add(Size::X, slot, slot, top);
            e.a.ldr(Width::W, top, STATE, state::VEPOCH as i32);
            e.a.mov_imm(Size::X, tag, next as u64 | (e.user as u64) << 32);
            e.a.logic_shift(Logic::Orr, false, Size::X, tag, tag, top, Shift::Lsl, 33);
            e.a.stp(Size::X, tag, RA, slot, 0);
            jump(e);
        });
    }

    /// Ends a RET to `eip`, skipping the dispatcher when the newest prediction names it.
    fn ret_predicted(&mut self, eip: Reg) {
        if self.page.is_some() {
            self.a.mov(Size::W, r(0), eip);
            let eip = r(0);
            self.x87_flush();
            let (top, slot, want, host, tag) = (T_B, T_D, T_E, T_C, T_RES);
            let miss = self.a.label();
            self.a.ldr(Width::W, top, STATE, state::RS_TOP as i32);
            self.a.add_imm(Size::X, slot, STATE, (layout::RSTACK - layout::STATE) as u32);
            self.a.add(Size::X, slot, slot, top);
            self.a.ldp(Size::X, want, host, slot, 0);
            self.a.sub_imm(Size::W, top, top, 16);
            let ok = self.a.logic_imm(Logic::And, Size::W, top, top, layout::RSTACK_BYTES as u64 - 1);
            assert!(ok);
            self.a.str(Width::W, top, STATE, state::RS_TOP as i32);
            self.a.ldr(Width::W, top, STATE, state::VEPOCH as i32);
            self.a.mov_imm(Size::X, tag, (self.user as u64) << 32);
            self.a.logic_shift(Logic::Orr, false, Size::X, tag, tag, eip, Shift::Lsl, 0);
            self.a.logic_shift(Logic::Orr, false, Size::X, tag, tag, top, Shift::Lsl, 33);
            self.a.eor(Size::X, tag, tag, want);
            self.a.cbnz(Size::X, tag, miss);
            self.a.ret_to(host);
            self.a.bind(miss);
            return self.flushed(|e| e.goto_indirect(eip));
        }
        self.goto_indirect(eip);
    }

    /// Jumps through the dispatcher even within the page, so it can check for interrupts.
    fn goto_dispatch(&mut self, eip: u32) {
        self.x87_flush();
        self.a.mov_imm(Size::W, r(0), eip as u64);
        let off = self.branch_offset(self.dispatch_va);
        self.a.b_offset(off);
    }

    /// Like `goto_reg` for indirect branches that keep the CPL, probing the
    /// dispatcher's EIP cache here so each site has its own `br` to predict.
    fn goto_indirect(&mut self, eip: Reg) {
        if eip != r(0) {
            self.a.mov(Size::W, r(0), eip);
        }
        self.x87_flush();
        if self.page.is_some() {
            let slow = self.a.label();
            let (tag, slot, epoch, base) = (r(6), r(7), r(3), r(2));
            self.a.ldr(Width::W, CHAIN_TMP, STATE, state::PENDING as i32);
            self.a.cbnz(Size::W, CHAIN_TMP, slow);
            // User code is never on global pages; kernel code nearly always is.
            if self.user {
                let ok = self.a.logic_imm(Logic::Orr, Size::X, tag, r(0), 1 << 32);
                assert!(ok);
                self.a.ldr(Width::W, epoch, STATE, state::VEPOCH as i32);
            } else {
                let ok = self.a.logic_imm(Logic::Orr, Size::X, tag, r(0), 1 << 33);
                assert!(ok);
                self.a.ldr(Width::W, epoch, STATE, state::GEPOCH as i32);
            }
            self.a.logic_shift(Logic::Orr, false, Size::X, tag, tag, epoch, Shift::Lsl, 34);
            self.a.logic_shift(Logic::Eor, false, Size::W, slot, r(0), r(0), Shift::Lsr, 16);
            let ok = self.a.logic_imm(Logic::And, Size::W, slot, slot, layout::VLOOKUP_ENTRIES as u64 - 1);
            assert!(ok);
            self.a.ldr(Width::X, base, STATE, state::VLOOKUP as i32);
            self.a.add_shift(Size::X, slot, base, slot, Shift::Lsl, 4);
            self.a.ldp(Size::X, r(4), r(5), slot, 0);
            self.a.eor(Size::X, r(4), r(4), tag);
            self.a.cbnz(Size::X, r(4), slow);
            self.a.br(r(5));
            self.a.bind(slow);
        }
        self.flushed(|e| e.goto_reg(r(0)));
    }

    fn goto_reg(&mut self, eip: Reg) {
        if eip != r(0) {
            self.a.mov(Size::W, r(0), eip);
        }
        self.x87_flush();
        let off = self.branch_offset(self.dispatch_va);
        self.a.b_offset(off);
    }

    /// Branches to `slow` unless memory at linear `at` holds `bytes` (1 to 15),
    /// for the part of an instruction that lies on the next page. NZCV stays.
    fn check_code_bytes(&mut self, at: u32, bytes: &[u8], slow: Label) {
        self.a.mov_imm(Size::W, T_ADDR, at as u64);
        for (i, chunk) in bytes.chunks(8).enumerate() {
            let mut want = 0u64;
            for (j, &b) in chunk.iter().enumerate() {
                want |= (b as u64) << (8 * j);
            }
            self.a.ldr(Width::X, T_B, T_ADDR, 8 * i as i32);
            let drop = 64 - 8 * chunk.len() as u32;
            if drop != 0 {
                self.a.lsl_imm(Size::X, T_B, T_B, drop);
            }
            self.a.mov_imm(Size::X, T_C, want << drop);
            self.a.eor(Size::X, T_B, T_B, T_C);
            self.a.cbnz(Size::X, T_B, slow);
        }
    }

    /// Leaves for the interpreter at `eip` from `slow`, in cold code.
    fn cold_exit(&mut self, slow: Label, eip: u32) {
        self.defer(move |e| {
            e.a.bind(slow);
            e.exit_interp(eip);
        });
    }

    /// Emits `f` after the block's hot path, with the x87 state as it is now.
    fn defer(&mut self, f: impl FnOnce(&mut Emit) + 'static) {
        self.cold.push((self.a.pos(), self.x87, Box::new(f)));
    }

    fn emit_cold(&mut self) {
        let mut i = 0;
        while i < self.cold.len() {
            let (_, x87, f) = std::mem::replace(&mut self.cold[i], (0, Default::default(), Box::new(|_| {})));
            let saved = std::mem::replace(&mut self.x87, x87);
            self.fold = None;
            f(self);
            self.x87 = saved;
            i += 1;
        }
        self.cold.clear();
    }

    /// Drops the code from word `start` on, with what refers to it.
    fn truncate_to(&mut self, start: usize) {
        self.a.truncate(start);
        self.chains.retain(|&(w, _)| (w as usize) < start);
        self.lp.counts.retain(|&w| w < start);
        match self.starts.iter().find(|m| m.0 == start) {
            Some(&(_, n)) => self.cold.truncate(n),
            None => self.cold.retain(|c| c.0 < start),
        }
    }

    /// Drops the code from an STI on and leaves the STI to the interpreter,
    /// which runs the instruction after it before taking interrupts.
    fn back_to_sti(&mut self, (start, eip, insns, nb): (usize, u32, usize, usize), pc: &mut u32, n: &mut usize, boundaries: &mut Vec<(u32, u32)>) {
        self.truncate_to(start);
        boundaries.truncate(nb + 1);
        *pc = eip;
        *n = insns;
        self.exit_interp(eip);
    }

    /// Like `check_code_bytes` for bytes on one page starting at `at`, never
    /// loading outside that page.
    fn check_bytes_in_page(&mut self, at: u32, bytes: &[u8], slow: Label) {
        let full = bytes.len() / 8 * 8;
        if full > 0 {
            self.check_code_bytes(at, &bytes[..full], slow);
        }
        let rest = &bytes[full..];
        if rest.is_empty() {
            return;
        }
        let end = at.wrapping_add(bytes.len() as u32);
        let from = end.wrapping_sub(8);
        if from & !0xfff == at & !0xfff {
            let mut want = 0u64;
            for (j, &b) in rest.iter().enumerate() {
                want |= (b as u64) << (8 * (8 - rest.len() + j));
            }
            let drop = 64 - 8 * rest.len() as u32;
            self.a.mov_imm(Size::W, T_ADDR, from as u64);
            self.a.ldr(Width::X, T_B, T_ADDR, 0);
            self.a.lsr_imm(Size::X, T_B, T_B, drop);
            self.a.mov_imm(Size::X, T_C, want >> drop);
            self.a.eor(Size::X, T_B, T_B, T_C);
            self.a.cbnz(Size::X, T_B, slow);
        } else {
            self.check_code_bytes(at.wrapping_add(full as u32), rest, slow);
        }
    }

    fn exit_with(&mut self, eip: u32, why: u32) {
        self.x87_flush();
        self.a.mov_imm(Size::W, r(0), eip as u64);
        self.a.movz(Size::W, r(1), why, 0);
        let off = self.branch_offset(self.exit_va);
        self.a.b_offset(off);
    }

    /// Bit 0 of `dst` becomes 1 when the last result's low byte has odd parity,
    /// that is when PF is clear. NZCV holds guest flags, so this avoids it.
    fn parity_odd(&mut self, dst: Reg) {
        self.a.ubfx(Size::W, dst, PF_RESULT, 0, 8);
        for sh in [4, 2, 1] {
            self.a.logic_shift(a64::Logic::Eor, false, Size::W, dst, dst, dst, Shift::Lsr, sh);
        }
    }

    fn exit_interp(&mut self, eip: u32) {
        self.x87_flush();
        self.a.mov_imm(Size::W, r(0), eip as u64);
        self.a.movz(Size::W, r(1), reason::INTERP, 0);
        let off = self.branch_offset(self.exit_va);
        self.a.b_offset(off);
    }

    fn add_const(&mut self, dst: Reg, src: Reg, v: u32) {
        if v == 0 {
            if dst != src {
                self.a.mov(Size::W, dst, src);
            }
        } else if v < 4096 {
            self.a.add_imm(Size::W, dst, src, v);
        } else if v.wrapping_neg() < 4096 {
            self.a.sub_imm(Size::W, dst, src, v.wrapping_neg());
        } else {
            self.a.mov_imm(Size::W, T_D, v as u64);
            self.a.add(Size::W, dst, src, T_D);
        }
    }

    /// Like `ea`, but the next `load`/`store` address memory from guest registers.
    /// Past 4 GiB, where x86 wraps, that faults and the host interprets it.
    fn ea_fold(&mut self, insn: &Insn, m: &MemOperand) -> bool {
        if !(self.fold_ok && insn.addr32 && insn.seg_override.is_none_or(|s| s < 4)) {
            return self.ea(insn, m);
        }
        let disp = m.disp as i32;
        let base = match (m.base, m.index) {
            (NO_REG, NO_REG) => return self.ea(insn, m),
            (b, NO_REG) => guest_reg(b as usize),
            (NO_REG, i) => {
                self.a.lsl_imm(Size::W, T_ADDR, guest_reg(i as usize), m.scale as u32);
                T_ADDR
            }
            (b, i) => {
                self.a.add_shift(Size::W, T_ADDR, guest_reg(b as usize), guest_reg(i as usize), Shift::Lsl, m.scale as u32);
                T_ADDR
            }
        };
        // Sums of guest values stay 32-bit, so they wrap as on x86; only the
        // final offset in the access itself can pass 4 GiB.
        let (base, off) = if (-256..4096).contains(&disp) {
            (base, disp)
        } else if disp > 0 && disp < 1 << 24 {
            self.a.add_imm(Size::W, T_ADDR, base, disp as u32 & !0xfff);
            (T_ADDR, disp & 0xfff)
        } else if disp < 0 && disp > -(1 << 24) {
            let hi = (disp.unsigned_abs() + 0xfff) & !0xfff;
            self.a.sub_imm(Size::W, T_ADDR, base, hi);
            (T_ADDR, hi as i32 + disp)
        } else {
            self.add_const(T_ADDR, base, disp as u32);
            (T_ADDR, 0)
        };
        self.fold = Some(Fold(base, off));
        true
    }

    /// The folded operand for an access of width `w`, or None after putting
    /// its address in T_ADDR when no A64 form fits.
    fn folded(&mut self, w: Width) -> Option<Fold> {
        let f = self.fold?;
        let size = match w {
            Width::B => 1,
            Width::H => 2,
            Width::W => 4,
            Width::X => 8,
        };
        let Fold(b, d) = f;
        if (d >= 0 && d % size == 0 && d / size < 4096) || (-256..256).contains(&d) {
            return Some(f);
        }
        self.fold = None;
        self.add_const(T_ADDR, b, d as u32);
        None
    }

    fn ldst_folded(&mut self, load: bool, w: Width, rt: Reg, Fold(b, d): Fold) {
        self.a.ldst(load, w, false, load && w == Width::X, rt, b, d);
    }

    /// Computes the effective (linear) address of `m` into T_ADDR.
    fn ea(&mut self, insn: &Insn, m: &MemOperand) -> bool {
        self.fold = None;
        if !insn.addr32 {
            return false;
        }
        let seg_base = match insn.seg_override {
            Some(4) => Some(state::FS_BASE),
            Some(5) => Some(state::GS_BASE),
            _ => None,
        };
        match (m.base, m.index) {
            (NO_REG, NO_REG) => self.a.mov_imm(Size::W, T_ADDR, m.disp as u64),
            (b, NO_REG) => self.add_const(T_ADDR, guest_reg(b as usize), m.disp),
            (NO_REG, i) => {
                self.a.mov_imm(Size::W, T_ADDR, m.disp as u64);
                self.a.add_shift(Size::W, T_ADDR, T_ADDR, guest_reg(i as usize), Shift::Lsl, m.scale as u32);
            }
            (b, i) => {
                self.a.add_shift(Size::W, T_ADDR, guest_reg(b as usize), guest_reg(i as usize), Shift::Lsl, m.scale as u32);
                self.add_const(T_ADDR, T_ADDR, m.disp);
            }
        }
        if let Some(off) = seg_base {
            self.a.ldr(Width::W, T_D, STATE, off as i32);
            self.a.add(Size::W, T_ADDR, T_ADDR, T_D);
        }
        true
    }

    fn load(&mut self, w: Width, dst: Reg) {
        match self.folded(w) {
            Some(f) => self.ldst_folded(true, w, dst, f),
            None => self.a.ldr(w, dst, T_ADDR, 0),
        }
    }

    /// A sign-extending 8- or 16-bit load into a W register.
    fn load_signed(&mut self, w: Width, dst: Reg) {
        match self.folded(w) {
            Some(Fold(b, d)) => self.a.ldst(true, w, true, false, dst, b, d),
            None => self.a.ldrs(w, Size::W, dst, T_ADDR, 0),
        }
    }

    fn store(&mut self, w: Width, src: Reg) {
        self.stored = true;
        match self.folded(w) {
            Some(f) => self.ldst_folded(false, w, src, f),
            None => self.a.str(w, src, T_ADDR, 0),
        }
    }

    fn read_r8(&mut self, n: usize, dst: Reg) {
        self.a.ubfx(Size::W, dst, guest_reg(n & 3), if n < 4 { 0 } else { 8 }, 8);
    }

    fn write_r8(&mut self, n: usize, src: Reg) {
        self.a.bfi(Size::W, guest_reg(n & 3), src, if n < 4 { 0 } else { 8 }, 8);
    }

    fn set_af(&mut self, a: Reg, b: Reg, res: Reg) {
        self.a.eor(Size::W, AF_AUX, a, b);
        self.a.eor(Size::W, AF_AUX, AF_AUX, res);
    }

    fn needs(&self, f: u8) -> bool {
        self.need & f != 0
    }

    fn result_flags(&mut self, a: Reg, b: Reg, logic: bool) {
        self.result_flags_of(T_RES, a, b, logic);
    }

    fn result_flags_of(&mut self, res: Reg, a: Reg, b: Reg, logic: bool) {
        if self.needs(flags::PF) {
            self.a.mov(Size::W, PF_RESULT, res);
        }
        if self.needs(flags::AF) {
            if logic {
                self.a.mov(Size::W, AF_AUX, ZR);
            } else {
                self.set_af(a, b, res);
            }
        }
    }

    /// Where a 32-bit ALU op writing guest register `n` may put its result:
    /// the register itself, unless AF still needs an operand it would overwrite.
    fn alu_dst(&self, n: usize, a: Reg, b: Reg) -> Reg {
        let d = guest_reg(n);
        if self.needs(flags::AF) && (d == a || d == b) { T_RES } else { d }
    }

    /// Group-1 operation `op` on 32-bit values; result in T_RES, flags in NZCV.
    fn alu32(&mut self, op: usize, a: Reg, b: Reg) -> bool {
        self.alu32_to(op, T_RES, a, b)
    }

    /// `alu32` with the result in `d`, which may be `a` or `b` only while AF is dead.
    fn alu32_to(&mut self, op: usize, d: Reg, a: Reg, b: Reg) -> bool {
        let nzcv = self.needs(flags::NZCV);
        match op {
            0 if nzcv => {
                self.a.adds(Size::W, d, a, b);
                self.a.cfinv();
            }
            0 => self.a.add(Size::W, d, a, b),
            1 | 4 | 6 => {
                match op {
                    1 => self.a.orr(Size::W, d, a, b),
                    4 => self.a.and(Size::W, d, a, b),
                    _ => self.a.eor(Size::W, d, a, b),
                }
                if nzcv {
                    self.a.tst(Size::W, d, d);
                    self.a.cfinv();
                }
                self.result_flags_of(d, a, b, true);
                return true;
            }
            2 => {
                self.a.cfinv();
                self.a.adcs(Size::W, d, a, b);
                self.a.cfinv();
            }
            3 => self.a.sbcs(Size::W, d, a, b),
            _ if nzcv => self.a.subs(Size::W, d, a, b),
            _ => self.a.sub(Size::W, d, a, b),
        }
        self.result_flags_of(d, a, b, false);
        true
    }

    /// `alu32` with an immediate `b`, when an A64 immediate form fits and AF,
    /// which needs `b` in a register, is dead. False when nothing was emitted.
    fn alu32_imm(&mut self, op: usize, d: Reg, a: Reg, b: u32) -> bool {
        if self.needs(flags::AF) || matches!(op, 2 | 3) {
            return false;
        }
        let nzcv = self.needs(flags::NZCV);
        let arith_ok = b < 4096 || (b & 0xfff == 0 && b >> 12 < 4096);
        match op {
            0 if arith_ok && nzcv => {
                self.a.adds_imm(Size::W, d, a, b);
                self.a.cfinv();
            }
            0 if arith_ok => self.a.add_imm(Size::W, d, a, b),
            5 | 7 if arith_ok && nzcv => self.a.subs_imm(Size::W, d, a, b),
            5 | 7 if arith_ok => self.a.sub_imm(Size::W, d, a, b),
            1 | 4 | 6 => {
                let kind = match op {
                    1 => Logic::Orr,
                    4 => Logic::And,
                    _ => Logic::Eor,
                };
                if !self.a.logic_imm(kind, Size::W, d, a, b as u64) {
                    return false;
                }
                if nzcv {
                    self.a.tst(Size::W, d, d);
                    self.a.cfinv();
                }
            }
            _ => return false,
        }
        if self.needs(flags::PF) {
            self.a.mov(Size::W, PF_RESULT, d);
        }
        true
    }

    fn alu8(&mut self, op: usize, a: Reg, b: Reg) -> bool {
        self.alu_narrow(op, a, b, 24)
    }

    /// Group-1 operation on the low 32 - `shift` bits of `a` and `b`, done in
    /// the top bits so NZCV come out right.
    fn alu_narrow(&mut self, op: usize, a: Reg, b: Reg, shift: u32) -> bool {
        if op == 2 || op == 3 {
            self.adc_sbb_narrow(op == 3, a, b, 32 - shift);
            return true;
        }
        let logic = matches!(op, 1 | 4 | 6);
        if !self.needs(flags::NZCV) {
            match op {
                0 => self.a.add(Size::W, T_RES, a, b),
                1 => self.a.orr(Size::W, T_RES, a, b),
                4 => self.a.and(Size::W, T_RES, a, b),
                6 => self.a.eor(Size::W, T_RES, a, b),
                _ => self.a.sub(Size::W, T_RES, a, b),
            }
            self.result_flags(a, b, logic);
            return true;
        }
        self.a.lsl_imm(Size::W, T_C, a, shift);
        self.a.lsl_imm(Size::W, T_D, b, shift);
        match op {
            0 => {
                self.a.adds(Size::W, T_RES, T_C, T_D);
                self.a.cfinv();
            }
            1 | 4 | 6 => {
                match op {
                    1 => self.a.orr(Size::W, T_RES, T_C, T_D),
                    4 => self.a.and(Size::W, T_RES, T_C, T_D),
                    _ => self.a.eor(Size::W, T_RES, T_C, T_D),
                }
                self.a.tst(Size::W, T_RES, T_RES);
                self.a.cfinv();
            }
            _ => self.a.subs(Size::W, T_RES, T_C, T_D),
        }
        self.a.lsr_imm(Size::W, T_RES, T_RES, shift);
        self.result_flags(a, b, logic);
        true
    }

    /// 8- or 16-bit ADC/SBB: carry from a zero-extended sum, N/Z/V from a
    /// sign-extended one through SETF8/SETF16.
    fn adc_sbb_narrow(&mut self, sub: bool, a: Reg, b: Reg, width: u32) {
        let arith = |e: &mut Self, d: Reg, x: Reg, y: Reg| {
            if sub {
                e.a.sub(Size::W, d, x, y)
            } else {
                e.a.add(Size::W, d, x, y)
            }
        };
        self.a.cset(Size::W, T_D, Cond::Lo);
        self.a.ubfx(Size::W, T_C, a, 0, width);
        self.a.ubfx(Size::W, T_RES, b, 0, width);
        arith(self, T_C, T_C, T_RES);
        arith(self, T_C, T_C, T_D);
        self.a.sbfx(Size::W, T_E, a, 0, width);
        self.a.sbfx(Size::W, T_RES, b, 0, width);
        arith(self, T_E, T_E, T_RES);
        arith(self, T_E, T_E, T_D);
        if width == 8 {
            self.a.setf8(T_E);
        } else {
            self.a.setf16(T_E);
        }
        self.a.ubfx(Size::W, T_D, T_C, width, 1);
        let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_D, T_D, 1);
        assert!(ok);
        self.a.rmif(T_D, 63, 0b0010);
        self.a.ubfx(Size::W, T_RES, T_C, 0, width);
        self.a.mov(Size::W, PF_RESULT, T_RES);
        self.set_af(a, b, T_RES);
    }

    fn push(&mut self, value: Reg) {
        let esp = guest_reg(4);
        self.a.sub_imm(Size::W, T_ADDR, esp, 4);
        self.store(Width::W, value);
        self.a.mov(Size::W, esp, T_ADDR);
    }

    /// Reads the r/m32 operand into a register (T_A for memory).
    fn rm32(&mut self, insn: &Insn) -> Option<Reg> {
        match insn.mem {
            Some(m) => {
                if !self.ea(insn, &m) {
                    return None;
                }
                self.load(Width::W, T_A);
                Some(T_A)
            }
            None => Some(guest_reg(insn.rm())),
        }
    }

    fn rm8(&mut self, insn: &Insn) -> Option<Reg> {
        match insn.mem {
            Some(m) => {
                if !self.ea_fold(insn, &m) {
                    return None;
                }
                self.load(Width::B, T_A);
            }
            None => self.read_r8(insn.rm(), T_A),
        }
        Some(T_A)
    }

    fn read_gpr(&mut self, w: Ow, n: usize, scratch: Reg) -> Reg {
        match w {
            Ow::D => guest_reg(n),
            Ow::B => {
                self.read_r8(n, scratch);
                scratch
            }
            Ow::H => {
                self.a.uxth(scratch, guest_reg(n));
                scratch
            }
        }
    }

    /// `read_gpr` for an operand of group-1 `op`: with NZCV dead only its low
    /// bits matter (but ADC/SBB's carry), so AL..BL and 16-bit registers are used in place.
    fn read_alu_operand(&mut self, op: usize, w: Ow, n: usize, scratch: Reg) -> Reg {
        let in_place = match w {
            Ow::D => true,
            Ow::H => true,
            Ow::B => n < 4,
        };
        if in_place && !self.needs(flags::NZCV) && !matches!(op, 2 | 3) { guest_reg(n) } else { self.read_gpr(w, n, scratch) }
    }

    fn write_gpr(&mut self, w: Ow, n: usize, src: Reg) {
        match w {
            Ow::D => {
                if guest_reg(n) != src {
                    self.a.mov(Size::W, guest_reg(n), src);
                }
            }
            Ow::B => self.write_r8(n, src),
            Ow::H => self.a.bfi(Size::W, guest_reg(n), src, 0, 16),
        }
    }

    fn alu(&mut self, op: usize, w: Ow, a: Reg, b: Reg) -> bool {
        match w {
            Ow::D => self.alu32(op, a, b),
            Ow::H => self.alu_narrow(op, a, b, 16),
            Ow::B => self.alu_narrow(op, a, b, 24),
        }
    }

    /// With `probe`, a memory operand is written back first, so the later store
    /// cannot fault after an instruction that reads the flags has changed them.
    fn rm_w(&mut self, insn: &Insn, w: Ow, probe: bool) -> Option<Reg> {
        match insn.mem {
            Some(m) => {
                if !self.ea_fold(insn, &m) {
                    return None;
                }
                self.load(w.mem(), T_A);
                if probe {
                    self.store(w.mem(), T_A);
                }
                Some(T_A)
            }
            None => Some(self.read_gpr(w, insn.rm(), T_A)),
        }
    }

    fn write_rm(&mut self, insn: &Insn, w: Ow, src: Reg) {
        match insn.mem {
            Some(_) => self.store(w.mem(), src),
            None => self.write_gpr(w, insn.rm(), src),
        }
    }

    /// Sets NZCV from T_RES, with x86 CF in T_B and OF in T_C (each 0 or 1).
    fn shift_flags(&mut self) {
        if !self.needs(flags::NZCV) {
            if self.needs(flags::PF) {
                self.a.mov(Size::W, PF_RESULT, T_RES);
            }
            return;
        }
        self.a.tst(Size::W, T_RES, T_RES);
        let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
        assert!(ok);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_C, T_B, Shift::Lsl, 1);
        self.a.rmif(T_C, 0, 0b0011);
        self.a.mov(Size::W, PF_RESULT, T_RES);
    }

    fn shift_cl(&mut self, kind: usize, a: Reg) {
        let n = T_D;
        let ok = self.a.logic_imm(a64::Logic::And, Size::W, n, guest_reg(1), 31);
        assert!(ok);
        match kind {
            4 | 6 => {
                self.a.lslv(Size::W, T_RES, a, n);
                self.a.movz(Size::W, T_C, 32, 0);
                self.a.sub(Size::W, T_C, T_C, n);
                self.a.lsrv(Size::W, T_B, a, T_C);
                self.a.lsr_imm(Size::W, T_C, T_RES, 31);
            }
            5 => {
                self.a.lsrv(Size::W, T_RES, a, n);
                self.a.sub_imm(Size::W, T_C, n, 1);
                self.a.lsrv(Size::W, T_B, a, T_C);
                self.a.lsr_imm(Size::W, T_C, a, 31);
            }
            _ => {
                self.a.asrv(Size::W, T_RES, a, n);
                self.a.sub_imm(Size::W, T_C, n, 1);
                self.a.asrv(Size::W, T_B, a, T_C);
                self.a.mov(Size::W, T_C, ZR);
            }
        }
        let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_B, T_B, 1);
        assert!(ok);
        if matches!(kind, 4 | 6) {
            self.a.eor(Size::W, T_C, T_C, T_B);
        }
        self.shift_flags();
    }

    /// Signed 32x32 multiply into `dst`, with x86 IMUL flags.
    fn imul32(&mut self, dst: Reg, a: Reg, b: Reg) {
        if self.need == 0 {
            self.a.mul(Size::W, dst, a, b);
            return;
        }
        self.a.smull(T_RES, a, b);
        self.a.addsub_ext(Size::X, true, true, ZR, T_RES, T_RES, Extend::Sxtw, 0);
        self.a.cset(Size::W, T_B, Cond::Ne);
        self.a.tst(Size::W, T_RES, T_RES);
        self.a.movz(Size::W, T_C, 2, 0);
        self.a.sub(Size::W, T_C, T_C, T_B);
        self.a.rmif(T_C, 0, 0b0011);
        self.a.mov(Size::W, PF_RESULT, T_RES);
        self.a.mov(Size::W, AF_AUX, ZR);
        self.a.mov(Size::W, dst, T_RES);
    }

    fn shift_imm(&mut self, kind: usize, dst: Reg, a: Reg, n: u32) -> bool {
        let n = n & 31;
        if n == 0 {
            if dst != a {
                self.a.mov(Size::W, dst, a);
            }
            return true;
        }
        if !self.needs(flags::NZCV) {
            let out = if self.needs(flags::PF) { T_RES } else { dst };
            match kind {
                4 | 6 => self.a.lsl_imm(Size::W, out, a, n),
                5 => self.a.lsr_imm(Size::W, out, a, n),
                7 => self.a.asr_imm(Size::W, out, a, n),
                _ => return false,
            }
            if out == T_RES {
                self.a.mov(Size::W, PF_RESULT, T_RES);
                if dst != T_RES {
                    self.a.mov(Size::W, dst, T_RES);
                }
            }
            return true;
        }
        match kind {
            4 | 6 => {
                self.a.lsl_imm(Size::W, T_RES, a, n);
                self.a.ubfx(Size::W, T_B, a, 32 - n, 1);
                self.a.lsr_imm(Size::W, T_C, T_RES, 31);
                self.a.eor(Size::W, T_C, T_C, T_B);
            }
            5 => {
                self.a.lsr_imm(Size::W, T_RES, a, n);
                self.a.ubfx(Size::W, T_B, a, n - 1, 1);
                self.a.lsr_imm(Size::W, T_C, a, 31);
            }
            7 => {
                self.a.asr_imm(Size::W, T_RES, a, n);
                self.a.ubfx(Size::W, T_B, a, n - 1, 1);
                self.a.mov(Size::W, T_C, ZR);
            }
            _ => return false,
        }
        self.a.tst(Size::W, T_RES, T_RES);
        let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
        assert!(ok);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_C, T_B, Shift::Lsl, 1);
        self.a.rmif(T_C, 0, 0b0011);
        self.a.mov(Size::W, PF_RESULT, T_RES);
        if dst != T_RES {
            self.a.mov(Size::W, dst, T_RES);
        }
        true
    }

    /// MOVS/STOS/LODS/CMPS/SCAS with optional REP/REPE/REPNE. DF=1 falls
    /// back to the interpreter at run time.
    fn string_op(&mut self, insn: &Insn, _eip: u32) -> bool {
        let op = insn.op;
        self.need = flags::ALL;
        if !insn.addr32 || matches!(insn.seg_override, Some(4) | Some(5)) {
            return false;
        }
        let w = Ow::of(op, insn.op32);
        let n = match w {
            Ow::B => 1,
            Ow::H => 2,
            Ow::D => 4,
        };
        let (back, done) = (self.a.label(), self.a.label());
        self.a.ldr(Width::W, T_D, STATE, state::DF as i32);
        self.a.cbnz(Size::W, T_D, back);
        self.string_loop(insn, w, n, false);
        self.a.b(done);
        self.a.bind(back);
        self.string_loop(insn, w, n, true);
        self.a.bind(done);
        true
    }

    /// One string instruction (with its REP loop) stepping forward, or back for DF = 1.
    fn string_loop(&mut self, insn: &Insn, w: Ow, n: u32, back: bool) {
        let op = insn.op;
        let (esi, edi, ecx) = (guest_reg(6), guest_reg(7), guest_reg(1));
        let rep = insn.rep != Rep::None;
        let compares = matches!(op, 0xa6 | 0xa7 | 0xae | 0xaf);
        let (top, done) = (self.a.label(), self.a.label());
        let step = |e: &mut Self, r: Reg| {
            if back {
                e.a.sub_imm(Size::W, r, r, n);
            } else {
                e.a.add_imm(Size::W, r, r, n);
            }
        };
        if rep && matches!(op, 0xa4 | 0xa5 | 0xaa | 0xab) {
            self.bulk_string(op, w, n, back);
        }
        self.a.bind(top);
        if rep {
            self.a.cbz(Size::W, ecx, done);
        }
        match op {
            0xa4 | 0xa5 => {
                self.a.ldr(w.mem(), T_A, esi, 0);
                self.a.str(w.mem(), T_A, edi, 0);
                step(self, esi);
                step(self, edi);
            }
            0xaa | 0xab => {
                let v = self.read_gpr(w, 0, T_A);
                self.a.str(w.mem(), v, edi, 0);
                step(self, edi);
            }
            0xac | 0xad => {
                self.a.ldr(w.mem(), T_A, esi, 0);
                self.write_gpr(w, 0, T_A);
                step(self, esi);
            }
            0xa6 | 0xa7 => {
                self.a.ldr(w.mem(), T_A, esi, 0);
                self.a.ldr(w.mem(), T_B, edi, 0);
                step(self, esi);
                step(self, edi);
            }
            _ => {
                self.a.ldr(w.mem(), T_B, edi, 0);
                step(self, edi);
            }
        }
        if rep {
            self.a.sub_imm(Size::W, ecx, ecx, 1);
        }
        if compares {
            let a = if matches!(op, 0xae | 0xaf) { self.read_gpr(w, 0, T_A) } else { T_A };
            self.alu(7, w, a, T_B);
            match insn.rep {
                Rep::Rep => self.a.b_cond(Cond::Eq, top),
                Rep::Repne => self.a.b_cond(Cond::Ne, top),
                Rep::None => {}
            }
        } else if rep {
            self.a.b(top);
        }
        self.a.bind(done);
    }

    /// REP MOVS/STOS 16 bytes at a time while that much is left. Single
    /// registers keep MMIO syndromes valid; registers advance after each chunk.
    fn bulk_string(&mut self, op: u16, w: Ow, n: u32, back: bool) {
        let (esi, edi, ecx) = (guest_reg(6), guest_reg(7), guest_reg(1));
        let per = 16 / n;
        let (top, out) = (self.a.label(), self.a.label());
        if matches!(op, 0xa4 | 0xa5) {
            // A destination less than 16 bytes past the source, in the direction
            // of the copy, must see each element's copy.
            if back {
                self.a.sub(Size::W, T_B, esi, edi);
            } else {
                self.a.sub(Size::W, T_B, edi, esi);
            }
            self.a.lsr_imm(Size::W, T_B, T_B, 4);
            self.a.cbz(Size::W, T_B, out);
        } else {
            let v = self.read_gpr(w, 0, T_A);
            match w {
                Ow::B => {
                    self.a.mov_imm(Size::X, T_C, 0x0101_0101_0101_0101);
                    self.a.uxtb(T_B, v);
                    self.a.mul(Size::X, T_B, T_B, T_C);
                }
                Ow::H => {
                    self.a.mov_imm(Size::X, T_C, 0x0001_0001_0001_0001);
                    self.a.uxth(T_B, v);
                    self.a.mul(Size::X, T_B, T_B, T_C);
                }
                Ow::D => {
                    self.a.mov(Size::W, T_B, v);
                    self.a.logic_shift(Logic::Orr, false, Size::X, T_B, T_B, T_B, Shift::Lsl, 32);
                }
            }
        }
        // Backwards, a chunk ends with the element at ESI/EDI.
        let lo = if back { n as i32 - 16 } else { 0 };
        self.a.bind(top);
        self.a.sub_imm(Size::W, T_C, ecx, per);
        self.a.tbnz(T_C, 31, out);
        if matches!(op, 0xa4 | 0xa5) {
            self.a.ldr(Width::X, T_A, esi, lo);
            self.a.ldr(Width::X, T_D, esi, lo + 8);
            self.a.str(Width::X, T_A, edi, lo);
            self.a.str(Width::X, T_D, edi, lo + 8);
            if back {
                self.a.sub_imm(Size::W, esi, esi, 16);
            } else {
                self.a.add_imm(Size::W, esi, esi, 16);
            }
        } else {
            self.a.str(Width::X, T_B, edi, lo);
            self.a.str(Width::X, T_B, edi, lo + 8);
        }
        if back {
            self.a.sub_imm(Size::W, edi, edi, 16);
        } else {
            self.a.add_imm(Size::W, edi, edi, 16);
        }
        self.a.mov(Size::W, ecx, T_C);
        self.a.b(top);
        self.a.bind(out);
    }

    /// 32-bit DIV/IDIV; division by zero and quotient overflow go to the
    /// interpreter, which raises #DE.
    fn div32(&mut self, insn: &Insn, eip: u32, signed: bool) -> bool {
        let Some(d) = self.rm_w(insn, Ow::D, false) else { return false };
        let (eax, edx) = (guest_reg(0), guest_reg(2));
        let slow = self.a.label();
        let done = self.a.label();
        self.a.mov(Size::W, T_B, d);
        self.a.cbz(Size::W, T_B, slow);
        self.a.lsl_imm(Size::X, T_C, edx, 32);
        self.a.orr(Size::X, T_C, T_C, eax);
        if signed {
            self.a.sxtw(T_B, T_B);
            self.a.sdiv(Size::X, T_RES, T_C, T_B);
            self.a.sxtw(T_D, T_RES);
            self.a.eor(Size::X, T_D, T_D, T_RES);
        } else {
            self.a.udiv(Size::X, T_RES, T_C, T_B);
            self.a.lsr_imm(Size::X, T_D, T_RES, 32);
        }
        self.a.cbnz(Size::X, T_D, slow);
        self.a.msub(Size::X, T_D, T_RES, T_B, T_C);
        self.a.mov(Size::W, eax, T_RES);
        self.a.mov(Size::W, edx, T_D);
        self.cold_exit(slow, eip);
        self.a.bind(done);
        true
    }

    fn insn(&mut self, insn: &Insn, eip: u32) -> Step {
        self.fold = None;
        let next = eip.wrapping_add(insn.len as u32);
        let op = insn.op;
        let op32 = insn.op32;
        if sse::is_sse(insn) {
            return if self.sse(insn, eip) { Step::Next } else { Step::Unsupported };
        }
        let string = matches!(op, 0xa4..=0xa7 | 0xaa..=0xaf);
        let branch = matches!(op, 0x70..=0x7f | 0x180..=0x18f | 0xe8 | 0xe9 | 0xeb) || (op == 0xff && matches!(insn.reg(), 2 | 4));
        if insn.rep != Rep::None && !(matches!(op, 0x90 | 0xc2 | 0xc3) || string || branch) {
            return Step::Unsupported;
        }
        match op {
            0x90 => {}
            0x00..=0x3f if op & 7 < 6 => {
                let aop = (op >> 3) as usize & 7;
                let w = Ow::of(op, op32);
                let writes = aop != 7;
                match op & 7 {
                    0 | 1 => {
                        let a = match insn.mem {
                            None => self.read_alu_operand(aop, w, insn.rm(), T_A),
                            Some(_) => {
                                let Some(a) = self.rm_w(insn, w, writes && matches!(aop, 2 | 3)) else { return Step::Unsupported };
                                a
                            }
                        };
                        let b = self.read_alu_operand(aop, w, insn.reg(), T_B);
                        let d = if writes && insn.mem.is_none() && matches!(w, Ow::D) { self.alu_dst(insn.rm(), a, b) } else { T_RES };
                        if d == T_RES {
                            self.alu(aop, w, a, b);
                        } else {
                            self.alu32_to(aop, d, a, b);
                        }
                        if writes && d == T_RES {
                            self.write_rm(insn, w, T_RES);
                        }
                    }
                    2 | 3 => {
                        let b = match insn.mem {
                            None => self.read_alu_operand(aop, w, insn.rm(), T_A),
                            Some(_) => {
                                let Some(b) = self.rm_w(insn, w, false) else { return Step::Unsupported };
                                b
                            }
                        };
                        let a = self.read_alu_operand(aop, w, insn.reg(), T_B);
                        let d = if writes && matches!(w, Ow::D) { self.alu_dst(insn.reg(), a, b) } else { T_RES };
                        if d == T_RES {
                            self.alu(aop, w, a, b);
                        } else {
                            self.alu32_to(aop, d, a, b);
                        }
                        if writes && d == T_RES {
                            self.write_gpr(w, insn.reg(), T_RES);
                        }
                    }
                    _ => {
                        let a = self.read_gpr(w, 0, T_A);
                        let d = if writes && matches!(w, Ow::D) { self.alu_dst(0, a, a) } else { T_RES };
                        if !(matches!(w, Ow::D) && self.alu32_imm(aop, d, a, insn.imm)) {
                            self.a.mov_imm(Size::W, T_B, insn.imm as u64 & w.mask());
                            if d == T_RES {
                                self.alu(aop, w, a, T_B);
                            } else {
                                self.alu32_to(aop, d, a, T_B);
                            }
                        }
                        if writes && d == T_RES {
                            self.write_gpr(w, 0, T_RES);
                        }
                    }
                }
            }
            0x40..=0x4f if op32 => {
                let reg = guest_reg((op & 7) as usize);
                self.inc_dec(op >= 0x48, reg, reg);
            }
            0xd7 if insn.addr32 && insn.seg_override.is_none_or(|s| s < 4) => {
                self.a.ubfx(Size::W, T_A, guest_reg(0), 0, 8);
                self.a.add(Size::W, T_ADDR, guest_reg(3), T_A);
                self.load(Width::B, T_A);
                self.a.bfi(Size::W, guest_reg(0), T_A, 0, 8);
            }
            0x50..=0x57 if op32 => self.push(guest_reg((op & 7) as usize)),
            0x58..=0x5f if op32 => {
                let esp = guest_reg(4);
                self.a.mov(Size::W, T_ADDR, esp);
                self.load(Width::W, T_A);
                self.a.add_imm(Size::W, esp, esp, 4);
                self.a.mov(Size::W, guest_reg((op & 7) as usize), T_A);
            }
            0x68 | 0x6a if op32 => {
                let v = if op == 0x6a { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                self.a.mov_imm(Size::W, T_B, v as u64);
                self.push(T_B);
            }
            0x70..=0x7f | 0x180..=0x18f => {
                let rel = if op < 0x100 { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                if !op32 {
                    return Step::Unsupported;
                }
                let target = next.wrapping_add(rel);
                if self.superblock {
                    let not_taken = self.a.label();
                    match x86_cond(op) {
                        Some(c) => self.a.b_cond(c.invert(), not_taken),
                        None => {
                            self.parity_odd(T_B);
                            if op & 1 == 0 {
                                self.a.tbnz(T_B, 0, not_taken);
                            } else {
                                self.a.tbz(T_B, 0, not_taken);
                            }
                        }
                    }
                    self.goto(target);
                    self.a.bind(not_taken);
                    return Step::Branch;
                }
                let taken = self.a.label();
                match x86_cond(op) {
                    Some(c) => self.a.b_cond(c, taken),
                    None => {
                        self.parity_odd(T_B);
                        if op & 1 == 0 {
                            self.a.tbz(T_B, 0, taken);
                        } else {
                            self.a.tbnz(T_B, 0, taken);
                        }
                    }
                }
                self.goto(next);
                self.a.bind(taken);
                self.goto(target);
                return Step::End;
            }
            0x80..=0x83 => {
                let aop = insn.reg();
                let w = Ow::of(op, op32);
                let imm = if op == 0x81 { insn.imm } else { insn.imm as u8 as i8 as i32 as u32 };
                let Some(a) = self.rm_w(insn, w, matches!(aop, 2 | 3)) else { return Step::Unsupported };
                let d = if aop != 7 && insn.mem.is_none() && matches!(w, Ow::D) { self.alu_dst(insn.rm(), a, a) } else { T_RES };
                if !(matches!(w, Ow::D) && self.alu32_imm(aop, d, a, imm)) {
                    self.a.mov_imm(Size::W, T_B, imm as u64 & w.mask());
                    if d == T_RES {
                        self.alu(aop, w, a, T_B);
                    } else {
                        self.alu32_to(aop, d, a, T_B);
                    }
                }
                if aop != 7 && d == T_RES {
                    self.write_rm(insn, w, T_RES);
                }
            }
            0x84 | 0x85 => {
                let w = Ow::of(op, op32);
                let Some(a) = self.rm_w(insn, w, false) else { return Step::Unsupported };
                let b = self.read_gpr(w, insn.reg(), T_B);
                self.alu(4, w, a, b);
            }
            0x86 | 0x87 => {
                let w = Ow::of(op, op32);
                let Some(a) = self.rm_w(insn, w, false) else { return Step::Unsupported };
                self.a.mov(Size::W, T_C, a);
                let b = self.read_gpr(w, insn.reg(), T_B);
                self.write_rm(insn, w, b);
                self.write_gpr(w, insn.reg(), T_C);
            }
            0x89 | 0x8b | 0xa1 | 0xa3 if self.tpr_mov(insn) => {}
            0x88 | 0x89 => {
                let w = Ow::of(op, op32);
                let v = self.read_gpr(w, insn.reg(), T_B);
                match insn.mem {
                    Some(m) => {
                        if !self.ea_fold(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.store(w.mem(), v);
                    }
                    None => self.write_gpr(w, insn.rm(), v),
                }
            }
            0x8b if op32 && insn.mem.is_some() => {
                let m = insn.mem.unwrap();
                if !self.ea_fold(insn, &m) {
                    return Step::Unsupported;
                }
                self.load(Width::W, guest_reg(insn.reg()));
            }
            0x8a | 0x8b => {
                let w = Ow::of(op, op32);
                let Some(v) = self.rm_w(insn, w, false) else { return Step::Unsupported };
                self.write_gpr(w, insn.reg(), v);
            }
            0x8d if op32 => {
                let Some(m) = insn.mem else { return Step::Unsupported };
                let plain = Insn { seg_override: None, ..*insn };
                if !self.ea(&plain, &m) {
                    return Step::Unsupported;
                }
                self.a.mov(Size::W, guest_reg(insn.reg()), T_ADDR);
            }
            0xa4..=0xa7 | 0xaa..=0xaf => {
                if !self.string_op(insn, eip) {
                    return Step::Unsupported;
                }
            }
            0xcb if op32 => {
                let esp = guest_reg(4);
                let slow = self.a.label();
                self.a.mov(Size::W, T_ADDR, esp);
                self.load(Width::W, T_A);
                self.a.ldr(Width::H, T_B, esp, 4);
                self.a.ldr(Width::W, T_C, STATE, state::CS_SEL as i32);
                self.a.eor(Size::W, T_C, T_C, T_B);
                self.a.cbnz(Size::W, T_C, slow);
                self.a.add_imm(Size::W, esp, esp, 8);
                self.goto_indirect(T_A);
                self.a.bind(slow);
                self.exit_interp(eip);
                return Step::End;
            }
            0xfe if insn.reg() < 2 => {
                let Some(a) = self.rm_w(insn, Ow::B, false) else { return Step::Unsupported };
                self.inc_dec_narrow(insn.reg() == 1, a, 8);
                self.write_rm(insn, Ow::B, T_RES);
            }
            0xfc => self.a.str(Width::W, ZR, STATE, state::DF as i32),
            0xfd => {
                self.a.movz(Size::W, T_B, 1, 0);
                self.a.str(Width::W, T_B, STATE, state::DF as i32);
            }
            0xfa | 0xfb if self.user => return Step::Unsupported,
            0xfa => self.a.str(Width::W, ZR, STATE, state::IF_FLAG as i32),
            0xfb => {
                self.a.movz(Size::W, T_B, 1, 0);
                self.a.str(Width::W, T_B, STATE, state::IF_FLAG as i32);
                return Step::Sti;
            }
            0x98 if op32 => self.a.sxth(Size::W, guest_reg(0), guest_reg(0)),
            0x99 if op32 => self.a.asr_imm(Size::W, guest_reg(2), guest_reg(0), 31),
            0xed if op32 && !self.user => {
                let slow = self.a.label();
                let done = self.a.label();
                self.a.ldr(Width::W, T_A, STATE, state::PM_PORT as i32);
                self.a.ubfx(Size::W, T_B, guest_reg(2), 0, 16);
                self.a.eor(Size::W, T_B, T_B, T_A);
                self.a.cbnz(Size::W, T_B, slow);
                self.a.mrs(T_A, a64::sysreg::CNTVCT_EL0);
                self.a.ldr(Width::X, T_B, STATE, state::PM_MUL as i32);
                self.a.umulh(T_A, T_A, T_B);
                self.a.ldr(Width::X, T_B, STATE, state::PM_ADD as i32);
                self.a.add(Size::X, T_A, T_A, T_B);
                self.a.ubfx(Size::W, guest_reg(0), T_A, 0, 24);
                self.cold_exit(slow, eip);
                self.a.bind(done);
            }
            0x131 => {
                if self.user {
                    let slow = self.a.label();
                    self.a.ldr(Width::W, T_A, STATE, state::CR4 as i32);
                    self.a.tbnz(T_A, x86::cpu::cr4::TSD.trailing_zeros(), slow);
                    self.cold_exit(slow, eip);
                }
                self.a.mrs(T_A, a64::sysreg::CNTVCT_EL0);
                self.a.ldr(Width::X, T_B, STATE, state::TSC_ORIGIN as i32);
                self.a.sub(Size::X, T_A, T_A, T_B);
                self.a.ldr(Width::X, T_B, STATE, state::TSC_MUL as i32);
                self.a.mul(Size::X, T_A, T_A, T_B);
                self.a.mov(Size::W, guest_reg(0), T_A);
                self.a.lsr_imm(Size::X, guest_reg(2), T_A, 32);
            }
            0xa0..=0xa3 => {
                if !insn.addr32 {
                    return Step::Unsupported;
                }
                let w = Ow::of(op, op32);
                let m = MemOperand { base: NO_REG, index: NO_REG, scale: 0, disp: insn.imm, default_seg: 3 };
                if !self.ea(insn, &m) {
                    return Step::Unsupported;
                }
                if op < 0xa2 {
                    self.load(w.mem(), T_A);
                    self.write_gpr(w, 0, T_A);
                } else {
                    let v = self.read_gpr(w, 0, T_B);
                    self.store(w.mem(), v);
                }
            }
            0xa8 | 0xa9 => {
                let w = Ow::of(op, op32);
                let a = self.read_gpr(w, 0, T_A);
                self.a.mov_imm(Size::W, T_B, insn.imm as u64 & w.mask());
                self.alu(4, w, a, T_B);
            }
            0xb0..=0xb7 => {
                self.a.mov_imm(Size::W, T_B, insn.imm as u64 & 0xff);
                self.write_r8((op & 7) as usize, T_B);
            }
            0xb8..=0xbf if op32 => self.a.mov_imm(Size::W, guest_reg((op & 7) as usize), insn.imm as u64),
            0xb8..=0xbf => {
                self.a.mov_imm(Size::W, T_B, insn.imm as u64 & 0xffff);
                self.write_gpr(Ow::H, (op & 7) as usize, T_B);
            }
            0xc9 if op32 => {
                let ebp = guest_reg(5);
                self.a.mov(Size::W, T_ADDR, ebp);
                self.load(Width::W, T_A);
                self.a.add_imm(Size::W, guest_reg(4), ebp, 4);
                self.a.mov(Size::W, ebp, T_A);
            }
            0xd3 if op32 && matches!(insn.reg(), 4..=7) => {
                let Some(a) = self.rm_w(insn, Ow::D, true) else { return Step::Unsupported };
                let skip = self.a.label();
                let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_D, guest_reg(1), 31);
                assert!(ok);
                self.a.cbz(Size::W, T_D, skip);
                self.shift_cl(insn.reg(), a);
                self.write_rm(insn, Ow::D, T_RES);
                self.a.bind(skip);
            }
            0xe3 if insn.addr32 => {
                let rel = insn.imm as u8 as i8 as i32 as u32;
                let taken = self.a.label();
                self.a.cbz(Size::W, guest_reg(1), taken);
                self.goto(next);
                self.a.bind(taken);
                self.goto(next.wrapping_add(rel));
                return Step::End;
            }
            0x69 | 0x6b if op32 => {
                let Some(a) = self.rm_w(insn, Ow::D, false) else { return Step::Unsupported };
                let imm = if op == 0x6b { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                self.a.mov(Size::W, T_A, a);
                self.a.mov_imm(Size::W, T_D, imm as u64);
                self.imul32(guest_reg(insn.reg()), T_A, T_D);
            }
            0xc1 | 0xd1 if op32 => {
                let n = if op == 0xc1 { insn.imm } else { 1 };
                let kind = insn.reg();
                if !matches!(kind, 0 | 1 | 4..=7) {
                    return Step::Unsupported;
                }
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.load(Width::W, T_A);
                        self.store(Width::W, T_A);
                        if !self.shift_or_rotate(kind, T_RES, T_A, n) {
                            return Step::Unsupported;
                        }
                        self.store(Width::W, T_RES);
                    }
                    None => {
                        let rg = guest_reg(insn.rm());
                        if !self.shift_or_rotate(kind, rg, rg, n) {
                            return Step::Unsupported;
                        }
                    }
                }
            }
            0xc2 | 0xc3 if op32 => {
                let esp = guest_reg(4);
                self.a.mov(Size::W, T_ADDR, esp);
                self.load(Width::W, T_A);
                let release = 4 + if op == 0xc2 { insn.imm & 0xffff } else { 0 };
                self.add_const(esp, esp, release);
                self.ret_predicted(T_A);
                return Step::End;
            }
            0xc6 if insn.reg() == 0 => {
                self.a.mov_imm(Size::W, T_B, insn.imm as u64 & 0xff);
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.store(Width::B, T_B);
                    }
                    None => self.write_r8(insn.rm(), T_B),
                }
            }
            0xc7 if insn.reg() == 0 => {
                let w = Ow::of(op, op32);
                self.a.mov_imm(Size::W, T_B, insn.imm as u64 & w.mask());
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.store(w.mem(), T_B);
                    }
                    None => self.write_gpr(w, insn.rm(), T_B),
                }
            }
            0xe8 if op32 => {
                self.a.mov_imm(Size::W, T_B, next as u64);
                self.push(T_B);
                self.call_predicted(next, |e| e.goto(next.wrapping_add(insn.imm)));
                return Step::End;
            }
            0xe9 | 0xeb if op32 => {
                let rel = if op == 0xeb { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                self.goto(next.wrapping_add(rel));
                return Step::End;
            }
            0xf6 if insn.reg() == 0 => {
                let Some(a) = self.rm8(insn) else { return Step::Unsupported };
                self.a.mov_imm(Size::W, T_B, insn.imm as u64 & 0xff);
                self.alu8(4, a, T_B);
            }
            0xf7 if !op32 && insn.reg() == 0 => {
                let Some(a) = self.rm_w(insn, Ow::H, false) else { return Step::Unsupported };
                self.a.mov_imm(Size::W, T_B, insn.imm as u64 & 0xffff);
                self.alu(4, Ow::H, a, T_B);
            }
            0xf7 if op32 && !matches!(insn.reg(), 4 | 5) => match insn.reg() {
                6 | 7 => {
                    if !self.div32(insn, eip, insn.reg() == 7) {
                        return Step::Unsupported;
                    }
                }
                0 => {
                    let Some(a) = self.rm32(insn) else { return Step::Unsupported };
                    self.a.mov_imm(Size::W, T_B, insn.imm as u64);
                    self.alu32(4, a, T_B);
                }
                2 | 3 => {
                    let neg = insn.reg() == 3;
                    match insn.mem {
                        Some(m) => {
                            if !self.ea(insn, &m) {
                                return Step::Unsupported;
                            }
                            self.load(Width::W, T_A);
                            self.store(Width::W, T_A);
                            self.not_neg(neg, T_RES, T_A);
                            self.store(Width::W, T_RES);
                        }
                        None => {
                            let rg = guest_reg(insn.rm());
                            self.not_neg(neg, rg, rg);
                        }
                    }
                }
                _ => return Step::Unsupported,
            },
            0xff if op32 => match insn.reg() {
                0 | 1 => match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.load(Width::W, T_A);
                        self.store(Width::W, T_A);
                        self.inc_dec(insn.reg() == 1, T_RES, T_A);
                        self.store(Width::W, T_RES);
                    }
                    None => {
                        let rg = guest_reg(insn.rm());
                        self.inc_dec(insn.reg() == 1, rg, rg);
                    }
                },
                2 | 4 => {
                    let Some(t) = self.rm32(insn) else { return Step::Unsupported };
                    self.a.mov(Size::W, T_C, t);
                    if insn.reg() == 2 {
                        self.a.mov_imm(Size::W, T_B, next as u64);
                        self.push(T_B);
                        self.a.mov(Size::W, r(0), T_C);
                        self.call_predicted(next, |e| e.goto_indirect(r(0)));
                    } else {
                        self.goto_indirect(T_C);
                    }
                    return Step::End;
                }
                6 => {
                    let Some(v) = self.rm32(insn) else { return Step::Unsupported };
                    self.a.mov(Size::W, T_B, v);
                    self.push(T_B);
                }
                _ => return Step::Unsupported,
            },
            0x140..=0x14f if op32 => {
                let Some(v) = self.rm32(insn) else { return Step::Unsupported };
                let dst = guest_reg(insn.reg());
                match x86_cond(op) {
                    Some(c) => self.a.csel(Size::W, dst, v, dst, c),
                    None => {
                        let skip = self.a.label();
                        self.parity_odd(T_B);
                        if op & 1 == 0 {
                            self.a.tbnz(T_B, 0, skip);
                        } else {
                            self.a.tbz(T_B, 0, skip);
                        }
                        self.a.mov(Size::W, dst, v);
                        self.a.bind(skip);
                    }
                }
            }
            0x190..=0x19f => {
                match x86_cond(op) {
                    Some(c) => self.a.cset(Size::W, T_B, c),
                    None => {
                        self.parity_odd(T_B);
                        let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_B, T_B, 1);
                        assert!(ok);
                        if op & 1 == 0 {
                            let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
                            assert!(ok);
                        }
                    }
                }
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.store(Width::B, T_B);
                    }
                    None => self.write_r8(insn.rm(), T_B),
                }
            }
            0x1af if op32 => {
                let Some(b) = self.rm_w(insn, Ow::D, false) else { return Step::Unsupported };
                let dst = guest_reg(insn.reg());
                self.imul32(dst, dst, b);
            }
            0x1b6 | 0x1be => {
                let signed = op == 0x1be;
                let to = if op32 { guest_reg(insn.reg()) } else { T_A };
                match insn.mem {
                    Some(m) => {
                        if !self.ea_fold(insn, &m) {
                            return Step::Unsupported;
                        }
                        if signed {
                            self.load_signed(Width::B, to);
                        } else {
                            self.load(Width::B, to);
                        }
                    }
                    None => {
                        self.read_r8(insn.rm(), to);
                        if signed {
                            self.a.sxtb(Size::W, to, to);
                        }
                    }
                }
                if !op32 {
                    self.a.bfi(Size::W, guest_reg(insn.reg()), T_A, 0, 16);
                }
            }
            0x1b7 | 0x1bf if op32 => {
                let signed = op == 0x1bf;
                match insn.mem {
                    Some(m) => {
                        if !self.ea_fold(insn, &m) {
                            return Step::Unsupported;
                        }
                        if signed {
                            self.load_signed(Width::H, T_A);
                        } else {
                            self.load(Width::H, T_A);
                        }
                    }
                    None => {
                        let src = guest_reg(insn.rm());
                        if signed {
                            self.a.sxth(Size::W, T_A, src);
                        } else {
                            self.a.uxth(T_A, src);
                        }
                    }
                }
                self.a.mov(Size::W, guest_reg(insn.reg()), T_A);
            }
            0xd3 if op32 && insn.reg() < 2 => {
                let Some(a) = self.rm_w(insn, Ow::D, true) else { return Step::Unsupported };
                let skip = self.a.label();
                let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_D, guest_reg(1), 31);
                assert!(ok);
                self.a.cbz(Size::W, T_D, skip);
                if insn.reg() == 0 {
                    self.a.neg(Size::W, T_D, T_D);
                }
                self.a.rorv(Size::W, T_RES, a, T_D);
                self.rotate_flags(insn.reg() == 0);
                self.write_rm(insn, Ow::D, T_RES);
                self.a.bind(skip);
            }
            0xf6 | 0xf7 if matches!(insn.reg(), 2 | 3) && !(op == 0xf7 && op32) => {
                let w = Ow::of(op, op32);
                let Some(a) = self.rm_w(insn, w, false) else { return Step::Unsupported };
                if insn.reg() == 3 {
                    self.alu_narrow(5, ZR, a, if w == Ow::B { 24 } else { 16 });
                } else {
                    self.a.mvn(Size::W, T_RES, a);
                }
                self.write_rm(insn, w, T_RES);
            }
            0x40..=0x4f => {
                let n = (op & 7) as usize;
                let a = self.read_gpr(Ow::H, n, T_A);
                self.inc_dec_narrow(op >= 0x48, a, 16);
                self.write_gpr(Ow::H, n, T_RES);
            }
            0xff if insn.reg() < 2 => {
                let Some(a) = self.rm_w(insn, Ow::H, false) else { return Step::Unsupported };
                self.inc_dec_narrow(insn.reg() == 1, a, 16);
                self.write_rm(insn, Ow::H, T_RES);
            }
            0x1c0 | 0x1c1 if op == 0x1c0 || op32 => {
                let w = Ow::of(op, op32);
                let Some(a) = self.rm_w(insn, w, true) else { return Step::Unsupported };
                self.a.mov(Size::W, T_E, a);
                let b = self.read_gpr(w, insn.reg(), T_B);
                self.alu(0, w, T_E, b);
                // The destination is written last, but a store must not see a new base.
                if insn.mem.is_some() {
                    self.write_rm(insn, w, T_RES);
                    self.write_gpr(w, insn.reg(), T_E);
                } else {
                    self.write_gpr(w, insn.reg(), T_E);
                    self.write_rm(insn, w, T_RES);
                }
            }
            0x1b0 | 0x1b1 if op == 0x1b0 || op32 => {
                let w = Ow::of(op, op32);
                let Some(a) = self.rm_w(insn, w, true) else { return Step::Unsupported };
                self.a.mov(Size::W, T_E, a);
                let acc = self.read_gpr(w, 0, T_B);
                self.need = flags::ALL;
                self.alu(7, w, acc, T_E);
                let (differ, done) = (self.a.label(), self.a.label());
                self.a.b_cond(Cond::Ne, differ);
                let src = self.read_gpr(w, insn.reg(), T_D);
                self.write_rm(insn, w, src);
                self.a.b(done);
                self.a.bind(differ);
                self.write_gpr(w, 0, T_E);
                self.a.bind(done);
            }
            0x1c7 if insn.reg() == 1 => {
                let Some(m) = insn.mem else { return Step::Unsupported };
                if !self.ea(insn, &m) {
                    return Step::Unsupported;
                }
                self.a.ldr(Width::W, T_A, T_ADDR, 0);
                self.a.ldr(Width::W, T_B, T_ADDR, 4);
                self.a.str(Width::W, T_A, T_ADDR, 0);
                self.a.str(Width::W, T_B, T_ADDR, 4);
                self.a.eor(Size::W, T_C, T_A, guest_reg(0));
                self.a.eor(Size::W, T_D, T_B, guest_reg(2));
                self.a.orr(Size::W, T_C, T_C, T_D);
                let (differ, done) = (self.a.label(), self.a.label());
                self.a.cbnz(Size::W, T_C, differ);
                self.a.str(Width::W, guest_reg(3), T_ADDR, 0);
                self.a.str(Width::W, guest_reg(1), T_ADDR, 4);
                self.a.movz(Size::W, T_C, 4, 0);
                self.a.b(done);
                self.a.bind(differ);
                self.a.mov(Size::W, guest_reg(0), T_A);
                self.a.mov(Size::W, guest_reg(2), T_B);
                self.a.mov(Size::W, T_C, ZR);
                self.a.bind(done);
                self.a.rmif(T_C, 0, 0b0100);
            }
            0x1a4 | 0x1a5 | 0x1ac | 0x1ad if op32 => {
                let left = op < 0x1a8;
                let Some(dst) = self.rm_w(insn, Ow::D, true) else { return Step::Unsupported };
                self.a.mov(Size::W, T_E, dst);
                let src = guest_reg(insn.reg());
                if op & 1 == 0 {
                    let n = insn.imm & 31;
                    if n != 0 {
                        if left {
                            self.a.extr(Size::W, T_RES, T_E, src, 32 - n);
                            self.a.ubfx(Size::W, T_B, T_E, 32 - n, 1);
                        } else {
                            self.a.extr(Size::W, T_RES, src, T_E, n);
                            self.a.ubfx(Size::W, T_B, T_E, n - 1, 1);
                        }
                        self.double_shift_done(insn);
                    }
                } else {
                    let skip = self.a.label();
                    let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_D, guest_reg(1), 31);
                    assert!(ok);
                    self.a.cbz(Size::W, T_D, skip);
                    self.a.movz(Size::W, T_C, 32, 0);
                    self.a.sub(Size::W, T_C, T_C, T_D);
                    if left {
                        self.a.lslv(Size::W, T_RES, T_E, T_D);
                        self.a.lsrv(Size::W, T_A, src, T_C);
                        self.a.lsrv(Size::W, T_B, T_E, T_C);
                    } else {
                        self.a.lsrv(Size::W, T_RES, T_E, T_D);
                        self.a.lslv(Size::W, T_A, src, T_C);
                        self.a.sub_imm(Size::W, T_C, T_D, 1);
                        self.a.lsrv(Size::W, T_B, T_E, T_C);
                    }
                    self.a.orr(Size::W, T_RES, T_RES, T_A);
                    let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_B, T_B, 1);
                    assert!(ok);
                    self.double_shift_done(insn);
                    self.a.bind(skip);
                }
            }
            0x1c8..=0x1cf if op32 => {
                let r = guest_reg((op & 7) as usize);
                self.a.rev(Size::W, r, r);
            }
            0xc0 | 0xd0 | 0xd2 => {
                let count = match op {
                    0xc0 => Some(insn.imm),
                    0xd0 => Some(1),
                    _ => None,
                };
                if !self.shift_narrow(insn, Ow::B, insn.reg(), count) {
                    return Step::Unsupported;
                }
            }
            0xc1 | 0xd1 | 0xd3 if !op32 => {
                let count = match op {
                    0xc1 => Some(insn.imm),
                    0xd1 => Some(1),
                    _ => None,
                };
                if !self.shift_narrow(insn, Ow::H, insn.reg(), count) {
                    return Step::Unsupported;
                }
            }
            0x9e => {
                self.a.ubfx(Size::W, T_A, guest_reg(0), 8, 8);
                self.a.ubfx(Size::W, T_C, T_A, 7, 1);
                self.a.ubfx(Size::W, T_B, T_A, 6, 1);
                self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_B, T_C, Shift::Lsl, 1);
                self.a.ubfx(Size::W, T_B, T_A, 0, 1);
                let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
                assert!(ok);
                self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_B, T_C, Shift::Lsl, 1);
                self.a.lsl_imm(Size::W, T_C, T_C, 1);
                self.a.rmif(T_C, 0, 0b1110);
                self.a.ubfx(Size::W, T_B, T_A, 2, 1);
                let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, PF_RESULT, T_B, 1);
                assert!(ok);
                let ok = self.a.logic_imm(a64::Logic::And, Size::W, AF_AUX, T_A, x86::cpu::flags::AF as u64);
                assert!(ok);
            }
            0x9f => {
                self.arith_flags(T_A);
                self.a.bfi(Size::W, guest_reg(0), T_A, 8, 8);
            }
            0x9c if op32 => self.pushf(),
            0x9d if op32 => return self.popf(eip, next),
            0x1ba if insn.reg() >= 4 => {
                let w = if op32 { Ow::D } else { Ow::H };
                let bit = insn.imm & if op32 { 31 } else { 15 };
                let kind = insn.reg() - 4;
                let Some(a) = self.rm_w(insn, w, kind != 0) else { return Step::Unsupported };
                self.a.ubfx(Size::W, T_B, a, bit, 1);
                if kind != 0 {
                    let (logic, imm) = match kind {
                        1 => (a64::Logic::Orr, 1u64 << bit),
                        2 => (a64::Logic::And, !(1u64 << bit) & 0xffff_ffff),
                        _ => (a64::Logic::Eor, 1u64 << bit),
                    };
                    let ok = self.a.logic_imm(logic, Size::W, T_RES, a, imm);
                    assert!(ok);
                    self.write_rm(insn, w, T_RES);
                }
                let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
                assert!(ok);
                self.a.lsl_imm(Size::W, T_B, T_B, 1);
                self.a.rmif(T_B, 0, 0b0010);
            }
            0xf5 => self.a.cfinv(),
            0xf8 | 0xf9 => {
                self.a.movz(Size::W, T_B, if op == 0xf8 { 2 } else { 0 }, 0);
                self.a.rmif(T_B, 0, 0b0010);
            }
            0x06 | 0x0e | 0x1e | 0x1a0 | 0x1a8 if op32 => {
                let at = match op {
                    0x06 => state::ES_SEL,
                    0x0e => state::CS_SEL,
                    0x1e => state::DS_SEL,
                    0x1a0 => state::FS_SEL,
                    _ => state::GS_SEL,
                };
                self.a.ldr(Width::W, T_A, STATE, at as i32);
                self.push(T_A);
            }
            0x8f if op32 && insn.reg() == 0 => {
                let esp = guest_reg(4);
                match insn.mem {
                    Some(m) if m.base != 4 && m.index != 4 => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.a.mov(Size::X, T_E, T_ADDR);
                        self.load(Width::W, T_B);
                        self.store(Width::W, T_B);
                        self.a.mov(Size::W, T_ADDR, esp);
                        self.load(Width::W, T_A);
                        self.a.mov(Size::X, T_ADDR, T_E);
                        self.store(Width::W, T_A);
                        self.a.add_imm(Size::W, esp, esp, 4);
                    }
                    Some(_) => return Step::Unsupported,
                    None => {
                        self.a.mov(Size::W, T_ADDR, esp);
                        self.load(Width::W, T_A);
                        self.a.add_imm(Size::W, esp, esp, 4);
                        self.a.mov(Size::W, guest_reg(insn.rm()), T_A);
                    }
                }
            }
            0x60 if op32 => {
                let esp = guest_reg(4);
                self.a.mov(Size::W, T_B, esp);
                for r in 0..8 {
                    self.a.sub_imm(Size::W, T_ADDR, T_B, 4 * (r as u32 + 1));
                    self.store(Width::W, if r == 4 { T_B } else { guest_reg(r) });
                }
                self.a.mov(Size::W, esp, T_ADDR);
            }
            0x61 if op32 => {
                let esp = guest_reg(4);
                for r in (0..8).filter(|&r| r != 4) {
                    self.a.add_imm(Size::W, T_ADDR, esp, 28 - 4 * r as u32);
                    self.load(Width::W, guest_reg(r));
                }
                self.a.add_imm(Size::W, esp, esp, 32);
            }
            0x91..=0x97 if op32 => {
                let r = guest_reg((op & 7) as usize);
                self.a.mov(Size::W, T_A, guest_reg(0));
                self.a.mov(Size::W, guest_reg(0), r);
                self.a.mov(Size::W, r, T_A);
            }
            0xf7 if op32 && matches!(insn.reg(), 4 | 5) => {
                let Some(b) = self.rm32(insn) else { return Step::Unsupported };
                let (eax, edx) = (guest_reg(0), guest_reg(2));
                let signed = insn.reg() == 5;
                if signed {
                    self.a.smull(T_RES, eax, b);
                } else {
                    self.a.umull(T_RES, eax, b);
                }
                if self.need != 0 {
                    if signed {
                        self.a.addsub_ext(Size::X, true, true, ZR, T_RES, T_RES, Extend::Sxtw, 0);
                    } else {
                        self.a.lsr_imm(Size::X, T_C, T_RES, 32);
                        self.a.cmp_imm(Size::X, T_C, 0);
                    }
                    self.a.cset(Size::W, T_B, Cond::Ne);
                    self.a.tst(Size::W, T_RES, T_RES);
                    self.a.movz(Size::W, T_C, 2, 0);
                    self.a.sub(Size::W, T_C, T_C, T_B);
                    self.a.rmif(T_C, 0, 0b0011);
                    self.a.mov(Size::W, PF_RESULT, T_RES);
                    self.a.mov(Size::W, AF_AUX, ZR);
                }
                self.a.lsr_imm(Size::X, edx, T_RES, 32);
                self.a.mov(Size::W, eax, T_RES);
            }
            0x1bc | 0x1bd if op32 => {
                let Some(v) = self.rm_w(insn, Ow::D, false) else { return Step::Unsupported };
                let (zero, done) = (self.a.label(), self.a.label());
                self.a.cbz(Size::W, v, zero);
                if op == 0x1bc {
                    self.a.rbit(Size::W, T_B, v);
                    self.a.clz(Size::W, guest_reg(insn.reg()), T_B);
                } else {
                    self.a.clz(Size::W, T_B, v);
                    self.a.movz(Size::W, T_C, 31, 0);
                    self.a.sub(Size::W, guest_reg(insn.reg()), T_C, T_B);
                }
                self.a.movz(Size::W, T_C, 0, 0);
                self.a.rmif(T_C, 0, 0b0100);
                self.a.b(done);
                self.a.bind(zero);
                self.a.movz(Size::W, T_C, 4, 0);
                self.a.rmif(T_C, 0, 0b0100);
                self.a.bind(done);
            }
            0x07 | 0x1f | 0x1a1 | 0x1a9 => {
                let seg = match op {
                    0x07 => 0,
                    0x1f => 3,
                    0x1a1 => 4,
                    _ => 5,
                };
                if !self.pop_seg(insn, eip, seg) {
                    return Step::Unsupported;
                }
            }
            0x8c if insn.reg() <= 5 => {
                let at = [state::ES_SEL, state::CS_SEL, state::SS_SEL, state::DS_SEL, state::FS_SEL, state::GS_SEL][insn.reg()];
                self.a.ldr(Width::W, T_B, STATE, at as i32);
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.store(Width::H, T_B);
                    }
                    None => self.write_gpr(if op32 { Ow::D } else { Ow::H }, insn.rm(), T_B),
                }
            }
            0x8e => {
                if !self.mov_to_seg(insn, eip) {
                    return Step::Unsupported;
                }
            }
            0xcf => return self.iret(insn, eip),
            0x121 | 0x123 => {
                if !self.mov_dr(insn, op == 0x123) {
                    return Step::Unsupported;
                }
            }
            0x100 if insn.reg() == 2 => {
                if !self.lldt(insn, eip) {
                    return Step::Unsupported;
                }
            }
            0x134 => return self.sysenter(eip),
            0x135 => return self.sysexit(eip),
            0x122 if insn.reg() == 0 && insn.mem.is_none() => {
                if !self.write_cr0_ts(Some(guest_reg(insn.rm())), eip) {
                    return Step::Unsupported;
                }
            }
            0x101 if insn.reg() == 7 && insn.mem.is_some() => {
                if !self.invlpg(insn, eip) {
                    return Step::Unsupported;
                }
            }
            0x122 if insn.reg() == 3 && insn.mem.is_none() => {
                if !self.write_cr3(guest_reg(insn.rm()), eip) {
                    return Step::Unsupported;
                }
            }
            0x106 => {
                if !self.write_cr0_ts(None, eip) {
                    return Step::Unsupported;
                }
            }
            0x120 => {
                if !self.mov_from_cr(insn) {
                    return Step::Unsupported;
                }
            }
            0x1ae if insn.mem.is_none() && matches!(insn.modrm, 0xe8 | 0xf0 | 0xf8) => self.a.dmb(0xb),
            0x1ae if matches!(insn.reg(), 2 | 3) && insn.rep == Rep::None && !insn.opsize_prefix => {
                let Some(m) = insn.mem else { return Step::Unsupported };
                self.leave_x87_fp();
                if !self.ea(insn, &m) {
                    return Step::Unsupported;
                }
                let (slow, done) = (self.a.label(), self.a.label());
                self.a.ldr(Width::W, T_B, STATE, state::SSE_OK as i32);
                self.a.cbz(Size::W, T_B, slow);
                if insn.reg() == 3 {
                    self.a.mrs(T_B, a64::sysreg::FPSR);
                    self.a.ldr(Width::W, T_C, STATE, state::FPSR as i32);
                    self.a.orr(Size::W, T_B, T_B, T_C);
                    self.a.ldr(Width::W, T_C, STATE, state::MXCSR as i32);
                    for (bit, to) in [(0, 0), (1, 2), (2, 3), (3, 4), (4, 5)] {
                        self.a.ubfx(Size::W, T_D, T_B, bit, 1);
                        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_C, T_D, Shift::Lsl, to);
                    }
                    self.store(Width::W, T_C);
                } else {
                    // Only a load that keeps the control bits stays here: FPCR,
                    // SSE_FP_OK and the x87 rounding check all follow them.
                    self.load(Width::W, T_B);
                    self.a.ldr(Width::W, T_C, STATE, state::MXCSR as i32);
                    self.a.eor(Size::W, T_C, T_C, T_B);
                    self.a.lsr_imm(Size::W, T_C, T_C, 6);
                    self.a.cbnz(Size::W, T_C, slow);
                    self.a.str(Width::W, T_B, STATE, state::MXCSR as i32);
                    self.a.str(Width::W, ZR, STATE, state::FPSR as i32);
                    self.a.msr(a64::sysreg::FPSR, ZR);
                }
                self.cold_exit(slow, eip);
                self.a.bind(done);
            }
            0x1a3 | 0x1ab | 0x1b3 | 0x1bb if op32 => {
                let kind = (op as usize >> 3) & 3;
                let idx = guest_reg(insn.reg());
                let a = match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.a.asr_imm(Size::W, T_B, idx, 5);
                        self.a.add_shift(Size::W, T_ADDR, T_ADDR, T_B, Shift::Lsl, 2);
                        self.load(Width::W, T_A);
                        T_A
                    }
                    None => guest_reg(insn.rm()),
                };
                let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_C, idx, 31);
                assert!(ok);
                self.a.lsrv(Size::W, T_B, a, T_C);
                if kind != 0 {
                    self.a.movz(Size::W, T_D, 1, 0);
                    self.a.lslv(Size::W, T_D, T_D, T_C);
                    let (logic, invert) = match kind {
                        1 => (a64::Logic::Orr, false),
                        2 => (a64::Logic::And, true),
                        _ => (a64::Logic::Eor, false),
                    };
                    self.a.logic_shift(logic, invert, Size::W, T_RES, a, T_D, Shift::Lsl, 0);
                    match insn.mem {
                        Some(_) => self.store(Width::W, T_RES),
                        None => self.a.mov(Size::W, guest_reg(insn.rm()), T_RES),
                    }
                }
                let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_B, T_B, 1);
                assert!(ok);
                let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
                assert!(ok);
                self.a.lsl_imm(Size::W, T_B, T_B, 1);
                self.a.rmif(T_B, 0, 0b0010);
            }
            0xdb if insn.modrm == 0xe3 => self.fpu_reset(eip, true),
            0x177 if !insn.opsize_prefix && insn.rep == Rep::None => self.fpu_reset(eip, false),
            0xd8..=0xdf => {
                if !self.x87(insn, eip) {
                    return Step::Unsupported;
                }
            }
            0x9b => self.fwait(eip),
            0x118 if insn.mem.is_some() && insn.reg() < 4 => {}
            0x11f => {}
            0x160..=0x17f | 0x1d0..=0x1ff if !insn.opsize_prefix && insn.rep == Rep::None => {
                if !self.mmx(insn, eip) {
                    return Step::Unsupported;
                }
            }
            0x12b if insn.rep == Rep::None => {
                let Some(m) = insn.mem else { return Step::Unsupported };
                if !self.ea(insn, &m) {
                    return Step::Unsupported;
                }
                let (slow, done) = (self.a.label(), self.a.label());
                self.a.ldr(Width::W, T_B, STATE, state::SSE_OK as i32);
                self.a.cbz(Size::W, T_B, slow);
                let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_B, T_ADDR, 15);
                assert!(ok);
                self.a.cbnz(Size::W, T_B, slow);
                let x = (state::XMM + 16 * insn.reg()) as i32;
                self.a.ldr(Width::X, T_B, STATE, x);
                self.a.ldr(Width::X, T_C, STATE, x + 8);
                self.a.str(Width::X, T_B, T_ADDR, 0);
                self.a.str(Width::X, T_C, T_ADDR, 8);
                self.cold_exit(slow, eip);
                self.a.bind(done);
            }
            _ => return Step::Unsupported,
        }
        Step::Next
    }

    /// 8- or 16-bit group-2 shift or rotate (not RCL/RCR) by `count`, or by CL when None.
    fn shift_narrow(&mut self, insn: &Insn, w: Ow, kind: usize, count: Option<u32>) -> bool {
        if !matches!(kind, 0 | 1 | 4..=7) {
            return false;
        }
        let bits = if w == Ow::B { 8 } else { 16 };
        if count.is_some_and(|n| n & 31 == 0) {
            return insn.mem.is_none() || self.rm_w(insn, w, false).is_some();
        }
        let Some(a) = self.rm_w(insn, w, true) else { return false };
        if a != T_A {
            self.a.mov(Size::W, T_A, a);
        }
        let skip = self.a.label();
        match count {
            Some(n) => self.a.movz(Size::W, T_D, n & 31, 0),
            None => {
                let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_D, guest_reg(1), 31);
                assert!(ok);
                self.a.cbz(Size::W, T_D, skip);
            }
        }
        match kind {
            0 | 1 => {
                let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_E, T_D, bits as u64 - 1);
                assert!(ok);
                self.a.movz(Size::W, T_C, bits, 0);
                self.a.sub(Size::W, T_C, T_C, T_E);
                if kind == 0 {
                    self.a.lslv(Size::W, T_RES, T_A, T_E);
                    self.a.lsrv(Size::W, T_B, T_A, T_C);
                } else {
                    self.a.lsrv(Size::W, T_RES, T_A, T_E);
                    self.a.lslv(Size::W, T_B, T_A, T_C);
                }
                self.a.orr(Size::W, T_RES, T_RES, T_B);
                self.a.ubfx(Size::W, T_RES, T_RES, 0, bits);
                if kind == 0 {
                    self.a.ubfx(Size::W, T_B, T_RES, 0, 1);
                    self.a.ubfx(Size::W, T_C, T_RES, bits - 1, 1);
                } else {
                    self.a.ubfx(Size::W, T_B, T_RES, bits - 1, 1);
                    self.a.ubfx(Size::W, T_C, T_RES, bits - 2, 1);
                }
                self.a.eor(Size::W, T_C, T_C, T_B);
                self.cf_of_flags();
                self.write_rm(insn, w, T_RES);
                self.a.bind(skip);
                return true;
            }
            4 | 6 => {
                self.a.lslv(Size::W, T_RES, T_A, T_D);
                self.a.ubfx(Size::W, T_B, T_RES, bits, 1);
                self.a.ubfx(Size::W, T_RES, T_RES, 0, bits);
                self.a.ubfx(Size::W, T_C, T_RES, bits - 1, 1);
                self.a.eor(Size::W, T_C, T_C, T_B);
            }
            5 => {
                self.a.ubfx(Size::W, T_C, T_A, bits - 1, 1);
                self.a.lsrv(Size::W, T_RES, T_A, T_D);
                self.a.sub_imm(Size::W, T_E, T_D, 1);
                self.a.lsrv(Size::W, T_B, T_A, T_E);
            }
            _ => {
                self.a.sbfx(Size::W, T_E, T_A, 0, bits);
                self.a.asrv(Size::W, T_RES, T_E, T_D);
                self.a.sub_imm(Size::W, T_C, T_D, 1);
                self.a.asrv(Size::W, T_B, T_E, T_C);
                self.a.ubfx(Size::W, T_RES, T_RES, 0, bits);
                self.a.mov(Size::W, T_C, ZR);
            }
        }
        let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_B, T_B, 1);
        assert!(ok);
        self.a.lsl_imm(Size::W, T_E, T_RES, 32 - bits);
        self.a.tst(Size::W, T_E, T_E);
        self.cf_of_flags();
        self.a.mov(Size::W, PF_RESULT, T_RES);
        self.write_rm(insn, w, T_RES);
        self.a.bind(skip);
        true
    }

    /// Sets ARM C and V from x86 CF in T_B and OF in T_C (each 0 or 1); N and Z stay.
    fn cf_of_flags(&mut self) {
        let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
        assert!(ok);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_C, T_B, Shift::Lsl, 1);
        self.a.rmif(T_C, 0, 0b0011);
    }

    fn pushf(&mut self) {
        use x86::cpu::flags;
        self.a.ldr(Width::W, T_C, STATE, state::EFLAGS_REST as i32);
        let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_C, T_C, !(flags::RF as u64) & 0xffff_ffff);
        assert!(ok);
        self.arith_flags(T_A);
        self.a.orr(Size::W, T_A, T_A, T_C);
        self.a.cset(Size::W, T_B, Cond::Vs);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_A, T_A, T_B, Shift::Lsl, 11);
        self.a.ldr(Width::W, T_B, STATE, state::IF_FLAG as i32);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_A, T_A, T_B, Shift::Lsl, 9);
        self.a.ldr(Width::W, T_B, STATE, state::DF as i32);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_A, T_A, T_B, Shift::Lsl, 10);
        self.push(T_A);
    }

    /// SF, ZF, AF, PF and CF in `rd` where EFLAGS keeps them, with the
    /// reserved bit 1 set: what LAHF puts in AH. Clobbers T_B.
    fn arith_flags(&mut self, rd: Reg) {
        use x86::cpu::flags;
        self.a.movz(Size::W, rd, 2, 0);
        for (c, bit) in [(Cond::Mi, 7), (Cond::Eq, 6), (Cond::Lo, 0)] {
            self.a.cset(Size::W, T_B, c);
            self.a.logic_shift(a64::Logic::Orr, false, Size::W, rd, rd, T_B, Shift::Lsl, bit);
        }
        self.a.ubfx(Size::W, T_B, PF_RESULT, 0, 8);
        for sh in [4, 2, 1] {
            self.a.logic_shift(a64::Logic::Eor, false, Size::W, T_B, T_B, T_B, Shift::Lsr, sh);
        }
        let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_B, T_B, 1);
        assert!(ok);
        let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
        assert!(ok);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, rd, rd, T_B, Shift::Lsl, 2);
        let ok = self.a.logic_imm(a64::Logic::And, Size::W, T_B, AF_AUX, flags::AF as u64);
        assert!(ok);
        self.a.orr(Size::W, rd, rd, T_B);
    }

    /// POPF that leaves the system flags alone; anything else goes to the
    /// interpreter. In kernel mode IF may change, so the block ends at the dispatcher.
    fn popf(&mut self, eip: u32, next: u32) -> Step {
        let esp = guest_reg(4);
        let slow = self.a.label();
        self.a.mov(Size::W, T_ADDR, esp);
        self.load(Width::W, T_A);
        self.load_eflags(slow, !self.user);
        self.a.add_imm(Size::W, esp, esp, 4);
        if self.user {
            let done = self.a.label();
            self.cold_exit(slow, eip);
            self.a.bind(done);
            return Step::Next;
        }
        self.goto_dispatch(next);
        self.a.bind(slow);
        self.exit_interp(eip);
        Step::End
    }

    /// Loads EFLAGS from T_A as POPF does (IF and IOPL only with `cpl0`), or
    /// branches to `slow` before changing anything if TF would change.
    fn load_eflags(&mut self, slow: Label, cpl0: bool) {
        use x86::cpu::flags;
        let mut rest = flags::NT | flags::RF | flags::AC | flags::ID;
        if cpl0 {
            rest |= flags::IOPL;
        }
        self.a.ldr(Width::W, T_B, STATE, state::EFLAGS_REST as i32);
        self.a.eor(Size::W, T_C, T_B, T_A);
        self.a.tbnz(T_C, flags::TF.trailing_zeros(), slow);
        self.a.mov_imm(Size::W, T_C, rest as u64);
        self.a.bic(Size::W, T_B, T_B, T_C);
        self.a.and(Size::W, T_C, T_A, T_C);
        self.a.orr(Size::W, T_B, T_B, T_C);
        self.a.str(Width::W, T_B, STATE, state::EFLAGS_REST as i32);
        self.a.ubfx(Size::W, T_C, T_A, 7, 1);
        self.a.ubfx(Size::W, T_B, T_A, 6, 1);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_B, T_C, Shift::Lsl, 1);
        self.a.ubfx(Size::W, T_B, T_A, 0, 1);
        let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
        assert!(ok);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_B, T_C, Shift::Lsl, 1);
        self.a.ubfx(Size::W, T_B, T_A, 11, 1);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_B, T_C, Shift::Lsl, 1);
        self.a.rmif(T_C, 0, 0b1111);
        self.a.ubfx(Size::W, T_B, T_A, 2, 1);
        let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, PF_RESULT, T_B, 1);
        assert!(ok);
        let ok = self.a.logic_imm(a64::Logic::And, Size::W, AF_AUX, T_A, flags::AF as u64);
        assert!(ok);
        self.a.ubfx(Size::W, T_B, T_A, 10, 1);
        self.a.str(Width::W, T_B, STATE, state::DF as i32);
        if cpl0 {
            self.a.ubfx(Size::W, T_B, T_A, 9, 1);
            self.a.str(Width::W, T_B, STATE, state::IF_FLAG as i32);
        }
    }

    fn mm_off(n: usize) -> i32 {
        (state::MM + 8 * n) as i32
    }

    /// The `rm` MMX operand into `dst` (64 bits, or the low 32 when `half`).
    fn mm_src_v(&mut self, insn: &Insn, dst: V, half: bool) -> bool {
        match insn.mem {
            Some(m) => {
                if !self.ea(insn, &m) {
                    return false;
                }
                // Through a general register: SIMD loads from MMIO give the
                // host no syndrome to emulate them with.
                self.a.ldr(if half { Width::W } else { Width::X }, T_A, T_ADDR, 0);
                self.a.fmov_dx(dst, T_A);
            }
            None => self.a.ldr_d(dst, STATE, Self::mm_off(insn.rm())),
        }
        true
    }

    /// MMX instructions; at run time they leave faulting cases (CR0.EM/TS, a
    /// pending x87 exception) to the interpreter.
    fn mmx(&mut self, insn: &Insn, eip: u32) -> bool {
        self.x87_flush();
        self.x87 = x87::X87Block::default();
        const VA: V = V(16);
        const VB: V = V(17);
        let (op, reg, rm) = (insn.op, insn.reg(), insn.rm());
        let three = simd_three;
        let supported = matches!(op, 0x16e | 0x17e | 0x16f | 0x17f | 0x1db | 0x1df | 0x1eb | 0x1ef | 0x1d4 | 0x1fb)
            || three(op).is_some()
            || matches!(op, 0x160..=0x163 | 0x167..=0x16b | 0x1e4 | 0x1e5 | 0x1f5)
            || (matches!(op, 0x171 | 0x172) && matches!(reg, 2 | 4 | 6) && insn.mem.is_none())
            || (op == 0x173 && matches!(reg, 2 | 6) && insn.mem.is_none());
        if !supported {
            return false;
        }
        let (ok, slow) = (self.a.label(), self.a.label());
        self.a.ldr(Width::W, T_B, STATE, state::MMX_OK as i32);
        self.a.cbz(Size::W, T_B, slow);
        self.a.ldr(Width::W, T_B, STATE, state::FP80_STALE as i32);
        self.a.cbz(Size::W, T_B, ok);
        let off = self.branch_offset(self.unstale_va);
        self.a.bl_offset(off);
        self.cold_exit(slow, eip);
        self.a.bind(ok);
        let mut written = None;
        match op {
            0x16e => {
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return false;
                        }
                        self.a.ldr(Width::W, T_A, T_ADDR, 0);
                    }
                    None => self.a.mov(Size::W, T_A, guest_reg(rm)),
                }
                self.a.str(Width::X, T_A, STATE, Self::mm_off(reg));
                written = Some(reg);
            }
            0x17e => {
                self.a.ldr(Width::X, T_A, STATE, Self::mm_off(reg));
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return false;
                        }
                        self.store(Width::W, T_A);
                    }
                    None => self.a.mov(Size::W, guest_reg(rm), T_A),
                }
            }
            0x16f => {
                if !self.mm_src_v(insn, VA, false) {
                    return false;
                }
                self.a.str_d(VA, STATE, Self::mm_off(reg));
                written = Some(reg);
            }
            0x17f => {
                self.a.ldr_d(VA, STATE, Self::mm_off(reg));
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return false;
                        }
                        self.a.fmov_xd(T_A, VA);
                        self.a.str(Width::X, T_A, T_ADDR, 0);
                    }
                    None => {
                        self.a.str_d(VA, STATE, Self::mm_off(rm));
                        written = Some(rm);
                    }
                }
            }
            0x1db | 0x1df | 0x1eb | 0x1ef | 0x1d4 | 0x1fb => {
                if !self.mm_src_v(insn, VB, false) {
                    return false;
                }
                self.a.fmov_xd(T_A, VB);
                self.a.ldr(Width::X, T_C, STATE, Self::mm_off(reg));
                match op {
                    0x1db => self.a.and(Size::X, T_C, T_C, T_A),
                    0x1df => self.a.bic(Size::X, T_C, T_A, T_C),
                    0x1eb => self.a.orr(Size::X, T_C, T_C, T_A),
                    0x1d4 => self.a.add(Size::X, T_C, T_C, T_A),
                    0x1fb => self.a.sub(Size::X, T_C, T_C, T_A),
                    _ => self.a.eor(Size::X, T_C, T_C, T_A),
                }
                self.a.str(Width::X, T_C, STATE, Self::mm_off(reg));
                written = Some(reg);
            }
            0x171 | 0x172 => {
                let esize = if op == 0x171 { 16 } else { 32 };
                let n = insn.imm & 0xff;
                self.a.ldr_d(VA, STATE, Self::mm_off(rm));
                match reg {
                    6 if n >= esize => self.a.vec3(false, true, 0, 0b00011, VA, VA, VA),
                    6 if n > 0 => self.a.shl_v(false, esize, VA, VA, n),
                    2 if n >= esize => self.a.vec3(false, true, 0, 0b00011, VA, VA, VA),
                    2 if n > 0 => self.a.shr_v(false, false, esize, VA, VA, n),
                    4 if n > 0 => self.a.shr_v(false, true, esize, VA, VA, n.min(esize)),
                    _ => {}
                }
                self.a.str_d(VA, STATE, Self::mm_off(rm));
                written = Some(rm);
            }
            0x173 => {
                let n = insn.imm & 0xff;
                self.a.ldr(Width::X, T_A, STATE, Self::mm_off(rm));
                if n >= 64 {
                    self.a.mov(Size::X, T_A, ZR);
                } else if n > 0 {
                    if reg == 6 {
                        self.a.lsl_imm(Size::X, T_A, T_A, n);
                    } else {
                        self.a.lsr_imm(Size::X, T_A, T_A, n);
                    }
                }
                self.a.str(Width::X, T_A, STATE, Self::mm_off(rm));
                written = Some(rm);
            }
            _ => {
                let low_half = matches!(op, 0x160..=0x162);
                self.a.ldr_d(VA, STATE, Self::mm_off(reg));
                if !self.mm_src_v(insn, VB, low_half) {
                    return false;
                }
                match op {
                    0x160..=0x162 => self.a.zip(false, (op - 0x160) as u32, false, VA, VA, VB),
                    0x168..=0x16a => self.a.zip(false, (op - 0x168) as u32, true, VA, VA, VB),
                    0x163 | 0x167 | 0x16b => {
                        self.a.ins_d1(VA, VB);
                        let (kind, size) = match op {
                            0x163 => (Narrow::Sqxtn, 0),
                            0x167 => (Narrow::Sqxtun, 0),
                            _ => (Narrow::Sqxtn, 1),
                        };
                        self.a.narrow(kind, size, VA, VA);
                    }
                    0x1e4 | 0x1e5 => {
                        self.a.mull(op == 0x1e5, 1, VA, VA, VB);
                        self.a.shrn(16, VA, VA, 16);
                    }
                    0x1f5 => {
                        self.a.mull(true, 1, VA, VA, VB);
                        self.a.vec3(true, false, 2, 0b10111, VA, VA, VA);
                    }
                    _ => {
                        let (u, size, opcode) = three(op).expect("checked above");
                        self.a.vec3(false, u, size, opcode, VA, VA, VB);
                    }
                }
                self.a.str_d(VA, STATE, Self::mm_off(reg));
                written = Some(reg);
            }
        }
        let bits = 1u64 << 8 | written.map_or(0, |n| 1u64 << n);
        self.a.ldr(Width::W, T_B, STATE, state::MM_USED as i32);
        self.a.mov_imm(Size::W, T_C, bits);
        self.a.orr(Size::W, T_B, T_B, T_C);
        self.a.str(Width::W, T_B, STATE, state::MM_USED as i32);
        if written.is_some() {
            self.a.str(Width::W, ZR, STATE, state::FP64_VALID as i32);
        }
        true
    }

    /// Group-2 shift or rotate by an immediate count.
    fn shift_or_rotate(&mut self, kind: usize, dst: Reg, a: Reg, n: u32) -> bool {
        if kind > 1 {
            return self.shift_imm(kind, dst, a, n);
        }
        let n = n & 31;
        if n == 0 {
            if dst != a {
                self.a.mov(Size::W, dst, a);
            }
            return true;
        }
        self.a.ror_imm(Size::W, T_RES, a, if kind == 0 { 32 - n } else { n });
        self.rotate_flags(kind == 0);
        if dst != T_RES {
            self.a.mov(Size::W, dst, T_RES);
        }
        true
    }

    /// Sets CF and OF after ROL or ROR left the result in T_RES; other flags stay.
    fn rotate_flags(&mut self, left: bool) {
        if left {
            self.a.ubfx(Size::W, T_B, T_RES, 0, 1);
            self.a.lsr_imm(Size::W, T_C, T_RES, 31);
        } else {
            self.a.lsr_imm(Size::W, T_B, T_RES, 31);
            self.a.ubfx(Size::W, T_C, T_RES, 30, 1);
        }
        self.a.eor(Size::W, T_C, T_C, T_B);
        self.cf_of_flags();
    }

    /// Flags and write-back for SHLD/SHRD: result in T_RES, CF in T_B, old destination in T_E.
    fn double_shift_done(&mut self, insn: &Insn) {
        self.a.eor(Size::W, T_C, T_RES, T_E);
        self.a.lsr_imm(Size::W, T_C, T_C, 31);
        self.shift_flags();
        self.write_rm(insn, Ow::D, T_RES);
    }

    /// 8- or 16-bit INC/DEC of `a` into T_RES, keeping CF.
    fn inc_dec_narrow(&mut self, dec: bool, a: Reg, width: u32) {
        self.a.mov(Size::W, T_A, a);
        self.a.movz(Size::W, T_B, 1, 0);
        let nzcv = self.needs(flags::NZCV);
        if nzcv {
            self.a.cset(Size::W, T_E, Cond::Hs);
        }
        self.alu_narrow(if dec { 5 } else { 0 }, T_A, T_B, 32 - width);
        if nzcv {
            self.a.rmif(T_E, 63, 0b0010);
        }
    }

    fn inc_dec(&mut self, dec: bool, dst: Reg, src: Reg) {
        if self.need == 0 {
            if dec { self.a.sub_imm(Size::W, dst, src, 1) } else { self.a.add_imm(Size::W, dst, src, 1) }
            return;
        }
        let nzcv = self.needs(flags::NZCV);
        if nzcv {
            self.a.cset(Size::W, T_B, Cond::Hs);
        }
        match (dec, nzcv) {
            (true, true) => self.a.subs_imm(Size::W, T_RES, src, 1),
            (false, true) => self.a.adds_imm(Size::W, T_RES, src, 1),
            (true, false) => self.a.sub_imm(Size::W, T_RES, src, 1),
            (false, false) => self.a.add_imm(Size::W, T_RES, src, 1),
        }
        if nzcv {
            self.a.rmif(T_B, 63, 0b0010);
        }
        if self.needs(flags::PF) {
            self.a.mov(Size::W, PF_RESULT, T_RES);
        }
        if self.needs(flags::AF) {
            self.a.eor(Size::W, AF_AUX, src, T_RES);
        }
        if dst != T_RES {
            self.a.mov(Size::W, dst, T_RES);
        }
    }

    fn not_neg(&mut self, neg: bool, dst: Reg, src: Reg) {
        if neg {
            if self.needs(flags::NZCV) {
                self.a.subs(Size::W, T_RES, ZR, src);
            } else {
                self.a.sub(Size::W, T_RES, ZR, src);
            }
            if self.needs(flags::PF) {
                self.a.mov(Size::W, PF_RESULT, T_RES);
            }
            if self.needs(flags::AF) {
                self.a.eor(Size::W, AF_AUX, src, T_RES);
            }
            if dst != T_RES {
                self.a.mov(Size::W, dst, T_RES);
            }
        } else {
            self.a.mvn(Size::W, dst, src);
        }
    }
}

/// The NEON three-same op (U bit, size, opcode) for an MMX or SSE2 integer opcode.
fn simd_three(op: u16) -> Option<(bool, u32, u32)> {
    Some(match op {
        0x1fc => (false, 0, 0b10000),
        0x1fd => (false, 1, 0b10000),
        0x1fe => (false, 2, 0b10000),
        0x1f8 => (true, 0, 0b10000),
        0x1f9 => (true, 1, 0b10000),
        0x1fa => (true, 2, 0b10000),
        0x1ec => (false, 0, 0b00001),
        0x1ed => (false, 1, 0b00001),
        0x1dc => (true, 0, 0b00001),
        0x1dd => (true, 1, 0b00001),
        0x1e8 => (false, 0, 0b00101),
        0x1e9 => (false, 1, 0b00101),
        0x1d8 => (true, 0, 0b00101),
        0x1d9 => (true, 1, 0b00101),
        0x174 => (true, 0, 0b10001),
        0x175 => (true, 1, 0b10001),
        0x176 => (true, 2, 0b10001),
        0x164 => (false, 0, 0b00110),
        0x165 => (false, 1, 0b00110),
        0x166 => (false, 2, 0b00110),
        0x1d5 => (false, 1, 0b10011),
        0x1e0 => (true, 0, 0b00010),
        0x1e3 => (true, 1, 0b00010),
        0x1da => (true, 0, 0b01101),
        0x1de => (true, 0, 0b01100),
        0x1ea => (false, 1, 0b01101),
        0x1ee => (false, 1, 0b01100),
        _ => return None,
    })
}

/// Instructions whose translations are new enough to verify on their own.
fn is_recent(insn: &Insn) -> bool {
    let (op, reg, op32) = (insn.op, insn.reg(), insn.op32);
    match op {
    0x1c8..=0x1cf | 0xc0 | 0xd0 | 0xd2 | 0x9c | 0x9d | 0x1ba | 0xf5 | 0xf8 | 0xf9 | 0x118 | 0x11f | 0x12b => true,
    0x1ae | 0x1a3 | 0x1ab | 0x1b3 | 0x1bb | 0x7a | 0x7b | 0x18a | 0x18b | 0x14a | 0x14b | 0x19a | 0x19b | 0xed | 0x9e => true,
    0x70..=0x7f | 0x180..=0x18f | 0xe8 | 0xe9 | 0xeb | 0xff if insn.rep != Rep::None => true,
    0x1c0 | 0x1c1 | 0x1b0 | 0x1b1 | 0x1c7 | 0x1a4 | 0x1a5 | 0x1ac | 0x1ad | 0xfb => true,
    0xd8..=0xdf | 0x9b => true,
    _ if sse::is_sse(insn) => true,
    0x160..=0x17f | 0x1d0..=0x1ff => !insn.opsize_prefix && insn.rep == Rep::None,
    0xc1 | 0xd1 | 0xd3 => !op32 || reg < 2,
    0xf6 | 0xf7 => matches!(reg, 2 | 3) && !(op == 0xf7 && op32),
    0x40..=0x4f => !op32,
    0xff => reg < 2 && !op32,
    _ => false,
    }
}

/// Translates the block starting at `eip`. `byte(eip)` returns the code byte at
/// a linear address on the block's page or the next one, or `None` where it
/// cannot. Only an instruction that straddles the page end reads the next page.
/// Whether the translator handles this instruction rather than exiting to the interpreter.
pub fn translatable(insn: &Insn, pc: u32, user: bool) -> bool {
    let mut e = Emit {
        a: Asm::new(),
        block_va: 0,
        dispatch_va: 0x1000,
        exit_va: 0x2000,
        unstale_va: 0x3000,
        page: None,
        chains: Vec::new(),
        user,
        stored: false,
        need: flags::ALL,
        x87: x87::X87Block::default(),
        superblock: false,
        fold: None,
        fold_ok: false,
        at: 0,
        lp: X87Loop::default(),
        counters: None,
        exits: Vec::new(),
        cold: Vec::new(),
        starts: Vec::new(),
    };
    !matches!(e.insn(insn, pc), Step::Unsupported)
}

/// The flags each instruction of the block at `eip` must produce, `superblock`
/// meaning conditional branches leave through side exits. `exact` is verify mode.
fn flag_needs(eip: u32, byte: &mut impl FnMut(u32) -> Option<u8>, user: bool, exact: bool, superblock: bool) -> Vec<u8> {
    let page = eip & !0xfff;
    let mut effects = Vec::new();
    let mut pc = eip;
    let mut stop_after_next = false;
    let mut live_out = None;
    while effects.len() < MAX_INSNS {
        let Ok(insn) = decode::<()>(true, |i| byte(pc.wrapping_add(i)).ok_or(())) else { break };
        let end = pc.wrapping_add(insn.len as u32);
        if end.wrapping_sub(1) & !0xfff != page {
            break;
        }
        let mut e = flags::effect(&insn);
        if exact && e.flow != flags::Flow::End && !translatable(&insn, pc, user) {
            e = flags::Effect { reads: flags::ALL, writes: 0, may: 0, flow: flags::Flow::End };
        }
        if e.flow == flags::Flow::Branch && !insn.op32 {
            e = flags::Effect { reads: flags::ALL, writes: 0, may: 0, flow: flags::Flow::End };
        }
        if e.flow == flags::Flow::Branch {
            let rel = if insn.op < 0x100 { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
            let taken = if exact { flags::ALL } else { live_in(end.wrapping_add(rel), byte, page) };
            e.reads |= taken;
            e.flow = if superblock && !stop_after_next { flags::Flow::Next } else { flags::Flow::End };
            if e.flow == flags::Flow::End {
                live_out = Some(if exact { flags::ALL } else { live_in(end, byte, page) });
            }
        }
        effects.push(e);
        pc = end;
        if stop_after_next || e.flow == flags::Flow::End {
            if live_out.is_none() && !exact && insn.op32 && matches!(insn.op, 0xe9 | 0xeb) {
                let rel = if insn.op == 0xeb { insn.imm as u8 as i8 as i32 as u32 } else { insn.imm };
                live_out = Some(live_in(end.wrapping_add(rel), byte, page));
            }
            break;
        }
        stop_after_next = e.flow == flags::Flow::EndAfterNext;
    }
    let live_out = live_out.unwrap_or(if exact || effects.last().is_some_and(|e| e.flow == flags::Flow::End) {
        flags::ALL
    } else {
        live_in(pc, byte, page)
    });
    flags::needed(&effects, live_out)
}

/// Flags code at `at` may read before writing them. Stays on `page`, whose
/// code cannot change without dropping the block that asked.
pub(super) fn live_in(at: u32, byte: &mut impl FnMut(u32) -> Option<u8>, page: u32) -> u8 {
    let (mut live, mut written) = (0, 0);
    let mut pc = at;
    for _ in 0..16 {
        if pc & !0xfff != page {
            break;
        }
        let Ok(insn) = decode::<()>(true, |i| byte(pc.wrapping_add(i)).ok_or(())) else { break };
        let end = pc.wrapping_add(insn.len as u32);
        if end.wrapping_sub(1) & !0xfff != page {
            break;
        }
        let e = flags::effect(&insn);
        live |= e.reads & !written;
        if e.flow != flags::Flow::Next {
            break;
        }
        written |= e.writes;
        if live | written == flags::ALL {
            return live;
        }
        pc = end;
    }
    live | (flags::ALL & !written)
}

pub fn compile(
    eip: u32,
    mut byte: impl FnMut(u32) -> Option<u8>,
    block_va: u64,
    rt_va: u64,
    rt: &Entries,
    chain: bool,
    user: bool,
    self_check: bool,
    exact_flags: bool,
    counters: Option<u64>,
) -> Compiled {
    let superblock = chain;
    let need = if self_check { Vec::new() } else { flag_needs(eip, &mut byte, user, exact_flags, superblock) };
    let mut pass = |head| compile_pass(eip, &mut byte, block_va, rt_va, rt, chain, user, self_check, &need, head, counters);
    let (mut out, mut seen) = pass(None);
    // A loop of x87 code: translate it again assuming at its start the x87
    // state its back edge has, until the back edge matches.
    for _ in 0..3 {
        let Some(back) = seen.filter(|_| chain && !self_check) else { break };
        let (code, again) = pass(Some(back.loop_head()));
        if code.x87_loop {
            out = code;
            break;
        }
        seen = again;
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn compile_pass(
    eip: u32,
    mut byte: impl FnMut(u32) -> Option<u8>,
    block_va: u64,
    rt_va: u64,
    rt: &Entries,
    chain: bool,
    user: bool,
    self_check: bool,
    need: &[u8],
    head: Option<x87::X87Block>,
    counters: Option<u64>,
) -> (Compiled, Option<x87::X87Block>) {
    let mut e = Emit {
        a: Asm::new(),
        block_va,
        dispatch_va: rt_va + rt.dispatch as u64,
        exit_va: rt_va + rt.exit as u64,
        unstale_va: rt_va + rt.unstale as u64,
        page: chain.then_some(eip & !0xfff),
        chains: Vec::new(),
        user,
        stored: false,
        need: flags::ALL,
        x87: x87::X87Block::default(),
        superblock: false,
        fold: None,
        fold_ok: false,
        at: 0,
        lp: X87Loop::default(),
        counters: None,
        exits: Vec::new(),
        cold: Vec::new(),
        starts: Vec::new(),
    };
    let superblock = chain;
    e.fold_ok = !self_check;
    e.lp.start = eip;
    e.counters = counters;
    let mut boundaries = Vec::new();
    let mut pc = eip;
    let mut insns = 0;
    let mut has_rep = false;
    let mut sti: Option<(usize, u32, usize, usize)> = None;
    let page = eip & !0xfff;
    let on_page = |pc: u32| pc & !0xfff == page;
    e.a.nop();
    let check = e.a.label();
    let body = e.a.label();
    if self_check {
        e.a.b(check);
    }
    e.a.bind(body);
    e.bump(0);
    if let Some(h) = head {
        e.x87_loop_prologue(h, eip);
        let l = e.a.label();
        e.a.bind(l);
        e.lp.head = Some((l, h));
    }
    let mut code_end = eip;
    let mut recent = false;
    let mut interp_after = None;
    let mut retry = false;
    let mut x87_snaps = Vec::new();
    let mut sti_x87 = x87::X87Block::default();
    // ICOUNT is added where each stretch between side exits starts: (word, instructions before it).
    let mut count_sites = vec![(0usize, 0usize)];
    loop {
        let mut missing = false;
        let insn = decode::<()>(true, |i| {
            let b = byte(pc.wrapping_add(i));
            missing |= b.is_none();
            b.ok_or(())
        });
        let Ok(insn) = insn else {
            if let Some(at) = sti {
                e.x87 = sti_x87;
                e.back_to_sti(at, &mut pc, &mut insns, &mut boundaries);
            } else if insns == 0 {
                e.exit_interp(pc);
                interp_after = Some(0);
                retry = missing;
            } else {
                e.goto(pc);
            }
            break;
        };
        boundaries.push((e.a.pos() as u32, pc));
        if let Some(snap) = e.x87.snap() {
            x87_snaps.push((e.a.pos() as u32, snap));
        }
        let x87_before = e.x87;
        e.starts.push((e.a.pos(), e.cold.len()));
        has_rep |= insn.rep != Rep::None && matches!(insn.op, 0xa4..=0xa7 | 0xaa..=0xaf);
        let start = e.a.pos();
        let end = pc.wrapping_add(insn.len as u32);
        code_end = code_end.max(end);
        recent |= is_recent(&insn) || !on_page(end.wrapping_sub(1));
        if !on_page(end.wrapping_sub(1)) {
            if sti.is_some() {
                let at = sti.unwrap();
                e.x87 = sti_x87;
                e.back_to_sti(at, &mut pc, &mut insns, &mut boundaries);
                break;
            }
            let tail: Vec<u8> = (page.wrapping_add(0x1000)..end).map(|a| byte(a).unwrap_or(0)).collect();
            let slow = e.a.label();
            e.check_code_bytes(page.wrapping_add(0x1000), &tail, slow);
            e.need = flags::ALL;
            e.superblock = false;
            e.at = pc;
            match e.insn(&insn, pc) {
                Step::Next | Step::Branch => {
                    insns += 1;
                    e.goto(end);
                }
                Step::End => insns += 1,
                Step::Unsupported | Step::Sti => {
                    e.truncate_to(start);
                    e.x87 = x87_before;
                    e.exit_interp(pc);
                    interp_after = Some(insns);
                    break;
                }
            }
            e.a.bind(slow);
            e.x87 = x87_before;
            e.exit_interp(pc);
            break;
        }
        e.stored = false;
        e.need = need.get(insns).copied().unwrap_or(flags::ALL);
        e.superblock = superblock && sti.is_none();
        e.at = pc;
        let step = e.insn(&insn, pc);
        if self_check && e.stored && matches!(step, Step::Next | Step::Sti) {
            // T_ADDR still holds the address of the last store: leave through
            // the dispatcher when it hit this page, so the entry check sees it.
            let next = pc.wrapping_add(insn.len as u32);
            let same = e.a.label();
            let cont = e.a.label();
            e.a.lsr_imm(Size::W, T_B, T_ADDR, 12);
            e.a.mov_imm(Size::W, T_C, (page >> 12) as u64);
            e.a.eor(Size::W, T_B, T_B, T_C);
            e.a.cbz(Size::W, T_B, same);
            e.a.b(cont);
            e.a.bind(same);
            e.goto_dispatch(next);
            e.a.bind(cont);
        }
        match step {
            Step::Sti if sti.is_none() => {
                sti = Some((start, pc, insns, boundaries.len() - 1));
                sti_x87 = x87_before;
                insns += 1;
                pc = pc.wrapping_add(insn.len as u32);
                if insns >= MAX_INSNS || !on_page(pc) {
                    e.x87 = sti_x87;
                    e.back_to_sti(sti.unwrap(), &mut pc, &mut insns, &mut boundaries);
                    break;
                }
            }
            Step::Next if sti.is_some() => {
                insns += 1;
                pc = pc.wrapping_add(insn.len as u32);
                e.goto_dispatch(pc);
                break;
            }
            Step::Next => {
                insns += 1;
                pc = pc.wrapping_add(insn.len as u32);
                if insns >= MAX_INSNS || !on_page(pc) {
                    e.goto(pc);
                    break;
                }
            }
            Step::Branch => {
                insns += 1;
                pc = end;
                count_sites.push((e.a.pos(), insns));
                e.a.nop();
                if insns >= MAX_INSNS || !on_page(pc) {
                    e.goto(pc);
                    break;
                }
            }
            Step::End => {
                insns += 1;
                break;
            }
            Step::Unsupported | Step::Sti => {
                if let Some(at) = sti {
                    e.x87 = sti_x87;
                e.back_to_sti(at, &mut pc, &mut insns, &mut boundaries);
                } else {
                    e.truncate_to(start);
                    e.x87 = x87_before;
                    e.exit_interp(pc);
                    interp_after = Some(insns);
                }
                break;
            }
        }
    }
    e.emit_cold();
    if self_check {
        e.a.bind(check);
        let page_end = page.wrapping_add(0x1000);
        let last = if code_end > page_end || code_end < eip { page_end } else { code_end };
        let bytes: Vec<u8> = (eip..last).map(|a| byte(a).unwrap_or(0)).collect();
        let stale = e.a.label();
        e.check_bytes_in_page(eip, &bytes, stale);
        e.a.b(body);
        e.a.bind(stale);
        e.x87 = x87::X87Block::default();
        e.exit_with(eip, reason::STALE);
    }
    let mut code = e.a.finish();
    let segment = |k: usize| count_sites.get(k + 1).map_or(insns, |&(_, n)| n) - count_sites[k].1;
    let sites = count_sites.iter().enumerate().map(|(k, &(word, _))| (word, segment(k)));
    for (word, n) in sites.chain(e.lp.counts.iter().map(|&w| (w, segment(0)))) {
        let mut count = Asm::new();
        count.add_imm(Size::X, ICOUNT, ICOUNT, n as u32);
        code[word] = count.code[0];
    }
    let x87_loop = !e.lp.counts.is_empty();
    let compiled = Compiled { code, boundaries, insns, chains: e.chains, has_rep, recent, interp_after, retry, x87: x87_snaps, x87_loop, exits: e.exits };
    (compiled, e.lp.seen)
}

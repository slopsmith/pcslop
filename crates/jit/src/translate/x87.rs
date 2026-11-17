//! x87 instructions on the A64 FPU, matching the interpreter: registers stay in
//! the 80-bit format in the runtime state, and arithmetic rounds the operands to
//! f64 as the interpreter does. Anything unusual (NaNs, infinities, denormals,
//! stack faults, exceptions other than inexact) leaves for the interpreter
//! before any state changes. Nothing here may set NZCV, which holds the guest
//! flags, except FCOMI writing them on purpose.

use a64::{Fcm, Label, Logic, Reg, Shift, Size, V, Width, ZR, sysreg};
use x86::decode::Insn;

use super::{Emit, T_A, T_ADDR, T_B, T_C, T_E};
use crate::runtime::{AF_AUX, PF_RESULT, STATE, guest_reg};
use crate::state;

const TOP: Reg = Reg(2);
const TAGS: Reg = Reg(3);
const SW: Reg = Reg(4);
const P: Reg = Reg(5);
const M: Reg = Reg(6);
const S: Reg = Reg(7);
const U: Reg = Reg(8);
/// FPSR as it was when the block's x87 work started; restored by `x87_flush`,
/// so x87 arithmetic leaves no flags for MXCSR.
const SAVED_FPSR: Reg = Reg(1);
const FA: V = V(18);
const FB: V = V(19);
const FR: V = V(20);
const FS: V = V(21);
const E1: V = V(26);
const E2: V = V(27);

const C0: u32 = 1 << 8;
const C1: u32 = 1 << 9;
const C2: u32 = 1 << 10;
const C3: u32 = 1 << 14;
const PE_SHIFT: u32 = 5;
const SQRT: u32 = 9;

/// What the translator knows about the x87 state at the current point of a
/// block. Registers TOP (as it was at the block's first x87 instruction),
/// TAGS and SW stay loaded from then on; stack slots are numbered relative to
/// that TOP. Results stay in V8 plus their slot until the block leaves, when
/// `x87_flush` writes them back (see `Snap` for faults in between).
#[derive(Default, Clone, Copy)]
pub(super) struct X87Block {
    entered: bool,
    rc_checked: bool,
    /// How far TOP moved since entry, mod 8 (a push adds 7).
    d: u8,
    /// Slots whose f64 value is in V8 plus the slot number.
    cached: u8,
    /// Cached slots whose 80-bit value in the runtime state is stale.
    dirty: u8,
    full: u8,
    empty: u8,
    /// SW, TAGS, FIP and FOP are newer in registers than in the state; FIP and
    /// FOP of the last instruction are `note`.
    meta: bool,
    note: (u32, u32),
}

/// What the host needs to write the x87 state back when it abandons a block
/// at an instruction boundary: TOP, TAGS and SW are in x2, x3 and x4.
#[derive(Clone, Copy, Debug)]
pub struct Snap {
    pub dirty: u8,
    pub meta: bool,
    pub note: (u32, u32),
}

impl X87Block {
    pub(super) fn snap(&self) -> Option<Snap> {
        (self.entered && (self.dirty != 0 || self.meta)).then_some(Snap { dirty: self.dirty, meta: self.meta, note: self.note })
    }
}

fn cv(slot: usize) -> V {
    V(8 + slot as u8)
}

#[derive(Clone, Copy)]
enum Src {
    F32,
    F64,
    I16,
    I32,
}

#[derive(Clone, Copy)]
enum K {
    LdMem(Src),
    Ild(Width),
    StMem { double: bool, pop: bool },
    Ist { w: Width, pop: bool },
    LdReg(usize),
    StReg { i: usize, pop: bool },
    Xch(usize),
    Chs,
    Abs,
    LdConst { mant: u64 },
    Arith { reg: usize, i: usize, dst: usize, pop: bool },
    ArithMem { reg: usize, src: Src },
    Com { i: usize, pops: u32 },
    ComMem { src: Src, pop: bool },
    Tst,
    Comi { i: usize, pop: bool },
    Sqrt,
    Rndint,
    StswAx,
    StswMem,
    StcwMem,
    Ldcw,
}

fn classify(insn: &Insn) -> Option<K> {
    let op = (insn.op & 7) as u8;
    let reg = insn.reg();
    let modrm = insn.modrm;
    let i = (modrm & 7) as usize;
    if insn.mem.is_some() {
        return Some(match (op, reg) {
            (0, 2 | 3) => K::ComMem { src: Src::F32, pop: reg == 3 },
            (4, 2 | 3) => K::ComMem { src: Src::F64, pop: reg == 3 },
            (0, _) => K::ArithMem { reg, src: Src::F32 },
            (4, _) => K::ArithMem { reg, src: Src::F64 },
            (2, r) if r != 2 && r != 3 => K::ArithMem { reg, src: Src::I32 },
            (6, r) if r != 2 && r != 3 => K::ArithMem { reg, src: Src::I16 },
            (1, 0) => K::LdMem(Src::F32),
            (5, 0) => K::LdMem(Src::F64),
            (3, 0) => K::Ild(Width::W),
            (7, 0) => K::Ild(Width::H),
            (7, 5) => K::Ild(Width::X),
            (1, 2 | 3) => K::StMem { double: false, pop: reg == 3 },
            (5, 2 | 3) => K::StMem { double: true, pop: reg == 3 },
            (3, 2 | 3) => K::Ist { w: Width::W, pop: reg == 3 },
            (7, 2 | 3) => K::Ist { w: Width::H, pop: reg == 3 },
            (7, 7) => K::Ist { w: Width::X, pop: true },
            (1, 5) => K::Ldcw,
            (1, 7) => K::StcwMem,
            (5, 7) => K::StswMem,
            _ => return None,
        });
    }
    Some(match (op, modrm) {
        (0, 0xd0..=0xdf) => K::Com { i, pops: (reg == 3) as u32 },
        (0, _) => K::Arith { reg, i, dst: 0, pop: false },
        (4, 0xc0..=0xcf | 0xe0..=0xff) => K::Arith { reg, i, dst: i, pop: false },
        (6, 0xc0..=0xcf | 0xe0..=0xff) => K::Arith { reg, i, dst: i, pop: true },
        (6, 0xd9) => K::Com { i: 1, pops: 2 },
        (5, 0xe0..=0xe7) => K::Com { i, pops: 0 },
        (5, 0xe8..=0xef) => K::Com { i, pops: 1 },
        (2, 0xe9) => K::Com { i: 1, pops: 2 },
        (1, 0xc0..=0xc7) => K::LdReg(i),
        (1, 0xc8..=0xcf) => K::Xch(i),
        (5, 0xd0..=0xd7) => K::StReg { i, pop: false },
        (5, 0xd8..=0xdf) => K::StReg { i, pop: true },
        (1, 0xe0) => K::Chs,
        (1, 0xe1) => K::Abs,
        (1, 0xe4) => K::Tst,
        (1, 0xe8) => K::LdConst { mant: 1 << 63 },
        (1, 0xee) => K::LdConst { mant: 0 },
        (1, 0xfa) => K::Sqrt,
        (1, 0xfc) => K::Rndint,
        (3, 0xe8..=0xf7) => K::Comi { i, pop: false },
        (7, 0xe8..=0xf7) => K::Comi { i, pop: true },
        (7, 0xe0) => K::StswAx,
        _ => return None,
    })
}

impl Emit {
    /// Translates an x87 instruction (D8..DF); false when it is left to the interpreter.
    pub(super) fn x87(&mut self, insn: &Insn, eip: u32) -> bool {
        let Some(k) = classify(insn) else { return false };
        if insn.mem.is_some() && (!insn.addr32 || matches!(insn.seg_override, Some(4 | 5))) {
            return false;
        }
        let control = matches!(k, K::StswAx | K::StswMem | K::StcwMem | K::Ldcw);
        let bail = self.a.label();
        let done = self.a.label();
        let mut seg = None;
        if let Some(m) = insn.mem {
            if !self.ea(insn, &m) {
                return false;
            }
            seg = Some(insn.seg_override.unwrap_or(m.default_seg));
        }
        self.x87_enter(bail);
        let before = self.x87;
        let mut comi = None;
        match k {
            K::LdMem(src) => {
                self.x87_need(7, false, bail);
                self.load_src(src, FB, bail);
                self.clear_c1();
                self.fpush_val(FB);
            }
            K::Ild(Width::X) => {
                self.x87_need(7, false, bail);
                self.a.ldr(Width::X, T_A, T_ADDR, 0);
                self.int_to_f80();
                self.clear_c1();
                self.fpush();
            }
            K::Ild(w) => {
                self.x87_need(7, false, bail);
                match w {
                    Width::W => self.a.ldr(Width::W, T_A, T_ADDR, 0),
                    _ => self.a.ldrs(w, Size::W, T_A, T_ADDR, 0),
                }
                self.a.scvtf(true, Size::W, FB, T_A);
                self.clear_c1();
                self.fpush_val(FB);
            }
            K::StMem { double, pop } => {
                self.x87_need(0, true, bail);
                self.check_rc_nearest(bail);
                if double && !self.is_cached(0) {
                    self.fget(0, M, S);
                    self.to_f64(bail, false);
                    self.and_imm(Size::X, T_B, M, 0x7ff);
                    self.flag_nonzero_to_pe(T_B);
                    self.store(Width::X, T_C);
                } else if double {
                    self.a.fmov_xd(T_C, cv(self.slot(0)));
                    self.store(Width::X, T_C);
                } else {
                    let v = self.st_val(0, bail);
                    self.a.fp1(true, 4, FB, v);
                    self.a.fmov_ws(T_A, FB);
                    let (normal, zero) = (self.a.label(), self.a.label());
                    self.a.ubfx(Size::W, T_B, T_A, 23, 8);
                    self.a.add_imm(Size::W, T_C, T_B, 1);
                    self.a.tbnz(T_C, 8, bail);
                    self.a.cbnz(Size::W, T_B, normal);
                    self.a.lsl_imm(Size::W, T_B, T_A, 1);
                    self.a.cbnz(Size::W, T_B, bail);
                    self.a.fmov_xd(T_B, v);
                    self.a.lsl_imm(Size::X, T_B, T_B, 1);
                    self.a.cbz(Size::X, T_B, zero);
                    self.a.b(bail);
                    self.a.bind(normal);
                    let exact = self.a.label();
                    self.a.tbnz(SW, PE_SHIFT, exact);
                    self.a.fp1(false, 5, E1, FB);
                    self.a.fcm(Fcm::Eq, true, E1, E1, v);
                    self.a.fmov_xd(T_B, E1);
                    self.a.mvn(Size::X, T_B, T_B);
                    self.flag_nonzero_to_pe(T_B);
                    self.a.bind(exact);
                    self.a.bind(zero);
                    self.store(Width::W, T_A);
                }
                if pop {
                    self.fpop();
                }
            }
            K::Ist { w, pop } => {
                self.x87_need(0, true, bail);
                let v = self.st_val(0, bail);
                self.round_by_rc(FB, v);
                self.a.fcm(Fcm::Eq, true, FR, FB, v);
                self.a.fmov_xd(T_B, FR);
                self.a.mvn(Size::X, T_B, T_B);
                self.flag_nonzero_to_pe(T_B);
                self.a.fcm(Fcm::Agt, true, FR, FB, v);
                self.a.fmov_xd(T_B, FR);
                self.a.bfi(Size::W, SW, T_B, 9, 1);
                self.a.fcvtzs(true, Size::X, T_A, FB);
                match w {
                    Width::X => {
                        self.a.mov_imm(Size::X, T_E, i64::MAX as u64);
                        self.a.eor(Size::X, T_E, T_E, T_A);
                        self.a.cbz(Size::X, T_E, bail);
                        self.a.mov_imm(Size::X, T_E, i64::MIN as u64);
                        self.a.eor(Size::X, T_E, T_E, T_A);
                        self.a.cbz(Size::X, T_E, bail);
                    }
                    Width::W => {
                        self.a.sxtw(T_E, T_A);
                        self.a.sub(Size::X, T_E, T_E, T_A);
                        self.a.cbnz(Size::X, T_E, bail);
                    }
                    _ => {
                        self.a.sxth(Size::X, T_E, T_A);
                        self.a.sub(Size::X, T_E, T_E, T_A);
                        self.a.cbnz(Size::X, T_E, bail);
                    }
                }
                self.store(w, T_A);
                if pop {
                    self.fpop();
                }
            }
            K::LdReg(i) => {
                self.x87_need(i, true, bail);
                self.x87_need(7, false, bail);
                if self.is_cached(i) {
                    let v = cv(self.slot(i));
                    self.clear_c1();
                    self.fpush_val(v);
                } else {
                    self.fget(i, M, S);
                    self.clear_c1();
                    self.fpush();
                }
            }
            K::StReg { i, pop } => {
                self.x87_need(0, true, bail);
                self.clear_c1();
                if self.is_cached(0) {
                    let v = cv(self.slot(0));
                    self.fset(i, v);
                } else {
                    self.fget(0, M, S);
                    self.fput(i, M, S);
                    self.uncache(i);
                }
                if pop {
                    self.fpop();
                }
            }
            K::Xch(i) => {
                self.x87_need(0, true, bail);
                self.x87_need(i, true, bail);
                self.clear_c1();
                let (a, b) = (self.slot(0), self.slot(i));
                if self.x87.cached & (1 << a | 1 << b) == 1 << a | 1 << b {
                    self.a.fp1(true, 0, FS, cv(a));
                    self.a.fp1(true, 0, cv(a), cv(b));
                    self.a.fp1(true, 0, cv(b), FS);
                    if a != b {
                        self.x87.dirty |= 1 << a | 1 << b;
                    }
                } else {
                    self.write_back(a);
                    self.write_back(b);
                    self.fget(0, M, S);
                    self.fget(i, T_A, T_B);
                    self.fput(0, T_A, T_B);
                    self.fput(i, M, S);
                    let (ca, cb) = (self.x87.cached & 1 << a != 0, self.x87.cached & 1 << b != 0);
                    if ca {
                        self.a.fp1(true, 0, cv(b), cv(a));
                    } else if cb {
                        self.a.fp1(true, 0, cv(a), cv(b));
                    }
                    if ca != cb {
                        self.x87.cached ^= 1 << a | 1 << b;
                    }
                }
            }
            K::Chs | K::Abs => {
                self.x87_need(0, true, bail);
                self.clear_c1();
                if self.is_cached(0) {
                    let v = cv(self.slot(0));
                    self.a.fp1(true, if matches!(k, K::Chs) { 2 } else { 1 }, v, v);
                    self.fset(0, v);
                } else {
                    self.fget(0, M, S);
                    if matches!(k, K::Chs) {
                        let ok = self.a.logic_imm(Logic::Eor, Size::W, S, S, 0x8000);
                        assert!(ok);
                    } else {
                        self.and_imm(Size::W, S, S, 0x7fff);
                    }
                    self.fput(0, M, S);
                }
            }
            K::LdConst { mant } => {
                self.x87_need(7, false, bail);
                let bits = if mant == 0 { 0 } else { 1.0f64.to_bits() };
                self.a.mov_imm(Size::X, T_C, bits);
                self.a.fmov_dx(FB, T_C);
                self.clear_c1();
                self.fpush_val(FB);
            }
            K::Arith { reg, i, dst, pop } => {
                self.x87_need(0, true, bail);
                self.x87_need(i, true, bail);
                let a = self.st_val(0, bail);
                let b = self.st_val(i, bail);
                self.arith(reg, a, b, bail);
                self.fset(dst, FR);
                if pop {
                    self.fpop();
                }
            }
            K::ArithMem { reg, src } => {
                self.x87_need(0, true, bail);
                self.load_src(src, FB, bail);
                let a = self.st_val(0, bail);
                self.arith(reg, a, FB, bail);
                self.fset(0, FR);
            }
            K::Com { i, pops } => {
                self.x87_need(0, true, bail);
                self.x87_need(i, true, bail);
                let a = self.st_val(0, bail);
                let b = self.st_val(i, bail);
                self.set_cc(a, b);
                for _ in 0..pops {
                    self.fpop();
                }
            }
            K::ComMem { src, pop } => {
                self.x87_need(0, true, bail);
                self.load_src(src, FB, bail);
                let a = self.st_val(0, bail);
                self.set_cc(a, FB);
                if pop {
                    self.fpop();
                }
            }
            K::Tst => {
                self.x87_need(0, true, bail);
                let a = self.st_val(0, bail);
                self.a.fmov_dx(FB, ZR);
                self.set_cc(a, FB);
            }
            K::Comi { i, pop } => {
                self.x87_need(0, true, bail);
                self.x87_need(i, true, bail);
                let a = self.st_val(0, bail);
                let b = self.st_val(i, bail);
                self.a.fp1(true, 0, FA, a);
                self.a.fp1(true, 0, FB, b);
                comi = Some(());
                if pop {
                    self.fpop();
                }
            }
            K::Sqrt => {
                self.x87_need(0, true, bail);
                self.check_rc_nearest(bail);
                let a = self.st_val(0, bail);
                self.a.fp1(true, 3, FR, a);
                self.check_result(FR, None, bail);
                self.track_inexact(SQRT, a, a);
                self.clear_c1();
                self.fset(0, FR);
            }
            K::Rndint => {
                self.x87_need(0, true, bail);
                let a = self.st_val(0, bail);
                self.round_by_rc(FR, a);
                self.a.fcm(Fcm::Eq, true, FS, FR, a);
                self.a.fmov_xd(T_B, FS);
                self.a.mvn(Size::X, T_B, T_B);
                self.flag_nonzero_to_pe(T_B);
                self.clear_c1();
                self.fset(0, FR);
            }
            K::StswAx => self.a.bfi(Size::W, guest_reg(0), SW, 0, 16),
            K::StswMem => self.store(Width::H, SW),
            K::StcwMem => {
                self.a.ldr(Width::W, T_A, STATE, state::FCW as i32);
                self.store(Width::H, T_A);
            }
            K::Ldcw => {
                self.fldcw(bail);
                self.x87_flush();
                self.x87 = X87Block::default();
            }
        }
        if !control {
            self.x87_commit(eip, insn, seg);
        }
        if comi.is_some() {
            self.a.fcmp(true, false, FA, FB);
            self.a.axflag();
            self.a.movz(Size::W, PF_RESULT, 1, 0);
            self.a.movz(Size::W, AF_AUX, 0, 0);
        }
        self.a.b(done);
        self.a.bind(bail);
        let after = std::mem::replace(&mut self.x87, before);
        self.exit_interp(eip);
        self.x87 = after;
        self.a.bind(done);
        true
    }

    /// FWAIT: nothing to do unless an x87 exception could be pending.
    pub(super) fn fwait(&mut self, eip: u32) {
        let ok = self.a.label();
        self.a.ldr(Width::W, U, STATE, state::X87_BASE as i32);
        self.a.cbnz(Size::W, U, ok);
        self.exit_interp(eip);
        self.a.bind(ok);
    }

    fn fldcw(&mut self, bail: Label) {
        self.a.ldr(Width::H, T_A, T_ADDR, 0);
        let ok = self.a.logic_imm(Logic::Orr, Size::W, T_A, T_A, 0x40);
        assert!(ok);
        self.a.bic(Size::W, T_B, SW, T_A);
        self.and_imm(Size::W, T_B, T_B, 0x3f);
        self.a.cbnz(Size::W, T_B, bail);
        self.and_imm(Size::W, T_B, T_A, 0x3f);
        let ok = self.a.logic_imm(Logic::Eor, Size::W, T_B, T_B, 0x3f);
        assert!(ok);
        self.a.ubfx(Size::W, T_C, T_A, 9, 1);
        let ok = self.a.logic_imm(Logic::Eor, Size::W, T_C, T_C, 1);
        assert!(ok);
        self.a.orr(Size::W, T_B, T_B, T_C);
        self.a.sub_imm(Size::W, T_B, T_B, 1);
        self.a.lsr_imm(Size::W, T_B, T_B, 31);
        self.a.ldr(Width::W, T_C, STATE, state::X87_BASE as i32);
        self.a.and(Size::W, T_B, T_B, T_C);
        self.a.str(Width::W, T_B, STATE, state::X87_OK as i32);
        self.a.str(Width::W, T_A, STATE, state::FCW as i32);
    }

    fn and_imm(&mut self, sz: Size, rd: Reg, rn: Reg, imm: u64) {
        let ok = self.a.logic_imm(Logic::And, sz, rd, rn, imm);
        assert!(ok);
    }

    fn x87_enter(&mut self, bail: Label) {
        if self.x87.entered {
            return;
        }
        self.x87 = X87Block { entered: true, ..X87Block::default() };
        self.a.ldr(Width::W, U, STATE, state::X87_OK as i32);
        self.a.cbz(Size::W, U, bail);
        self.a.ldr(Width::W, U, STATE, state::MM_USED as i32);
        self.a.tbnz(U, 8, bail);
        self.a.ldr(Width::W, SW, STATE, state::FSW as i32);
        self.a.ubfx(Size::W, TOP, SW, 11, 3);
        self.a.ldr(Width::W, TAGS, STATE, state::FTAGS as i32);
        self.a.mrs(SAVED_FPSR, sysreg::FPSR);
    }

    /// The slot of ST(i), relative to TOP at the block's first x87 instruction.
    fn slot(&self, i: usize) -> usize {
        (self.x87.d as usize + i) & 7
    }

    fn is_cached(&self, i: usize) -> bool {
        self.x87.cached & 1 << self.slot(i) != 0
    }

    fn phys(&mut self, i: usize) {
        let r = self.slot(i);
        if r == 0 {
            self.a.mov(Size::W, P, TOP);
        } else {
            self.a.add_imm(Size::W, P, TOP, r as u32);
            self.and_imm(Size::W, P, P, 7);
        }
    }

    /// Leaves unless ST(i) is full (`full`) or empty.
    fn x87_need(&mut self, i: usize, full: bool, bail: Label) {
        let bit = 1u8 << self.slot(i);
        let known = if full { self.x87.full } else { self.x87.empty };
        if known & bit != 0 {
            return;
        }
        self.phys(i);
        self.a.lsrv(Size::W, U, TAGS, P);
        if full {
            self.a.tbz(U, 0, bail);
            self.x87.full |= bit;
        } else {
            self.a.tbnz(U, 0, bail);
            self.x87.empty |= bit;
        }
    }

    /// The V register holding ST(i) as an exact f64, loading it if needed;
    /// leaves unless the value is zero or a normal f64.
    fn st_val(&mut self, i: usize, bail: Label) -> V {
        let r = self.slot(i);
        if self.x87.cached & 1 << r == 0 {
            self.fget(i, M, S);
            self.to_f64(bail, true);
            self.a.fmov_dx(cv(r), T_C);
            self.x87.cached |= 1 << r;
        }
        cv(r)
    }

    fn cache_set(&mut self, i: usize, v: V) {
        let r = self.slot(i);
        if v != cv(r) {
            self.a.fp1(true, 0, cv(r), v);
        }
        self.x87.cached |= 1 << r;
    }

    fn uncache(&mut self, i: usize) {
        let bit = 1u8 << self.slot(i);
        self.x87.cached &= !bit;
        self.x87.dirty &= !bit;
    }

    /// Makes `v` the value of ST(i), leaving the state's copy stale.
    fn fset(&mut self, i: usize, v: V) {
        let bit = 1u8 << self.slot(i);
        if self.x87.full & bit == 0 {
            self.phys(i);
            self.a.movz(Size::W, U, 1, 0);
            self.a.lslv(Size::W, U, U, P);
            self.a.orr(Size::W, TAGS, TAGS, U);
        }
        self.cache_set(i, v);
        self.x87.dirty |= bit;
        self.x87.full |= bit;
        self.x87.empty &= !bit;
    }

    fn fpush_val(&mut self, v: V) {
        self.phys(7);
        self.a.bfi(Size::W, SW, P, 11, 3);
        self.x87.d = (self.x87.d + 7) & 7;
        self.fset(0, v);
    }

    /// Writes slot `r` to the state if it is dirty.
    fn write_back(&mut self, r: usize) {
        if self.x87.dirty & 1 << r == 0 {
            return;
        }
        self.f80_from_cache(r);
        self.x87.dirty &= !(1 << r);
    }

    fn f80_from_cache(&mut self, r: usize) {
        let (zero, done) = (self.a.label(), self.a.label());
        self.a.fmov_xd(T_A, cv(r));
        self.a.ubfx(Size::X, T_B, T_A, 52, 11);
        self.a.cbz(Size::W, T_B, zero);
        self.a.lsl_imm(Size::X, M, T_A, 11);
        let ok = self.a.logic_imm(Logic::Orr, Size::X, M, M, 1 << 63);
        assert!(ok);
        self.a.mov_imm(Size::W, T_C, 0x3c00);
        self.a.add(Size::W, S, T_B, T_C);
        self.a.lsr_imm(Size::X, T_E, T_A, 63);
        self.a.logic_shift(Logic::Orr, false, Size::W, S, S, T_E, Shift::Lsl, 15);
        self.a.b(done);
        self.a.bind(zero);
        self.a.movz(Size::X, M, 0, 0);
        self.a.lsr_imm(Size::X, S, T_A, 63);
        self.a.lsl_imm(Size::W, S, S, 15);
        self.a.bind(done);
        if r == 0 {
            self.a.mov(Size::W, P, TOP);
        } else {
            self.a.add_imm(Size::W, P, TOP, r as u32);
            self.and_imm(Size::W, P, P, 7);
        }
        self.a.add_shift(Size::X, U, STATE, P, Shift::Lsl, 3);
        self.a.str(Width::X, M, U, state::MM as i32);
        self.a.add_shift(Size::X, U, STATE, P, Shift::Lsl, 1);
        self.a.str(Width::H, S, U, state::FSE as i32);
    }

    /// Writes everything newer in registers back to the state, on the path
    /// being emitted; the translator's view stays as it was.
    pub(super) fn x87_flush(&mut self) {
        if !self.x87.entered {
            return;
        }
        let saved = self.x87;
        self.a.msr(sysreg::FPSR, SAVED_FPSR);
        for r in 0..8 {
            if saved.dirty & 1 << r != 0 {
                self.f80_from_cache(r);
            }
        }
        if saved.meta {
            self.a.str(Width::W, SW, STATE, state::FSW as i32);
            self.a.str(Width::W, TAGS, STATE, state::FTAGS as i32);
            self.a.mov_imm(Size::W, U, saved.note.0 as u64);
            self.a.str(Width::W, U, STATE, state::FIP as i32);
            self.a.mov_imm(Size::W, U, saved.note.1 as u64);
            self.a.str(Width::W, U, STATE, state::FNOTE as i32);
        }
        self.x87 = saved;
    }

    /// Leaves unless `v` needs no flag but PE: not infinite, NaN or denormal, and a
    /// zero only when one of `zero_needs` (factors, or a dividend) is zero.
    fn check_result(&mut self, v: V, zero_needs: Option<(V, Option<V>)>, bail: Label) {
        let ok = self.a.label();
        self.a.fmov_xd(T_A, v);
        self.a.ubfx(Size::X, T_B, T_A, 52, 11);
        self.a.add_imm(Size::W, T_C, T_B, 1);
        self.a.tbnz(T_C, 11, bail);
        self.a.cbnz(Size::W, T_B, ok);
        self.a.lsl_imm(Size::X, T_C, T_A, 1);
        self.a.cbnz(Size::X, T_C, bail);
        if let Some((x, y)) = zero_needs {
            for f in [Some(x), y].into_iter().flatten() {
                self.a.fmov_xd(T_C, f);
                self.a.lsl_imm(Size::X, T_C, T_C, 1);
                self.a.cbz(Size::X, T_C, ok);
            }
            self.a.b(bail);
        }
        self.a.bind(ok);
    }

    /// Sets PE unless FR is exactly `x` op `y` (op as in `fp2`, or SQRT of `x`);
    /// skipped once PE is set, as it is sticky.
    fn track_inexact(&mut self, op: u32, x: V, y: V) {
        let done = self.a.label();
        self.a.tbnz(SW, PE_SHIFT, done);
        match op {
            2 | 3 => {
                let sub = op == 3;
                self.a.fp2(true, 3, E1, FR, x);
                self.a.fp2(true, 3, E2, FR, E1);
                self.a.fp2(true, 3, E2, x, E2);
                self.a.fp2(true, if sub { 2 } else { 3 }, E1, y, E1);
                if sub {
                    self.a.fp2(true, 3, E1, E2, E1);
                } else {
                    self.a.fp2(true, 2, E1, E2, E1);
                }
            }
            SQRT => self.a.fmadd_d(true, E1, FR, FR, x),
            0 => self.a.fmadd_d(true, E1, x, y, FR),
            _ => self.a.fmadd_d(true, E1, FR, y, x),
        }
        self.a.fmov_xd(T_B, E1);
        self.a.lsl_imm(Size::X, T_B, T_B, 1);
        self.flag_nonzero_to_pe(T_B);
        self.a.bind(done);
    }

    fn fget(&mut self, i: usize, m: Reg, s: Reg) {
        self.phys(i);
        self.a.add_shift(Size::X, U, STATE, P, Shift::Lsl, 3);
        self.a.ldr(Width::X, m, U, state::MM as i32);
        self.a.add_shift(Size::X, U, STATE, P, Shift::Lsl, 1);
        self.a.ldr(Width::H, s, U, state::FSE as i32);
    }

    fn fput(&mut self, i: usize, m: Reg, s: Reg) {
        self.phys(i);
        self.write_phys(m, s);
        let bit = 1u8 << self.slot(i);
        self.x87.full |= bit;
        self.x87.empty &= !bit;
        self.x87.dirty &= !bit;
    }

    fn write_phys(&mut self, m: Reg, s: Reg) {
        self.a.add_shift(Size::X, U, STATE, P, Shift::Lsl, 3);
        self.a.str(Width::X, m, U, state::MM as i32);
        self.a.add_shift(Size::X, U, STATE, P, Shift::Lsl, 1);
        self.a.str(Width::H, s, U, state::FSE as i32);
        self.a.movz(Size::W, U, 1, 0);
        self.a.lslv(Size::W, U, U, P);
        self.a.orr(Size::W, TAGS, TAGS, U);
    }

    fn fpush(&mut self) {
        self.phys(7);
        self.write_phys(M, S);
        self.a.bfi(Size::W, SW, P, 11, 3);
        self.x87.d = (self.x87.d + 7) & 7;
        let bit = 1u8 << self.slot(0);
        self.x87.full |= bit;
        self.x87.empty &= !bit;
        self.x87.cached &= !bit;
        self.x87.dirty &= !bit;
    }

    fn fpop(&mut self) {
        self.a.ubfx(Size::W, P, SW, 11, 3);
        self.a.movz(Size::W, U, 1, 0);
        self.a.lslv(Size::W, U, U, P);
        self.a.bic(Size::W, TAGS, TAGS, U);
        self.a.add_imm(Size::W, P, P, 1);
        self.a.bfi(Size::W, SW, P, 11, 3);
        let bit = 1u8 << self.slot(0);
        self.x87.full &= !bit;
        self.x87.empty |= bit;
        self.x87.d = (self.x87.d + 1) & 7;
    }

    fn clear_c1(&mut self) {
        self.and_imm(Size::W, SW, SW, !C1 as u64);
    }

    /// Ors PE into SW when `r` is non-zero.
    fn flag_nonzero_to_pe(&mut self, r: Reg) {
        let zero = self.a.label();
        self.a.cbz(Size::X, r, zero);
        let ok = self.a.logic_imm(Logic::Orr, Size::W, SW, SW, 1 << PE_SHIFT);
        assert!(ok);
        self.a.bind(zero);
    }

    /// Leaves unless the x87 and the SSE rounding control (the FPCR one) are nearest.
    fn check_rc_nearest(&mut self, bail: Label) {
        if self.x87.rc_checked {
            return;
        }
        self.x87.rc_checked = true;
        self.a.ldr(Width::W, T_A, STATE, state::FCW as i32);
        self.a.ubfx(Size::W, T_A, T_A, 10, 2);
        self.a.cbnz(Size::W, T_A, bail);
        self.a.ldr(Width::W, T_A, STATE, state::MXCSR as i32);
        self.a.ubfx(Size::W, T_A, T_A, 13, 2);
        self.a.cbnz(Size::W, T_A, bail);
    }

    fn x87_commit(&mut self, eip: u32, insn: &Insn, seg: Option<u8>) {
        let fop = ((insn.op as u32 & 7) << 8) | insn.modrm as u32;
        self.x87.meta = true;
        self.x87.note = (eip, 1 << 31 | fop);
        if let Some(seg) = seg {
            self.a.str(Width::W, T_ADDR, STATE, state::FDP as i32);
            self.a.movz(Size::W, U, 0x100 | seg as u32, 0);
            self.a.str(Width::W, U, STATE, state::FDSEG as i32);
        }
    }

    /// The f64 bits of the 80-bit M and S into T_C, rounded to nearest as in
    /// `F80::to_f64`; leaves unless zero or normal in f64 range (and exact, with `exact`).
    fn to_f64(&mut self, bail: Label, exact: bool) {
        let (slow, done) = (self.a.label(), self.a.label());
        self.and_imm(Size::W, T_A, S, 0x7fff);
        self.a.mov_imm(Size::W, T_C, 0x3c01);
        self.a.sub(Size::W, T_B, T_A, T_C);
        self.a.tbnz(T_B, 31, slow);
        self.a.add_imm(Size::W, T_C, T_B, 3);
        self.a.lsr_imm(Size::W, T_C, T_C, 11);
        self.a.cbnz(Size::W, T_C, slow);
        self.a.tbz(M, 63, bail);
        if exact {
            self.and_imm(Size::X, T_C, M, 0x7ff);
            self.a.cbnz(Size::X, T_C, bail);
        }
        self.a.lsr_imm(Size::X, T_C, M, 11);
        if !exact {
            self.and_imm(Size::X, T_A, M, 0x7ff);
            self.and_imm(Size::X, T_E, T_C, 1);
            self.a.add(Size::X, T_A, T_A, T_E);
            self.a.add_imm(Size::X, T_A, T_A, 0x3ff);
            self.a.lsr_imm(Size::X, T_A, T_A, 11);
        }
        self.and_imm(Size::X, T_C, T_C, (1 << 52) - 1);
        self.a.add_imm(Size::W, T_B, T_B, 1);
        self.a.logic_shift(Logic::Orr, false, Size::X, T_C, T_C, T_B, Shift::Lsl, 52);
        if !exact {
            self.a.add(Size::X, T_C, T_C, T_A);
        }
        self.a.lsr_imm(Size::W, T_E, S, 15);
        self.a.logic_shift(Logic::Orr, false, Size::X, T_C, T_C, T_E, Shift::Lsl, 63);
        self.a.b(done);
        self.a.bind(slow);
        self.a.cbnz(Size::W, T_A, bail);
        self.a.cbnz(Size::X, M, bail);
        self.a.lsr_imm(Size::W, T_E, S, 15);
        self.a.lsl_imm(Size::X, T_C, T_E, 63);
        self.a.bind(done);
    }

    /// The signed integer in T_A as an exact 80-bit M and S.
    fn int_to_f80(&mut self) {
        let done = self.a.label();
        self.a.movz(Size::X, M, 0, 0);
        self.a.movz(Size::W, S, 0, 0);
        self.a.cbz(Size::X, T_A, done);
        self.a.asr_imm(Size::X, T_B, T_A, 63);
        self.a.eor(Size::X, T_C, T_A, T_B);
        self.a.sub(Size::X, T_C, T_C, T_B);
        self.a.clz(Size::X, T_E, T_C);
        self.a.lslv(Size::X, M, T_C, T_E);
        self.a.mov_imm(Size::W, S, 0x403e);
        self.a.sub(Size::W, S, S, T_E);
        self.and_imm(Size::W, T_B, T_B, 1);
        self.a.logic_shift(Logic::Orr, false, Size::W, S, S, T_B, Shift::Lsl, 15);
        self.a.bind(done);
    }

    /// The memory operand at T_ADDR as f64 in `dst`; leaves for NaNs,
    /// infinities and denormals, which the interpreter flags.
    fn load_src(&mut self, src: Src, dst: V, bail: Label) {
        match src {
            Src::F32 => {
                let ok = self.a.label();
                self.a.ldr(Width::W, T_A, T_ADDR, 0);
                self.a.ubfx(Size::W, T_B, T_A, 23, 8);
                self.a.add_imm(Size::W, T_C, T_B, 1);
                self.a.tbnz(T_C, 8, bail);
                self.a.cbnz(Size::W, T_B, ok);
                self.a.lsl_imm(Size::W, T_C, T_A, 1);
                self.a.cbnz(Size::W, T_C, bail);
                self.a.bind(ok);
                self.a.fmov_sw(dst, T_A);
                self.a.fp1(false, 5, dst, dst);
            }
            Src::F64 => {
                let ok = self.a.label();
                self.a.ldr(Width::X, T_A, T_ADDR, 0);
                self.a.ubfx(Size::X, T_B, T_A, 52, 11);
                self.a.add_imm(Size::W, T_C, T_B, 1);
                self.a.tbnz(T_C, 11, bail);
                self.a.cbnz(Size::W, T_B, ok);
                self.a.lsl_imm(Size::X, T_C, T_A, 1);
                self.a.cbnz(Size::X, T_C, bail);
                self.a.bind(ok);
                self.a.fmov_dx(dst, T_A);
            }
            Src::I32 => {
                self.a.ldr(Width::W, T_A, T_ADDR, 0);
                self.a.scvtf(true, Size::W, dst, T_A);
            }
            Src::I16 => {
                self.a.ldrs(Width::H, Size::W, T_A, T_ADDR, 0);
                self.a.scvtf(true, Size::W, dst, T_A);
            }
        }
    }

    /// `a` op `b`, or the reverse, by the ModRM operation; result in FR.
    fn arith(&mut self, reg: usize, a: V, b: V, bail: Label) {
        self.check_rc_nearest(bail);
        let (x, y) = if reg == 5 || reg == 7 { (b, a) } else { (a, b) };
        let op = match reg {
            0 => 2,
            1 => 0,
            4 | 5 => 3,
            _ => 1,
        };
        self.a.fp2(true, op, FR, x, y);
        let zero_needs = match op {
            0 => Some((x, Some(y))),
            1 => Some((x, None)),
            _ => None,
        };
        self.check_result(FR, zero_needs, bail);
        self.track_inexact(op, x, y);
        self.clear_c1();
    }

    /// FCOM's condition codes for `a` against `b`.
    fn set_cc(&mut self, a: V, b: V) {
        self.a.mov_imm(Size::W, T_A, (C0 | C1 | C2 | C3) as u64);
        self.a.bic(Size::W, SW, SW, T_A);
        self.a.fcm(Fcm::Eq, true, FR, a, b);
        self.a.fmov_xd(T_A, FR);
        self.and_imm(Size::W, T_A, T_A, 1);
        self.a.logic_shift(Logic::Orr, false, Size::W, SW, SW, T_A, Shift::Lsl, 14);
        self.a.fcm(Fcm::Gt, true, FR, b, a);
        self.a.fmov_xd(T_A, FR);
        self.and_imm(Size::W, T_A, T_A, 1);
        self.a.logic_shift(Logic::Orr, false, Size::W, SW, SW, T_A, Shift::Lsl, 8);
    }

    /// `src` rounded to an integral value into `dst` by the control word's RC.
    fn round_by_rc(&mut self, dst: V, src: V) {
        let (z, m, p, done) = (self.a.label(), self.a.label(), self.a.label(), self.a.label());
        self.a.ldr(Width::W, T_E, STATE, state::FCW as i32);
        self.a.ubfx(Size::W, T_E, T_E, 10, 2);
        self.a.tbnz(T_E, 0, z);
        self.a.tbnz(T_E, 1, p);
        self.a.fp1(true, 8, dst, src);
        self.a.b(done);
        self.a.bind(z);
        self.a.tbz(T_E, 1, m);
        self.a.fp1(true, 11, dst, src);
        self.a.b(done);
        self.a.bind(m);
        self.a.fp1(true, 10, dst, src);
        self.a.b(done);
        self.a.bind(p);
        self.a.fp1(true, 9, dst, src);
        self.a.bind(done);
    }
}

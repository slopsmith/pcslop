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
const FA: V = V(18);
const FB: V = V(19);
const FR: V = V(20);
const FS: V = V(21);

const C0: u32 = 1 << 8;
const C1: u32 = 1 << 9;
const C2: u32 = 1 << 10;
const C3: u32 = 1 << 14;
const PE_SHIFT: u32 = 5;

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
    LdConst { mant: u64, se: u16 },
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
        (1, 0xe8) => K::LdConst { mant: 1 << 63, se: 0x3fff },
        (1, 0xee) => K::LdConst { mant: 0, se: 0 },
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
        match k {
            K::LdMem(src) => {
                self.x87_need(7, false, bail);
                self.load_src(src, FB, bail);
                self.from_f64(FB, bail);
                self.clear_c1();
                self.fpush();
            }
            K::Ild(w) => {
                self.x87_need(7, false, bail);
                match w {
                    Width::X => self.a.ldr(Width::X, T_A, T_ADDR, 0),
                    _ => self.a.ldrs(w, Size::X, T_A, T_ADDR, 0),
                }
                self.int_to_f80();
                self.clear_c1();
                self.fpush();
            }
            K::StMem { double, pop } => {
                self.x87_need(0, true, bail);
                self.fget(0, M, S);
                self.check_rc_nearest(bail);
                if double {
                    self.to_f64(bail, false);
                    self.and_imm(Size::X, T_B, M, 0x7ff);
                    self.flag_nonzero_to_pe(T_B);
                    self.store(Width::X, T_C);
                } else {
                    self.to_f64(bail, true);
                    self.a.fmov_dx(FA, T_C);
                    self.fp_begin();
                    self.a.fp1(true, 4, FB, FA);
                    self.fp_end(bail);
                    self.a.fmov_ws(T_A, FB);
                    let normal = self.a.label();
                    self.a.ubfx(Size::W, T_B, T_A, 23, 8);
                    self.a.cbnz(Size::W, T_B, normal);
                    self.a.lsl_imm(Size::W, T_B, T_A, 1);
                    self.a.cbnz(Size::W, T_B, bail);
                    self.a.bind(normal);
                    self.store(Width::W, T_A);
                }
                if pop {
                    self.fpop();
                }
            }
            K::Ist { w, pop } => {
                self.x87_need(0, true, bail);
                self.fget(0, M, S);
                self.to_f64(bail, true);
                self.a.fmov_dx(FA, T_C);
                self.round_by_rc(FB, FA);
                self.a.fcm(Fcm::Eq, true, FR, FB, FA);
                self.a.fmov_xd(T_B, FR);
                self.a.mvn(Size::X, T_B, T_B);
                self.flag_nonzero_to_pe(T_B);
                self.a.fcm(Fcm::Agt, true, FR, FB, FA);
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
                self.fget(i, M, S);
                self.clear_c1();
                self.fpush();
            }
            K::StReg { i, pop } => {
                self.x87_need(0, true, bail);
                self.fget(0, M, S);
                self.clear_c1();
                self.fput(i, M, S);
                if pop {
                    self.fpop();
                }
            }
            K::Xch(i) => {
                self.x87_need(0, true, bail);
                self.x87_need(i, true, bail);
                self.fget(0, M, S);
                self.fget(i, T_A, T_B);
                self.fput(0, T_A, T_B);
                self.fput(i, M, S);
                self.clear_c1();
            }
            K::Chs | K::Abs => {
                self.x87_need(0, true, bail);
                self.fget(0, M, S);
                if matches!(k, K::Chs) {
                    let ok = self.a.logic_imm(Logic::Eor, Size::W, S, S, 0x8000);
                    assert!(ok);
                } else {
                    self.and_imm(Size::W, S, S, 0x7fff);
                }
                self.clear_c1();
                self.fput(0, M, S);
            }
            K::LdConst { mant, se } => {
                self.x87_need(7, false, bail);
                self.a.mov_imm(Size::X, M, mant);
                self.a.mov_imm(Size::W, S, se as u64);
                self.clear_c1();
                self.fpush();
            }
            K::Arith { reg, i, dst, pop } => {
                self.x87_need(0, true, bail);
                self.x87_need(i, true, bail);
                self.load_st_f64(0, FA, bail, false);
                self.load_st_f64(i, FB, bail, false);
                self.arith(reg, bail);
                self.fput(dst, M, S);
                if pop {
                    self.fpop();
                }
            }
            K::ArithMem { reg, src } => {
                self.x87_need(0, true, bail);
                self.load_src(src, FB, bail);
                self.load_st_f64(0, FA, bail, false);
                self.arith(reg, bail);
                self.fput(0, M, S);
            }
            K::Com { i, pops } => {
                self.x87_need(0, true, bail);
                self.x87_need(i, true, bail);
                self.load_st_f64(0, FA, bail, true);
                self.load_st_f64(i, FB, bail, true);
                self.set_cc();
                for _ in 0..pops {
                    self.fpop();
                }
            }
            K::ComMem { src, pop } => {
                self.x87_need(0, true, bail);
                self.load_src(src, FB, bail);
                self.load_st_f64(0, FA, bail, true);
                self.set_cc();
                if pop {
                    self.fpop();
                }
            }
            K::Tst => {
                self.x87_need(0, true, bail);
                self.load_st_f64(0, FA, bail, true);
                self.a.fmov_dx(FB, ZR);
                self.set_cc();
            }
            K::Comi { i, pop } => {
                self.x87_need(0, true, bail);
                self.x87_need(i, true, bail);
                self.load_st_f64(0, FA, bail, true);
                self.load_st_f64(i, FB, bail, true);
                if pop {
                    self.fpop();
                }
            }
            K::Sqrt => {
                self.x87_need(0, true, bail);
                self.check_rc_nearest(bail);
                self.load_st_f64(0, FA, bail, false);
                self.fp_begin();
                self.a.fp1(true, 3, FR, FA);
                self.fp_end(bail);
                self.from_f64(FR, bail);
                self.clear_c1();
                self.fput(0, M, S);
            }
            K::Rndint => {
                self.x87_need(0, true, bail);
                self.load_st_f64(0, FA, bail, true);
                self.round_by_rc(FR, FA);
                self.a.fcm(Fcm::Eq, true, FS, FR, FA);
                self.a.fmov_xd(T_B, FS);
                self.a.mvn(Size::X, T_B, T_B);
                self.flag_nonzero_to_pe(T_B);
                self.from_f64(FR, bail);
                self.clear_c1();
                self.fput(0, M, S);
            }
            K::StswAx => self.a.bfi(Size::W, guest_reg(0), SW, 0, 16),
            K::StswMem => self.store(Width::H, SW),
            K::StcwMem => {
                self.a.ldr(Width::W, T_A, STATE, state::FCW as i32);
                self.store(Width::H, T_A);
            }
            K::Ldcw => self.fldcw(bail),
        }
        if !control {
            self.x87_commit(eip, insn, seg);
        }
        if matches!(k, K::Comi { .. }) {
            self.a.fcmp(true, false, FA, FB);
            self.a.axflag();
            self.a.movz(Size::W, PF_RESULT, 1, 0);
            self.a.movz(Size::W, AF_AUX, 0, 0);
        }
        self.a.b(done);
        self.a.bind(bail);
        self.exit_interp(eip);
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
        self.a.ldr(Width::W, U, STATE, state::X87_OK as i32);
        self.a.cbz(Size::W, U, bail);
        self.a.ldr(Width::W, U, STATE, state::MM_USED as i32);
        self.a.tbnz(U, 8, bail);
        self.a.ldr(Width::W, SW, STATE, state::FSW as i32);
        self.a.ubfx(Size::W, TOP, SW, 11, 3);
        self.a.ldr(Width::W, TAGS, STATE, state::FTAGS as i32);
    }

    fn phys(&mut self, i: usize) {
        if i == 0 {
            self.a.mov(Size::W, P, TOP);
        } else {
            self.a.add_imm(Size::W, P, TOP, i as u32);
            self.and_imm(Size::W, P, P, 7);
        }
    }

    /// Leaves unless ST(i) is full (`full`) or empty.
    fn x87_need(&mut self, i: usize, full: bool, bail: Label) {
        self.phys(i);
        self.a.lsrv(Size::W, U, TAGS, P);
        if full {
            self.a.tbz(U, 0, bail);
        } else {
            self.a.tbnz(U, 0, bail);
        }
    }

    fn fget(&mut self, i: usize, m: Reg, s: Reg) {
        self.phys(i);
        self.a.add_shift(Size::X, U, STATE, P, Shift::Lsl, 3);
        self.a.ldr(Width::X, m, U, state::MM as i32);
        self.a.add_shift(Size::X, U, STATE, P, Shift::Lsl, 1);
        self.a.ldr(Width::H, s, U, state::FSE as i32);
    }

    fn load_st_f64(&mut self, i: usize, dst: V, bail: Label, exact: bool) {
        self.fget(i, M, S);
        self.to_f64(bail, exact);
        self.a.fmov_dx(dst, T_C);
    }

    fn fput(&mut self, i: usize, m: Reg, s: Reg) {
        self.phys(i);
        self.write_phys(m, s);
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
    }

    fn fpop(&mut self) {
        self.a.ubfx(Size::W, P, SW, 11, 3);
        self.a.movz(Size::W, U, 1, 0);
        self.a.lslv(Size::W, U, U, P);
        self.a.bic(Size::W, TAGS, TAGS, U);
        self.a.add_imm(Size::W, P, P, 1);
        self.a.bfi(Size::W, SW, P, 11, 3);
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

    fn check_rc_nearest(&mut self, bail: Label) {
        self.a.ldr(Width::W, T_A, STATE, state::FCW as i32);
        self.a.ubfx(Size::W, T_A, T_A, 10, 2);
        self.a.cbnz(Size::W, T_A, bail);
    }

    fn x87_commit(&mut self, eip: u32, insn: &Insn, seg: Option<u8>) {
        self.a.str(Width::W, SW, STATE, state::FSW as i32);
        self.a.str(Width::W, TAGS, STATE, state::FTAGS as i32);
        self.a.mov_imm(Size::W, U, eip as u64);
        self.a.str(Width::W, U, STATE, state::FIP as i32);
        let fop = ((insn.op as u32 & 7) << 8) | insn.modrm as u32;
        self.a.mov_imm(Size::W, U, (1 << 31 | fop) as u64);
        self.a.str(Width::W, U, STATE, state::FNOTE as i32);
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

    /// The f64 in `src` as the 80-bit M and S; leaves unless zero or normal.
    fn from_f64(&mut self, src: V, bail: Label) {
        let (zero, done) = (self.a.label(), self.a.label());
        self.a.fmov_xd(T_A, src);
        self.a.ubfx(Size::X, T_B, T_A, 52, 11);
        self.a.cbz(Size::W, T_B, zero);
        self.a.add_imm(Size::W, T_C, T_B, 1);
        self.a.tbnz(T_C, 11, bail);
        self.a.lsl_imm(Size::X, M, T_A, 11);
        let ok = self.a.logic_imm(Logic::Orr, Size::X, M, M, 1 << 63);
        assert!(ok);
        self.a.mov_imm(Size::W, T_C, 0x3c00);
        self.a.add(Size::W, S, T_B, T_C);
        self.a.lsr_imm(Size::X, T_E, T_A, 63);
        self.a.logic_shift(Logic::Orr, false, Size::W, S, S, T_E, Shift::Lsl, 15);
        self.a.b(done);
        self.a.bind(zero);
        self.a.lsl_imm(Size::X, T_C, T_A, 1);
        self.a.cbnz(Size::X, T_C, bail);
        self.a.movz(Size::X, M, 0, 0);
        self.a.lsr_imm(Size::X, S, T_A, 63);
        self.a.lsl_imm(Size::W, S, S, 15);
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

    fn fp_begin(&mut self) {
        self.a.msr(sysreg::FPSR, ZR);
    }

    /// Leaves on any FP exception but inexact, which becomes PE.
    fn fp_end(&mut self, bail: Label) {
        self.a.mrs(T_A, sysreg::FPSR);
        self.and_imm(Size::W, T_B, T_A, 0xf);
        self.a.cbnz(Size::W, T_B, bail);
        self.a.ubfx(Size::W, T_B, T_A, 4, 1);
        self.a.logic_shift(Logic::Orr, false, Size::W, SW, SW, T_B, Shift::Lsl, PE_SHIFT);
    }

    /// FA op FB, or the reverse, by the ModRM operation; result in M and S.
    fn arith(&mut self, reg: usize, bail: Label) {
        self.check_rc_nearest(bail);
        let (x, y) = if reg == 5 || reg == 7 { (FB, FA) } else { (FA, FB) };
        let op = match reg {
            0 => 2,
            1 => 0,
            4 | 5 => 3,
            _ => 1,
        };
        self.fp_begin();
        self.a.fp2(true, op, FR, x, y);
        self.fp_end(bail);
        self.from_f64(FR, bail);
        self.clear_c1();
    }

    /// FCOM's condition codes for FA against FB.
    fn set_cc(&mut self) {
        self.a.mov_imm(Size::W, T_A, (C0 | C1 | C2 | C3) as u64);
        self.a.bic(Size::W, SW, SW, T_A);
        self.a.fcm(Fcm::Eq, true, FR, FA, FB);
        self.a.fmov_xd(T_A, FR);
        self.and_imm(Size::W, T_A, T_A, 1);
        self.a.logic_shift(Logic::Orr, false, Size::W, SW, SW, T_A, Shift::Lsl, 14);
        self.a.fcm(Fcm::Gt, true, FR, FB, FA);
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

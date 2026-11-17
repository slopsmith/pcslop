//! SSE and SSE2 on NEON. XMM registers live in the runtime state and are
//! loaded per instruction. Arithmetic runs on the A64 FPU in FEAT_AFP mode
//! (see `state::fpcr_for` and docs/sse-neon.md), whose NaN, MIN/MAX, flush and
//! flag rules match x86; FPSR then accumulates the MXCSR flags, except DE,
//! which A64 has no equivalent for: translated code never sets MXCSR.DE.

use a64::{Fcm, Label, Logic, Size, V, Width};
use x86::decode::{Insn, Rep};

use super::{Emit, T_A, T_ADDR, T_B, T_C, T_D, simd_three};
use crate::runtime::{AF_AUX, PF_RESULT, STATE, guest_reg};
use crate::state;

const XA: V = V(22);
const XB: V = V(23);
const XR: V = V(24);
const XT: V = V(25);

const NP: u8 = 0;
const P66: u8 = 1;
const F3: u8 = 2;
const F2: u8 = 3;

fn prefix(insn: &Insn) -> u8 {
    match insn.rep {
        Rep::Rep => F3,
        Rep::Repne => F2,
        Rep::None if insn.opsize_prefix => P66,
        Rep::None => NP,
    }
}

fn xmm(n: usize) -> i32 {
    (state::XMM + 16 * n) as i32
}

/// Whether `insn` is an SSE instruction this module handles, given its opcode
/// and mandatory prefix; the translator tries it before anything else.
pub(super) fn is_sse(insn: &Insn) -> bool {
    let pfx = prefix(insn);
    let mem = insn.mem.is_some();
    match insn.op {
        0x110 | 0x111 | 0x158 | 0x159 | 0x15c | 0x15e | 0x15d | 0x15f | 0x151 | 0x1c2 => true,
        0x128 | 0x129 | 0x114 | 0x115 | 0x154..=0x157 | 0x1c6 | 0x12e | 0x12f => pfx <= P66,
        0x12b => pfx <= P66 && mem,
        0x112 | 0x116 => pfx == NP || (pfx == P66 && mem),
        0x113 | 0x117 => pfx <= P66 && mem,
        0x12a | 0x12c | 0x12d | 0x15a => pfx >= F3,
        0x16f | 0x17f => pfx == P66 || pfx == F3,
        0x16e | 0x1d6 | 0x170 | 0x1db | 0x1df | 0x1eb | 0x1ef => pfx == P66,
        0x17e => pfx == P66 || pfx == F3,
        0x1e7 => pfx == P66 && mem,
        0x160..=0x16d | 0x1d4 | 0x1fb | 0x1e4 | 0x1e5 | 0x1f4..=0x1f6 => pfx == P66,
        op if simd_three(op).is_some() => pfx == P66,
        0x171 | 0x172 => pfx == P66 && !mem && matches!(insn.reg(), 2 | 4 | 6),
        0x173 => pfx == P66 && !mem && matches!(insn.reg(), 2 | 3 | 6 | 7),
        0x1c4 => pfx == P66,
        0x1c5 | 0x1d7 => pfx == P66 && !mem,
        0x1d1..=0x1d3 | 0x1e1 | 0x1e2 | 0x1f1..=0x1f3 => pfx == P66,
        _ => false,
    }
}

impl Emit {
    /// Translates an SSE instruction that `is_sse` accepted; false leaves it
    /// to the interpreter.
    pub(super) fn sse(&mut self, insn: &Insn, eip: u32) -> bool {
        if insn.mem.is_some() && !insn.addr32 {
            return false;
        }
        self.x87_flush();
        self.x87 = super::x87::X87Block::default();
        let pfx = prefix(insn);
        let (op, reg, rm) = (insn.op, insn.reg(), insn.rm());
        let scalar = pfx >= F3;
        let double = pfx == P66 || pfx == F2;
        let arith = matches!(op, 0x158 | 0x159 | 0x15c | 0x15e | 0x15d | 0x15f | 0x151 | 0x1c2 | 0x12e | 0x12f | 0x12a | 0x12c | 0x12d | 0x15a);
        let bail = self.a.label();
        let done = self.a.label();
        if let Some(m) = insn.mem {
            if !self.ea(insn, &m) {
                return false;
            }
        }
        self.a.ldr(Width::W, T_A, STATE, if arith { state::SSE_FP_OK } else { state::SSE_OK } as i32);
        self.a.cbz(Size::W, T_A, bail);
        match (op, pfx) {
            (0x110, NP | P66) | (0x128, _) | (0x16f, _) => {
                self.src(insn, 16, op != 0x110 && !(op == 0x16f && pfx == F3), XB, bail);
                self.a.str_q(XB, STATE, xmm(reg));
            }
            (0x111, NP | P66) | (0x129, _) | (0x12b, _) | (0x17f, _) | (0x1e7, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                let aligned = !(op == 0x111 || (op == 0x17f && pfx == F3));
                self.dst16(insn, XA, aligned, bail);
            }
            (0x110, _) => {
                let n = if pfx == F3 { 4 } else { 8 };
                if insn.mem.is_some() {
                    self.load_mem(n, XB);
                    self.a.str_q(XB, STATE, xmm(reg));
                } else {
                    self.a.ldr_q(XA, STATE, xmm(reg));
                    self.a.ldr_q(XB, STATE, xmm(rm));
                    self.a.ins(if n == 4 { 2 } else { 3 }, XA, 0, XB, 0);
                    self.a.str_q(XA, STATE, xmm(reg));
                }
            }
            (0x111, _) => {
                let n = if pfx == F3 { 4 } else { 8 };
                if insn.mem.is_some() {
                    self.a.ldr(if n == 4 { Width::W } else { Width::X }, T_A, STATE, xmm(reg));
                    self.store(if n == 4 { Width::W } else { Width::X }, T_A);
                } else {
                    self.a.ldr_q(XA, STATE, xmm(rm));
                    self.a.ldr_q(XB, STATE, xmm(reg));
                    self.a.ins(if n == 4 { 2 } else { 3 }, XA, 0, XB, 0);
                    self.a.str_q(XA, STATE, xmm(rm));
                }
            }
            (0x112 | 0x116, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                let lane = (op == 0x116) as u32;
                if insn.mem.is_some() {
                    self.load_mem(8, XB);
                    self.a.ins(3, XA, lane, XB, 0);
                } else {
                    self.a.ldr_q(XB, STATE, xmm(rm));
                    self.a.ins(3, XA, lane, XB, 1 - lane);
                }
                self.a.str_q(XA, STATE, xmm(reg));
            }
            (0x113 | 0x117, _) => {
                self.a.ldr(Width::X, T_A, STATE, xmm(reg) + 8 * (op == 0x117) as i32);
                self.store(Width::X, T_A);
            }
            (0x114 | 0x115, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.src(insn, 16, true, XB, bail);
                self.a.zip(true, if double { 3 } else { 2 }, op == 0x115, XR, XA, XB);
                self.a.str_q(XR, STATE, xmm(reg));
            }
            (0x154..=0x157, _) | (0x1db | 0x1df | 0x1eb | 0x1ef, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.src(insn, 16, true, XB, bail);
                match op {
                    0x154 | 0x1db => self.a.vec3(true, false, 0, 0b00011, XR, XA, XB),
                    0x155 | 0x1df => self.a.vec3(true, false, 1, 0b00011, XR, XB, XA),
                    0x156 | 0x1eb => self.a.vec3(true, false, 2, 0b00011, XR, XA, XB),
                    _ => self.a.vec3(true, true, 0, 0b00011, XR, XA, XB),
                }
                self.a.str_q(XR, STATE, xmm(reg));
            }
            (0x1c6, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.src(insn, 16, true, XB, bail);
                let imm = insn.imm;
                if double {
                    self.a.ins(3, XR, 0, XA, imm & 1);
                    self.a.ins(3, XR, 1, XB, (imm >> 1) & 1);
                } else {
                    for i in 0..4 {
                        self.a.ins(2, XR, i, if i < 2 { XA } else { XB }, (imm >> (2 * i)) & 3);
                    }
                }
                self.a.str_q(XR, STATE, xmm(reg));
            }
            (0x170, _) => {
                self.src(insn, 16, true, XB, bail);
                for i in 0..4 {
                    self.a.ins(2, XR, i, XB, (insn.imm >> (2 * i)) & 3);
                }
                self.a.str_q(XR, STATE, xmm(reg));
            }
            (0x171..=0x173, _) => {
                self.a.ldr_q(XA, STATE, xmm(rm));
                let n = insn.imm & 0xff;
                let esize = match op {
                    0x171 => 16,
                    0x172 => 32,
                    _ => 64,
                };
                match (op, reg) {
                    (0x173, 3 | 7) if n >= 16 => self.a.vec3(true, true, 0, 0b00011, XA, XA, XA),
                    (0x173, 3 | 7) if n > 0 => {
                        self.a.vec3(true, true, 0, 0b00011, XT, XT, XT);
                        if reg == 3 { self.a.ext(XA, XA, XT, n) } else { self.a.ext(XA, XT, XA, 16 - n) }
                    }
                    (_, 2 | 6) if n >= esize => self.a.vec3(true, true, 0, 0b00011, XA, XA, XA),
                    (_, 6) if n > 0 => self.a.shl_v(true, esize, XA, XA, n),
                    (_, 2) if n > 0 => self.a.shr_v(true, false, esize, XA, XA, n),
                    (_, 4) if n > 0 => self.a.shr_v(true, true, esize, XA, XA, n.min(esize)),
                    _ => {}
                }
                self.a.str_q(XA, STATE, xmm(rm));
            }
            (0x1d1..=0x1d3 | 0x1e1 | 0x1e2 | 0x1f1..=0x1f3, _) => {
                let (esize, log) = match op & 3 {
                    1 => (16, 4),
                    2 => (32, 5),
                    _ => (64, 6),
                };
                let (right, arith) = (op < 0x1f0, op & 0xf0 == 0xe0);
                self.src(insn, 16, true, XB, bail);
                self.a.fmov_xd(T_C, XB);
                let small = self.a.label();
                self.a.lsr_imm(Size::X, T_D, T_C, log);
                self.a.cbz(Size::X, T_D, small);
                self.a.movz(Size::W, T_C, esize, 0);
                self.a.bind(small);
                if right {
                    self.a.neg(Size::W, T_C, T_C);
                }
                let ok = self.a.logic_imm(Logic::And, Size::W, T_C, T_C, 0xff);
                assert!(ok);
                if esize == 16 {
                    self.a.logic_shift(Logic::Orr, false, Size::W, T_C, T_C, T_C, a64::Shift::Lsl, 16);
                }
                if esize <= 32 {
                    self.a.logic_shift(Logic::Orr, false, Size::X, T_C, T_C, T_C, a64::Shift::Lsl, 32);
                }
                self.a.fmov_dx(XT, T_C);
                self.a.ins(3, XT, 1, XT, 0);
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.a.vec3(true, !arith, log - 3, 0b01000, XA, XA, XT);
                self.a.str_q(XA, STATE, xmm(reg));
            }
            (0x1c4, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                match insn.mem {
                    Some(_) => self.a.ldr(Width::H, T_A, T_ADDR, 0),
                    None => self.a.mov(Size::W, T_A, guest_reg(rm)),
                }
                self.a.ins_h(XA, insn.imm & 7, T_A);
                self.a.str_q(XA, STATE, xmm(reg));
            }
            (0x1c5, _) => {
                self.a.ldr_q(XB, STATE, xmm(rm));
                self.a.umov_h(guest_reg(reg), XB, insn.imm & 7);
            }
            (0x1d7, _) => {
                // The shift-and-accumulate mask gathering of sse2neon's
                // _mm_movemask_epi8 (sse2neon contributors, MIT licence).
                self.a.ldr_q(XB, STATE, xmm(rm));
                self.a.shr_v(true, false, 8, XR, XB, 7);
                self.a.usra(true, 16, XR, XR, 7);
                self.a.usra(true, 32, XR, XR, 14);
                self.a.usra(true, 64, XR, XR, 28);
                self.a.umov_b(T_A, XR, 0);
                self.a.umov_b(T_B, XR, 8);
                self.a.logic_shift(Logic::Orr, false, Size::W, T_A, T_A, T_B, a64::Shift::Lsl, 8);
                self.a.mov(Size::W, guest_reg(reg), T_A);
            }
            (_, P66) if matches!(op, 0x160..=0x16d | 0x1d4 | 0x1fb | 0x1e4 | 0x1e5 | 0x1f4..=0x1f6) || simd_three(op).is_some() => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.src(insn, 16, true, XB, bail);
                self.sse_int(op);
                self.a.str_q(XR, STATE, xmm(reg));
            }
            (0x16e, _) => {
                match insn.mem {
                    Some(_) => self.a.ldr_s(XR, T_ADDR, 0),
                    None => self.a.fmov_sw(XR, guest_reg(rm)),
                }
                self.a.str_q(XR, STATE, xmm(reg));
            }
            (0x17e, P66) => {
                self.a.ldr(Width::W, T_A, STATE, xmm(reg));
                match insn.mem {
                    Some(_) => self.store(Width::W, T_A),
                    None => self.a.mov(Size::W, guest_reg(rm), T_A),
                }
            }
            (0x17e, _) => {
                match insn.mem {
                    Some(_) => self.a.ldr_d(XR, T_ADDR, 0),
                    None => self.a.ldr_d(XR, STATE, xmm(rm)),
                }
                self.a.str_q(XR, STATE, xmm(reg));
            }
            (0x1d6, _) => match insn.mem {
                Some(_) => {
                    self.a.ldr(Width::X, T_A, STATE, xmm(reg));
                    self.store(Width::X, T_A);
                }
                None => {
                    self.a.ldr_d(XR, STATE, xmm(reg));
                    self.a.str_q(XR, STATE, xmm(rm));
                }
            },
            (0x158 | 0x159 | 0x15c | 0x15e | 0x15d | 0x15f, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.fp_src(insn, scalar, double, bail);
                let (u, size, opc, sop) = match op {
                    0x158 => (false, 0, 0b11010, 2),
                    0x15c => (false, 2, 0b11010, 3),
                    0x159 => (true, 0, 0b11011, 0),
                    0x15e => (true, 0, 0b11111, 1),
                    0x15d => (false, 2, 0b11110, 5),
                    _ => (false, 0, 0b11110, 4),
                };
                if scalar {
                    self.a.fp2(double, sop, XR, XA, XB);
                } else {
                    self.a.vec3(true, u, size | double as u32, opc, XR, XA, XB);
                }
                self.commit_fp(reg, scalar, double);
            }
            (0x151, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.fp_src(insn, scalar, double, bail);
                if scalar {
                    self.a.fp1(double, 3, XR, XB);
                } else {
                    self.a.fsqrt_v(double, XR, XB);
                }
                self.commit_fp(reg, scalar, double);
            }
            (0x1c2, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.fp_src(insn, scalar, double, bail);
                self.cmp_pred(insn.imm as u8 & 7, scalar, double);
                self.commit_fp(reg, scalar, double);
            }
            (0x12e | 0x12f, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.fp_src(insn, true, double, bail);
                self.a.fcmp(double, op == 0x12f, XA, XB);
                self.a.cset(Size::W, PF_RESULT, a64::Cond::Vc);
                self.a.axflag();
                self.a.movz(Size::W, AF_AUX, 0, 0);
            }
            (0x12a, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                match insn.mem {
                    Some(_) => self.a.ldr(Width::W, T_A, T_ADDR, 0),
                    None => self.a.mov(Size::W, T_A, guest_reg(rm)),
                }
                self.a.scvtf(double, Size::W, XR, T_A);
                self.commit_fp(reg, true, double);
            }
            (0x12c | 0x12d, _) => {
                self.fp_src(insn, true, double, bail);
                self.a.fp1(double, if op == 0x12c { 16 } else { 17 }, XR, XB);
                self.a.fcvtzs(double, Size::W, T_A, XR);
                self.a.mov(Size::W, guest_reg(reg), T_A);
            }
            (0x15a, _) => {
                self.a.ldr_q(XA, STATE, xmm(reg));
                self.fp_src(insn, true, pfx == F2, bail);
                self.a.fp1(pfx == F2, if pfx == F3 { 5 } else { 4 }, XR, XB);
                self.commit_fp(reg, true, pfx == F3);
            }
            _ => unreachable!("is_sse accepted {op:#x} with prefix {pfx}"),
        }
        self.a.b(done);
        self.a.bind(bail);
        self.exit_interp(eip);
        self.a.bind(done);
        true
    }

    /// A 128-bit integer operation on XA and XB into XR.
    fn sse_int(&mut self, op: u16) {
        use a64::Narrow;
        match op {
            0x160..=0x162 => self.a.zip(true, (op - 0x160) as u32, false, XR, XA, XB),
            0x16c => self.a.zip(true, 3, false, XR, XA, XB),
            0x168..=0x16a => self.a.zip(true, (op - 0x168) as u32, true, XR, XA, XB),
            0x16d => self.a.zip(true, 3, true, XR, XA, XB),
            0x163 | 0x167 | 0x16b => {
                let (kind, size) = match op {
                    0x163 => (Narrow::Sqxtn, 0),
                    0x167 => (Narrow::Sqxtun, 0),
                    _ => (Narrow::Sqxtn, 1),
                };
                self.a.narrow(kind, size, XR, XA);
                self.a.narrow_hi(kind, size, XR, XB);
            }
            0x1d4 => self.a.vec3(true, false, 3, 0b10000, XR, XA, XB),
            0x1fb => self.a.vec3(true, true, 3, 0b10000, XR, XA, XB),
            0x1e4 | 0x1e5 => {
                self.a.mull(op == 0x1e5, 1, XR, XA, XB);
                self.a.mull_hi(op == 0x1e5, 1, XT, XA, XB);
                self.a.uzp(true, 1, true, XR, XR, XT);
            }
            0x1f5 => {
                self.a.mull(true, 1, XR, XA, XB);
                self.a.mull_hi(true, 1, XT, XA, XB);
                self.a.vec3(true, false, 2, 0b10111, XR, XR, XT);
            }
            0x1f4 => {
                self.a.narrow(Narrow::Xtn, 2, XR, XA);
                self.a.narrow(Narrow::Xtn, 2, XT, XB);
                self.a.mull(false, 2, XR, XR, XT);
            }
            0x1f6 => {
                self.a.vec3(true, true, 0, 0b01110, XR, XA, XB);
                self.a.uaddlp(true, 0, XR, XR);
                self.a.uaddlp(true, 1, XR, XR);
                self.a.uaddlp(true, 2, XR, XR);
            }
            _ => {
                let (u, size, opcode) = simd_three(op).expect("checked by is_sse");
                self.a.vec3(true, u, size, opcode, XR, XA, XB);
            }
        }
    }

    /// The `rm` operand into `dst`: `n` bytes from memory (zero-extended), or
    /// the whole register. `aligned` memory operands must be 16-byte aligned.
    fn src(&mut self, insn: &Insn, n: u32, aligned: bool, dst: V, bail: Label) {
        match insn.mem {
            Some(_) => {
                if aligned {
                    self.check_align(bail);
                }
                self.load_mem(n, dst);
            }
            None => self.a.ldr_q(dst, STATE, xmm(insn.rm())),
        }
    }

    fn load_mem(&mut self, n: u32, dst: V) {
        match n {
            4 => self.a.ldr_s(dst, T_ADDR, 0),
            8 => self.a.ldr_d(dst, T_ADDR, 0),
            _ => self.a.ldr_q(dst, T_ADDR, 0),
        }
    }

    fn dst16(&mut self, insn: &Insn, v: V, aligned: bool, bail: Label) {
        match insn.mem {
            Some(_) => {
                if aligned {
                    self.check_align(bail);
                }
                self.stored = true;
                self.a.str_q(v, T_ADDR, 0);
            }
            None => self.a.str_q(v, STATE, xmm(insn.rm())),
        }
    }

    fn check_align(&mut self, bail: Label) {
        let ok = self.a.logic_imm(Logic::And, Size::W, T_B, T_ADDR, 15);
        assert!(ok);
        self.a.cbnz(Size::W, T_B, bail);
    }

    fn fp_src(&mut self, insn: &Insn, scalar: bool, double: bool, bail: Label) {
        let n = if !scalar { 16 } else if double { 8 } else { 4 };
        self.src(insn, n, !scalar, XB, bail);
    }

    /// Writes XR to XMM `reg`: the whole register, or for scalar operations
    /// only its low lane into XA (the old destination).
    fn commit_fp(&mut self, reg: usize, scalar: bool, double: bool) {
        if scalar {
            self.a.ins(if double { 3 } else { 2 }, XA, 0, XR, 0);
            self.a.str_q(XA, STATE, xmm(reg));
        } else {
            self.a.str_q(XR, STATE, xmm(reg));
        }
    }

    /// CMPPS-family predicate `pred` on XA and XB into XR.
    fn cmp_pred(&mut self, pred: u8, scalar: bool, double: bool) {
        let fcm = |e: &mut Emit, k: Fcm, rd: V, rn: V, rm: V| {
            if scalar { e.a.fcm(k, double, rd, rn, rm) } else { e.a.fcm_v(k, double, rd, rn, rm) }
        };
        match pred & 3 {
            0 => fcm(self, Fcm::Eq, XR, XA, XB),
            1 => fcm(self, Fcm::Gt, XR, XB, XA),
            2 => fcm(self, Fcm::Ge, XR, XB, XA),
            _ => {
                fcm(self, Fcm::Eq, XR, XA, XA);
                fcm(self, Fcm::Eq, XT, XB, XB);
                self.a.vec3(true, false, 0, 0b00011, XR, XR, XT);
            }
        }
        if matches!(pred, 3..=6) {
            self.a.not_v(XR, XR);
        }
    }
}

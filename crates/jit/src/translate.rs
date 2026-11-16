//! Translates basic blocks of 32-bit flat-mode x86 code to ARM64.
//!
//! Every x86 instruction finishes its memory accesses before it writes guest
//! registers, so a fault on any access leaves the x86 state of the
//! instruction's start (apart from NZCV, which a restart recomputes).

use a64::{Asm, Cond, Extend, Reg, Shift, Size, Width, ZR, r};
use x86::decode::{Insn, MemOperand, NO_REG, Rep, decode};

use crate::runtime::{AF_AUX, Entries, ICOUNT, PF_RESULT, STATE, guest_reg};
use crate::state::{self, reason};

const MAX_INSNS: usize = 64;

const T_ADDR: Reg = Reg(9);
const T_A: Reg = Reg(10);
const T_RES: Reg = Reg(11);
const T_B: Reg = Reg(12);
const T_C: Reg = Reg(13);
const T_D: Reg = Reg(14);
const T_E: Reg = Reg(15);
const CHAIN_TMP: Reg = Reg(16);

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
}

struct Emit {
    a: Asm,
    block_va: u64,
    dispatch_va: u64,
    exit_va: u64,
    /// Linear page of the block start; jumps within it may be chained.
    page: Option<u32>,
    chains: Vec<(u32, u32)>,
    /// Translating CPL 3 code: privileged instructions go to the interpreter.
    user: bool,
}

enum Step {
    Next,
    End,
    Unsupported,
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

    fn goto(&mut self, eip: u32) {
        if self.page == Some(eip & !0xfff) {
            let slow = self.a.label();
            self.a.ldr(Width::W, CHAIN_TMP, STATE, state::PENDING as i32);
            self.a.cbnz(Size::W, CHAIN_TMP, slow);
            self.chains.push((self.a.pos() as u32, eip));
            self.a.b(slow);
            self.a.bind(slow);
        }
        self.goto_dispatch(eip);
    }

    /// Jumps through the dispatcher even within the page, so it can check for interrupts.
    fn goto_dispatch(&mut self, eip: u32) {
        self.a.mov_imm(Size::W, r(0), eip as u64);
        let off = self.branch_offset(self.dispatch_va);
        self.a.b_offset(off);
    }

    fn goto_reg(&mut self, eip: Reg) {
        if eip != r(0) {
            self.a.mov(Size::W, r(0), eip);
        }
        let off = self.branch_offset(self.dispatch_va);
        self.a.b_offset(off);
    }

    fn exit_interp(&mut self, eip: u32) {
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

    /// Computes the effective (linear) address of `m` into T_ADDR.
    fn ea(&mut self, insn: &Insn, m: &MemOperand) -> bool {
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
        self.a.ldr(w, dst, T_ADDR, 0);
    }

    fn store(&mut self, w: Width, src: Reg) {
        self.a.str(w, src, T_ADDR, 0);
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

    /// Group-1 operation `op` on 32-bit values; result in T_RES, flags in NZCV.
    fn alu32(&mut self, op: usize, a: Reg, b: Reg) -> bool {
        match op {
            0 => {
                self.a.adds(Size::W, T_RES, a, b);
                self.a.cfinv();
            }
            1 | 4 | 6 => {
                match op {
                    1 => self.a.orr(Size::W, T_RES, a, b),
                    4 => self.a.and(Size::W, T_RES, a, b),
                    _ => self.a.eor(Size::W, T_RES, a, b),
                }
                self.a.tst(Size::W, T_RES, T_RES);
                self.a.cfinv();
                self.a.mov(Size::W, PF_RESULT, T_RES);
                self.a.mov(Size::W, AF_AUX, ZR);
                return true;
            }
            2 => {
                self.a.cfinv();
                self.a.adcs(Size::W, T_RES, a, b);
                self.a.cfinv();
            }
            3 => self.a.sbcs(Size::W, T_RES, a, b),
            _ => self.a.subs(Size::W, T_RES, a, b),
        }
        self.a.mov(Size::W, PF_RESULT, T_RES);
        self.set_af(a, b, T_RES);
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
        self.a.mov(Size::W, PF_RESULT, T_RES);
        if matches!(op, 1 | 4 | 6) {
            self.a.mov(Size::W, AF_AUX, ZR);
        } else {
            self.set_af(a, b, T_RES);
        }
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
        debug_assert!(ok);
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
                if !self.ea(insn, &m) {
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

    /// With `probe`, a memory operand is written back first, so the later
    /// store cannot fault after the flags have changed.
    fn rm_w(&mut self, insn: &Insn, w: Ow, probe: bool) -> Option<Reg> {
        match insn.mem {
            Some(m) => {
                if !self.ea(insn, &m) {
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
        self.a.tst(Size::W, T_RES, T_RES);
        let ok = self.a.logic_imm(a64::Logic::Eor, Size::W, T_B, T_B, 1);
        debug_assert!(ok);
        self.a.logic_shift(a64::Logic::Orr, false, Size::W, T_C, T_C, T_B, Shift::Lsl, 1);
        self.a.rmif(T_C, 0, 0b0011);
        self.a.mov(Size::W, PF_RESULT, T_RES);
    }

    fn shift_cl(&mut self, kind: usize, a: Reg) {
        let n = T_D;
        let ok = self.a.logic_imm(a64::Logic::And, Size::W, n, guest_reg(1), 31);
        debug_assert!(ok);
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
        debug_assert!(ok);
        if matches!(kind, 4 | 6) {
            self.a.eor(Size::W, T_C, T_C, T_B);
        }
        self.shift_flags();
    }

    /// Signed 32x32 multiply into `dst`, with x86 IMUL flags.
    fn imul32(&mut self, dst: Reg, a: Reg, b: Reg) {
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
        debug_assert!(ok);
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
    fn string_op(&mut self, insn: &Insn, eip: u32) -> bool {
        let op = insn.op;
        if !insn.addr32 || matches!(insn.seg_override, Some(4) | Some(5)) {
            return false;
        }
        let w = Ow::of(op, insn.op32);
        let n = match w {
            Ow::B => 1,
            Ow::H => 2,
            Ow::D => 4,
        };
        let (esi, edi, ecx) = (guest_reg(6), guest_reg(7), guest_reg(1));
        let rep = insn.rep != Rep::None;
        let compares = matches!(op, 0xa6 | 0xa7 | 0xae | 0xaf);
        let slow = self.a.label();
        let done = self.a.label();
        let top = self.a.label();
        self.a.ldr(Width::W, T_D, STATE, state::DF as i32);
        self.a.cbnz(Size::W, T_D, slow);
        self.a.bind(top);
        if rep {
            self.a.cbz(Size::W, ecx, done);
        }
        match op {
            0xa4 | 0xa5 => {
                self.a.ldr(w.mem(), T_A, esi, 0);
                self.a.str(w.mem(), T_A, edi, 0);
                self.a.add_imm(Size::W, esi, esi, n);
                self.a.add_imm(Size::W, edi, edi, n);
            }
            0xaa | 0xab => {
                let v = self.read_gpr(w, 0, T_A);
                self.a.str(w.mem(), v, edi, 0);
                self.a.add_imm(Size::W, edi, edi, n);
            }
            0xac | 0xad => {
                self.a.ldr(w.mem(), T_A, esi, 0);
                self.write_gpr(w, 0, T_A);
                self.a.add_imm(Size::W, esi, esi, n);
            }
            0xa6 | 0xa7 => {
                self.a.ldr(w.mem(), T_A, esi, 0);
                self.a.ldr(w.mem(), T_B, edi, 0);
                self.a.add_imm(Size::W, esi, esi, n);
                self.a.add_imm(Size::W, edi, edi, n);
            }
            _ => {
                self.a.ldr(w.mem(), T_B, edi, 0);
                self.a.add_imm(Size::W, edi, edi, n);
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
        self.a.b(done);
        self.a.bind(slow);
        self.exit_interp(eip);
        self.a.bind(done);
        true
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
        self.a.b(done);
        self.a.bind(slow);
        self.exit_interp(eip);
        self.a.bind(done);
        true
    }

    fn insn(&mut self, insn: &Insn, eip: u32) -> Step {
        let next = eip.wrapping_add(insn.len as u32);
        let op = insn.op;
        let op32 = insn.op32;
        let string = matches!(op, 0xa4..=0xa7 | 0xaa..=0xaf);
        if insn.rep != Rep::None && !(op == 0x90 || string) {
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
                        let Some(a) = self.rm_w(insn, w, writes) else { return Step::Unsupported };
                        let b = self.read_gpr(w, insn.reg(), T_B);
                        self.alu(aop, w, a, b);
                        if writes {
                            self.write_rm(insn, w, T_RES);
                        }
                    }
                    2 | 3 => {
                        let Some(b) = self.rm_w(insn, w, false) else { return Step::Unsupported };
                        let a = self.read_gpr(w, insn.reg(), T_B);
                        self.alu(aop, w, a, b);
                        if writes {
                            self.write_gpr(w, insn.reg(), T_RES);
                        }
                    }
                    _ => {
                        let a = self.read_gpr(w, 0, T_A);
                        self.a.mov_imm(Size::W, T_B, insn.imm as u64 & w.mask());
                        self.alu(aop, w, a, T_B);
                        if writes {
                            self.write_gpr(w, 0, T_RES);
                        }
                    }
                }
            }
            0x40..=0x4f => {
                if !op32 {
                    return Step::Unsupported;
                }
                let reg = guest_reg((op & 7) as usize);
                self.inc_dec(op >= 0x48, reg, reg);
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
                if !op32 && op >= 0x100 {
                    return Step::Unsupported;
                }
                let Some(c) = x86_cond(op) else { return Step::Unsupported };
                let taken = self.a.label();
                self.a.b_cond(c, taken);
                self.goto(next);
                self.a.bind(taken);
                self.goto(next.wrapping_add(rel));
                return Step::End;
            }
            0x80..=0x83 => {
                let aop = insn.reg();
                let w = Ow::of(op, op32);
                let imm = if op == 0x81 { insn.imm } else { insn.imm as u8 as i8 as i32 as u32 };
                let Some(a) = self.rm_w(insn, w, aop != 7) else { return Step::Unsupported };
                self.a.mov_imm(Size::W, T_B, imm as u64 & w.mask());
                self.alu(aop, w, a, T_B);
                if aop != 7 {
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
            0x88 | 0x89 => {
                let w = Ow::of(op, op32);
                let v = self.read_gpr(w, insn.reg(), T_B);
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.store(w.mem(), v);
                    }
                    None => self.write_gpr(w, insn.rm(), v),
                }
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
                self.goto_reg(T_A);
                self.a.bind(slow);
                self.exit_interp(eip);
                return Step::End;
            }
            0xfe if insn.reg() < 2 => {
                let Some(a) = self.rm_w(insn, Ow::B, true) else { return Step::Unsupported };
                self.a.mov(Size::W, T_A, a);
                self.a.movz(Size::W, T_B, 1, 0);
                self.a.cset(Size::W, T_E, Cond::Hs);
                self.alu_narrow(if insn.reg() == 0 { 0 } else { 5 }, T_A, T_B, 24);
                self.a.rmif(T_E, 63, 0b0010);
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
                self.goto_dispatch(next);
                return Step::End;
            }
            0x98 if op32 => self.a.sxth(Size::W, guest_reg(0), guest_reg(0)),
            0x99 if op32 => self.a.asr_imm(Size::W, guest_reg(2), guest_reg(0), 31),
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
                debug_assert!(ok);
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
                if !matches!(kind, 4..=7) {
                    return Step::Unsupported;
                }
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        self.load(Width::W, T_A);
                        self.store(Width::W, T_A);
                        if !self.shift_imm(kind, T_RES, T_A, n) {
                            return Step::Unsupported;
                        }
                        self.store(Width::W, T_RES);
                    }
                    None => {
                        let rg = guest_reg(insn.rm());
                        if !self.shift_imm(kind, rg, rg, n) {
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
                self.goto_reg(T_A);
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
                self.goto(next.wrapping_add(insn.imm));
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
            0xf7 if op32 => match insn.reg() {
                6 | 7 => {
                    if !self.div32(insn, eip, insn.reg() == 7) {
                        return Step::Unsupported;
                    }
                }
                4 => {
                    let Some(b) = self.rm_w(insn, Ow::D, false) else { return Step::Unsupported };
                    self.a.umull(T_RES, guest_reg(0), b);
                    self.a.lsr_imm(Size::X, T_B, T_RES, 32);
                    self.a.cmp_imm(Size::W, T_B, 0);
                    self.a.cset(Size::W, T_C, Cond::Ne);
                    self.a.tst(Size::W, T_RES, T_RES);
                    self.a.movz(Size::W, T_D, 2, 0);
                    self.a.sub(Size::W, T_D, T_D, T_C);
                    self.a.rmif(T_D, 0, 0b0011);
                    self.a.mov(Size::W, PF_RESULT, T_RES);
                    self.a.mov(Size::W, AF_AUX, ZR);
                    self.a.mov(Size::W, guest_reg(0), T_RES);
                    self.a.mov(Size::W, guest_reg(2), T_B);
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
                    }
                    self.goto_reg(T_C);
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
                let Some(c) = x86_cond(op) else { return Step::Unsupported };
                let Some(v) = self.rm32(insn) else { return Step::Unsupported };
                let dst = guest_reg(insn.reg());
                self.a.csel(Size::W, dst, v, dst, c);
            }
            0x190..=0x19f => {
                let Some(c) = x86_cond(op) else { return Step::Unsupported };
                self.a.cset(Size::W, T_B, c);
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
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        if signed {
                            self.a.ldrs(Width::B, Size::W, T_A, T_ADDR, 0);
                        } else {
                            self.load(Width::B, T_A);
                        }
                    }
                    None => {
                        self.read_r8(insn.rm(), T_A);
                        if signed {
                            self.a.sxtb(Size::W, T_A, T_A);
                        }
                    }
                }
                if op32 {
                    self.a.mov(Size::W, guest_reg(insn.reg()), T_A);
                } else {
                    self.a.bfi(Size::W, guest_reg(insn.reg()), T_A, 0, 16);
                }
            }
            0x1b7 | 0x1bf if op32 => {
                let signed = op == 0x1bf;
                match insn.mem {
                    Some(m) => {
                        if !self.ea(insn, &m) {
                            return Step::Unsupported;
                        }
                        if signed {
                            self.a.ldrs(Width::H, Size::W, T_A, T_ADDR, 0);
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
            _ => return Step::Unsupported,
        }
        Step::Next
    }

    fn inc_dec(&mut self, dec: bool, dst: Reg, src: Reg) {
        self.a.cset(Size::W, T_B, Cond::Hs);
        if dec {
            self.a.subs_imm(Size::W, T_RES, src, 1);
        } else {
            self.a.adds_imm(Size::W, T_RES, src, 1);
        }
        self.a.rmif(T_B, 63, 0b0010);
        self.a.mov(Size::W, PF_RESULT, T_RES);
        self.a.eor(Size::W, AF_AUX, src, T_RES);
        if dst != T_RES {
            self.a.mov(Size::W, dst, T_RES);
        }
    }

    fn not_neg(&mut self, neg: bool, dst: Reg, src: Reg) {
        if neg {
            self.a.subs(Size::W, T_RES, ZR, src);
            self.a.mov(Size::W, PF_RESULT, T_RES);
            self.a.eor(Size::W, AF_AUX, src, T_RES);
            if dst != T_RES {
                self.a.mov(Size::W, dst, T_RES);
            }
        } else {
            self.a.mvn(Size::W, dst, src);
        }
    }
}

/// Translates the block starting at `eip`. `byte(eip)` returns the code byte at
/// a linear address on the block's page, or `None` past the page.
pub fn compile(
    eip: u32,
    mut byte: impl FnMut(u32) -> Option<u8>,
    block_va: u64,
    rt_va: u64,
    rt: &Entries,
    chain: bool,
    user: bool,
) -> Compiled {
    let mut e = Emit {
        a: Asm::new(),
        block_va,
        dispatch_va: rt_va + rt.dispatch as u64,
        exit_va: rt_va + rt.exit as u64,
        page: chain.then_some(eip & !0xfff),
        chains: Vec::new(),
        user,
    };
    let mut boundaries = Vec::new();
    let mut pc = eip;
    let mut insns = 0;
    e.a.nop();
    loop {
        let insn = decode::<()>(true, |i| byte(pc.wrapping_add(i)).ok_or(()));
        let Ok(insn) = insn else {
            if insns == 0 {
                e.exit_interp(pc);
            } else {
                e.goto(pc);
            }
            break;
        };
        boundaries.push((e.a.pos() as u32, pc));
        let start = e.a.pos();
        match e.insn(&insn, pc) {
            Step::Next => {
                insns += 1;
                pc = pc.wrapping_add(insn.len as u32);
                if insns >= MAX_INSNS || byte(pc).is_none() {
                    e.goto(pc);
                    break;
                }
            }
            Step::End => {
                insns += 1;
                break;
            }
            Step::Unsupported => {
                e.a.code.truncate(start);
                e.chains.retain(|&(w, _)| (w as usize) < start);
                e.exit_interp(pc);
                break;
            }
        }
    }
    let mut count = Asm::new();
    count.add_imm(Size::X, ICOUNT, ICOUNT, insns as u32);
    let mut code = e.a.finish();
    code[0] = count.code[0];
    Compiled { code, boundaries, insns, chains: e.chains }
}

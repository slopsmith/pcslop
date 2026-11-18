//! x87 FPU instructions (`D8`..`DF` and `FWAIT`).
//!
//! Approximations: arithmetic and transcendental results are computed in
//! `f64` (inputs are first rounded to `f64`), so they carry at most double
//! precision and the `f64` exponent range. C1 is not set as a round-up
//! indicator except on stack faults and integer stores. Unmasked overflow and
//! underflow on register destinations deliver the masked result instead of
//! the exponent-adjusted one.

use std::cmp::Ordering;

use super::mem::Loc;
use super::{Ctx, Exception, Res, ud, vector};
use crate::bus::Bus;
use crate::cpu::{CS, EAX, cr0, flags};
use crate::fparith::{self, Op};
use crate::trig;
use crate::fpu::{F80, exc, rc, sw};

const EQ: u8 = 0;
const LT: u8 = 1;
const GT: u8 = 2;
const UNORDERED: u8 = 3;

impl<B: Bus> Ctx<'_, B> {
    /// Reads `n` bytes (at most 16) as a little-endian integer.
    pub(super) fn read_wide(&mut self, seg: usize, off: u32, n: u32) -> Res<u128> {
        let mut v = 0u128;
        let mut i = 0;
        while i < n {
            let sz = match n - i {
                4.. => 4,
                2 | 3 => 2,
                _ => 1,
            };
            let part = self.read_seg(seg, self.off_add(off, i), sz)?;
            v |= (part as u128) << (8 * i);
            i += sz;
        }
        Ok(v)
    }

    /// Writes `n` bytes (at most 16), faulting before any byte is stored.
    pub(super) fn write_wide(&mut self, seg: usize, off: u32, n: u32, v: u128) -> Res<()> {
        self.probe_span(seg, off, n)?;
        let mut i = 0;
        while i < n {
            let sz = match n - i {
                4.. => 4,
                2 | 3 => 2,
                _ => 1,
            };
            self.write_seg(seg, self.off_add(off, i), sz, (v >> (8 * i)) as u32)?;
            i += sz;
        }
        Ok(())
    }

    /// Checks that the first and last byte of an `n`-byte store are writable.
    pub(super) fn probe_span(&mut self, seg: usize, off: u32, n: u32) -> Res<()> {
        self.probe_write(Loc::Mem(seg, off), 1)?;
        self.probe_write(Loc::Mem(seg, self.off_add(off, n - 1)), 1)
    }

    pub(super) fn fwait(&mut self) -> Res<()> {
        if self.cpu.cr0 & (cr0::TS | cr0::MP) == cr0::TS | cr0::MP {
            return Err(Exception::new(vector::NM).into());
        }
        self.fpu_check_pending()
    }

    /// The pending-exception check that waiting x87 instructions and MMX
    /// instructions perform first: #MF, or FERR# when CR0.NE is clear.
    pub(super) fn fpu_check_pending(&mut self) -> Res<()> {
        if self.cpu.fpu.sw & sw::ES == 0 {
            return Ok(());
        }
        if self.cpu.cr0 & cr0::NE != 0 {
            return Err(Exception::new(vector::MF).into());
        }
        self.cpu.ferr_pending = true;
        Ok(())
    }

    pub(super) fn fpu_sync_ferr(&mut self) {
        if self.cpu.fpu.sw & sw::ES == 0 {
            self.cpu.ferr_pending = false;
        }
    }

    /// Records exception flags (and SF). Returns false when an unmasked IE,
    /// DE or ZE means the destination must stay unchanged.
    fn fpu_exc(&mut self, e: u16) -> bool {
        let f = &mut self.cpu.fpu;
        f.sw |= e & (exc::ALL | sw::SF);
        f.update_es();
        e & !f.cw & (exc::IE | exc::DE | exc::ZE) == 0
    }

    fn fpu_store_ok(&self, e: u16) -> bool {
        e & !self.cpu.fpu.cw & (exc::IE | exc::DE | exc::OE | exc::UE) == 0
    }

    fn set_c1(&mut self, on: bool) {
        if on {
            self.cpu.fpu.sw |= sw::C1;
        } else {
            self.cpu.fpu.sw &= !sw::C1;
        }
    }

    fn fpu_underflow(&mut self) -> bool {
        self.set_c1(false);
        self.fpu_exc(exc::IE | sw::SF)
    }

    fn st_opt(&self, i: usize) -> Option<F80> {
        if self.cpu.fpu.is_empty(i) { None } else { Some(self.cpu.fpu.st(i)) }
    }

    fn fpu_push(&mut self, mut v: F80) {
        let t = (self.cpu.fpu.top() + 7) & 7;
        if self.cpu.fpu.tags & (1 << t) != 0 {
            self.set_c1(true);
            if !self.fpu_exc(exc::IE | sw::SF) {
                return;
            }
            v = F80::INDEFINITE;
        }
        let f = &mut self.cpu.fpu;
        f.set_top(t);
        f.regs[t] = v;
        f.tags |= 1 << t;
    }

    fn fpu_pop(&mut self) {
        let f = &mut self.cpu.fpu;
        let t = f.top();
        f.tags &= !(1 << t);
        f.set_top(t + 1);
    }

    fn set_cc(&mut self, c3: bool, c2: bool, c1: bool, c0: bool) {
        let f = &mut self.cpu.fpu;
        f.sw &= !sw::CC;
        f.sw |= ((c3 as u16) * sw::C3) | ((c2 as u16) * sw::C2) | ((c1 as u16) * sw::C1) | ((c0 as u16) * sw::C0);
    }

    pub(super) fn x87(&mut self) -> Res<()> {
        if self.cpu.cr0 & (cr0::EM | cr0::TS) != 0 {
            return Err(Exception::new(vector::NM).into());
        }
        let insn = self.insn;
        let op = (insn.op & 7) as u8;
        let reg = insn.reg();
        let modrm = insn.modrm;
        let mem = insn.mem.is_some();
        if !valid(op, reg, modrm, mem) {
            return ud();
        }
        let (control, nonwaiting) = if mem {
            (matches!((op, reg), (1, 4..=7) | (5, 4 | 6 | 7)), matches!((op, reg), (1, 6 | 7) | (5, 6 | 7)))
        } else {
            let nw = (op == 3 && (0xe0..=0xe4).contains(&modrm)) || (op == 7 && modrm == 0xe0);
            (nw, nw)
        };
        if !nonwaiting {
            self.fpu_check_pending()?;
        }
        if mem { self.x87_mem(op, reg, control) } else { self.x87_reg(op, modrm, control) }
    }

    fn note_fpu_insn(&mut self, data: Option<(usize, u32)>) {
        let sel = self.cpu.segs[CS].selector;
        let f = &mut self.cpu.fpu;
        f.fip = self.start_eip;
        f.fcs = sel;
        f.fop = ((self.insn.op & 7) << 8) | self.insn.modrm as u16;
        if let Some((seg, off)) = data {
            f.fdp = off;
            f.fds = self.cpu.segs[seg].selector;
        }
    }

    fn x87_mem(&mut self, op: u8, reg: usize, control: bool) -> Res<()> {
        let (seg, off) = self.require_mem()?;
        if !control {
            self.note_fpu_insn(Some((seg, off)));
        }
        let rc = self.cpu.fpu.rc();
        match (op, reg) {
            (0 | 2 | 4 | 6, _) => {
                let (v, e) = match op {
                    0 => {
                        let b = self.read_seg(seg, off, 4)?;
                        (F80::from_f32_bits(b), if b & 0x7f80_0000 == 0 && b & 0x7f_ffff != 0 { exc::DE } else { 0 })
                    }
                    4 => {
                        let b = self.read_wide(seg, off, 8)? as u64;
                        let den = b & 0x7ff0_0000_0000_0000 == 0 && b & 0xf_ffff_ffff_ffff != 0;
                        (F80::from_f64_bits(b), if den { exc::DE } else { 0 })
                    }
                    2 => (F80::from_i64(self.read_seg(seg, off, 4)? as i32 as i64), 0),
                    _ => (F80::from_i64(self.read_seg(seg, off, 2)? as i16 as i64), 0),
                };
                let a = self.st_opt(0);
                match reg {
                    2 | 3 => {
                        self.fcom(a, Some(v), false, reg == 3, e);
                    }
                    _ => self.farith(reg, a, Some(v), 0, false, e),
                }
            }
            (1, 0) => {
                let b = self.read_seg(seg, off, 4)?;
                let den = b & 0x7f80_0000 == 0 && b & 0x7f_ffff != 0;
                self.fld_checked(F80::from_f32_bits(b), if den { exc::DE } else { 0 });
            }
            (5, 0) => {
                let b = self.read_wide(seg, off, 8)? as u64;
                let den = b & 0x7ff0_0000_0000_0000 == 0 && b & 0xf_ffff_ffff_ffff != 0;
                self.fld_checked(F80::from_f64_bits(b), if den { exc::DE } else { 0 });
            }
            (3, 5) => {
                let v = f80_from_wide(self.read_wide(seg, off, 10)?);
                self.set_c1(false);
                self.fpu_push(v);
            }
            (3, 0) | (7, 0) | (7, 5) => {
                let v = match (op, reg) {
                    (3, _) => self.read_seg(seg, off, 4)? as i32 as i64,
                    (_, 0) => self.read_seg(seg, off, 2)? as i16 as i64,
                    _ => self.read_wide(seg, off, 8)? as u64 as i64,
                };
                self.set_c1(false);
                self.fpu_push(F80::from_i64(v));
            }
            (7, 4) => {
                let b = self.read_wide(seg, off, 10)?.to_le_bytes();
                let mut v: i64 = 0;
                for i in (0..9).rev() {
                    v = v * 100 + (b[i] >> 4) as i64 * 10 + (b[i] & 0xf) as i64;
                }
                self.set_c1(false);
                self.fpu_push(F80::from_i64(v).with_sign(b[9] & 0x80 != 0));
            }
            (1, 2 | 3) | (5, 2 | 3) | (3, 7) => {
                let n = match op {
                    1 => 4,
                    5 => 8,
                    _ => 10,
                };
                let (bits, e) = match self.st_opt(0) {
                    None => {
                        let indef = match op {
                            1 => 0xffc0_0000,
                            5 => 0xfff8_0000_0000_0000,
                            _ => f80_to_wide(F80::INDEFINITE),
                        };
                        (indef, exc::IE | sw::SF)
                    }
                    Some(v) => match op {
                        1 => {
                            let (b, e) = v.to_f32_bits(rc);
                            (b as u128, e)
                        }
                        5 => {
                            let (b, e) = v.to_f64_bits(rc);
                            (b as u128, e)
                        }
                        _ => (f80_to_wide(v), 0),
                    },
                };
                self.fstore_mem(seg, off, n, bits, e, reg != 2)?;
            }
            (3, 2 | 3) | (7, 2 | 3 | 7) => {
                let bits: u32 = match (op, reg) {
                    (3, _) => 32,
                    (7, 7) => 64,
                    _ => 16,
                };
                let indef = 1u128 << (bits - 1);
                let (v, e) = match self.st_opt(0) {
                    None => (indef, exc::IE | sw::SF),
                    Some(x) => match x.round_int(rc) {
                        Some((s, mag, inexact, up)) if mag <= (1u128 << (bits - 1)) - (!s) as u128 => {
                            self.set_c1(up);
                            let v = if s { mag.wrapping_neg() } else { mag };
                            (v & ((1u128 << bits) - 1), if inexact { exc::PE } else { 0 })
                        }
                        _ => (indef, exc::IE),
                    },
                };
                self.fstore_mem(seg, off, bits / 8, v, e, reg != 2)?;
            }
            (7, 6) => {
                let indef = u128::from_le_bytes(pad16([0, 0, 0, 0, 0, 0, 0, 0xc0, 0xff, 0xff]));
                let (v, e) = match self.st_opt(0) {
                    None => (indef, exc::IE | sw::SF),
                    Some(x) => match x.round_int(rc) {
                        Some((s, mag, inexact, _)) if mag <= 999_999_999_999_999_999 => {
                            let mut b = [0u8; 16];
                            let mut m = mag as u64;
                            for byte in b.iter_mut().take(9) {
                                let lo = (m % 10) as u8;
                                m /= 10;
                                *byte = ((m % 10) as u8) << 4 | lo;
                                m /= 10;
                            }
                            b[9] = if s { 0x80 } else { 0 };
                            (u128::from_le_bytes(b), if inexact { exc::PE } else { 0 })
                        }
                        _ => (indef, exc::IE),
                    },
                };
                self.fstore_mem(seg, off, 10, v, e, true)?;
            }
            (1, 4) => self.fldenv(seg, off, false)?,
            (5, 4) => self.fldenv(seg, off, true)?,
            (1, 5) => {
                let cw = self.read_seg(seg, off, 2)? as u16;
                self.cpu.fpu.cw = cw | 0x40;
                self.cpu.fpu.update_es();
                self.fpu_sync_ferr();
            }
            (1, 6) => {
                self.fstenv(seg, off, false)?;
                self.cpu.fpu.cw |= exc::ALL;
            }
            (5, 6) => {
                self.fstenv(seg, off, true)?;
                self.cpu.fpu.init();
                self.fpu_sync_ferr();
            }
            (1, 7) => self.write_seg(seg, off, 2, self.cpu.fpu.cw as u32)?,
            (5, 7) => self.write_seg(seg, off, 2, self.cpu.fpu.sw as u32)?,
            _ => return ud(),
        }
        Ok(())
    }

    fn fstore_mem(&mut self, seg: usize, off: u32, n: u32, v: u128, e: u16, pop: bool) -> Res<()> {
        if self.fpu_store_ok(e) {
            self.write_wide(seg, off, n, v)?;
            if e & sw::SF != 0 {
                self.set_c1(false);
            }
            self.fpu_exc(e);
            if pop {
                self.fpu_pop();
            }
        } else {
            self.fpu_exc(e);
        }
        Ok(())
    }

    /// FLD of a 32- or 64-bit real: SNaNs raise IE and are quieted.
    fn fld_checked(&mut self, v: F80, mut e: u16) {
        self.set_c1(false);
        let v = if v.is_snan() {
            e |= exc::IE;
            v.quiet()
        } else {
            v
        };
        let t = (self.cpu.fpu.top() + 7) & 7;
        if self.cpu.fpu.tags & (1 << t) == 0 && !self.fpu_exc(e) {
            return;
        }
        self.fpu_push(v);
    }

    fn x87_reg(&mut self, op: u8, modrm: u8, control: bool) -> Res<()> {
        if !control {
            self.note_fpu_insn(None);
        }
        let i = (modrm & 7) as usize;
        let reg = ((modrm >> 3) & 7) as usize;
        let rc = self.cpu.fpu.rc();
        match (op, modrm) {
            (0, _) => {
                let (a, b) = (self.st_opt(0), self.st_opt(i));
                match reg {
                    2 | 3 => {
                        self.fcom(a, b, false, reg == 3, 0);
                    }
                    _ => self.farith(reg, a, b, 0, false, 0),
                }
            }
            (4 | 6, _) => {
                let (a, b) = (self.st_opt(0), self.st_opt(i));
                match reg {
                    2 => {
                        self.fcom(a, b, false, op == 6, 0);
                    }
                    3 if op == 4 => {
                        self.fcom(a, b, false, true, 0);
                    }
                    3 => {
                        if self.fcom(a, b, false, true, 0) {
                            self.fpu_pop();
                        }
                    }
                    _ => self.farith(reg, a, b, i, op == 6, 0),
                }
            }
            (1, 0xc0..=0xc7) => {
                let v = match self.st_opt(i) {
                    Some(v) => v,
                    None if self.fpu_underflow() => F80::INDEFINITE,
                    None => return Ok(()),
                };
                self.set_c1(false);
                self.fpu_push(v);
            }
            (1 | 5 | 7, 0xc8..=0xcf) => {
                let (a, b) = (self.st_opt(0), self.st_opt(i));
                if (a.is_none() || b.is_none()) && !self.fpu_underflow() {
                    return Ok(());
                }
                let a = a.unwrap_or(F80::INDEFINITE);
                let b = b.unwrap_or(F80::INDEFINITE);
                self.cpu.fpu.set_st(0, b);
                self.cpu.fpu.set_st(i, a);
                self.set_c1(false);
            }
            (1, 0xd0) => {}
            (1, 0xd8..=0xdf) | (5, 0xd0..=0xdf) | (7, 0xd0..=0xdf) => {
                let pop = !(op == 5 && modrm < 0xd8);
                let v = match self.st_opt(0) {
                    Some(v) => v,
                    None if self.fpu_underflow() => F80::INDEFINITE,
                    None => return Ok(()),
                };
                self.set_c1(false);
                self.cpu.fpu.set_st(i, v);
                if pop {
                    self.fpu_pop();
                }
            }
            (1, 0xe0 | 0xe1) => {
                let v = match self.st_opt(0) {
                    Some(v) => v,
                    None if self.fpu_underflow() => F80::INDEFINITE,
                    None => return Ok(()),
                };
                let r = if modrm == 0xe0 { v.negated() } else { v.abs() };
                self.set_c1(false);
                self.cpu.fpu.set_st(0, r);
            }
            (1, 0xe4) => {
                self.fcom(self.st_opt(0), Some(F80::ZERO), false, false, 0);
            }
            (1, 0xe5) => {
                let v = self.cpu.fpu.st(0);
                let (c3, c2, c0) = if self.cpu.fpu.is_empty(0) {
                    (true, false, true)
                } else if v.is_unsupported() {
                    (false, false, false)
                } else if v.is_nan() {
                    (false, false, true)
                } else if v.is_inf() {
                    (false, true, true)
                } else if v.is_zero() {
                    (true, false, false)
                } else if v.is_denormal() {
                    (true, true, false)
                } else {
                    (false, true, false)
                };
                self.set_cc(c3, c2, v.sign(), c0);
            }
            (1, 0xe8..=0xee) => {
                let down = rc == rc::DOWN || rc == rc::ZERO;
                // Rounding-direction table from QEMU target/i386/tcg/fpu_helper.c
                // (fldl2t_ST0 and friends), by Fabrice Bellard and the QEMU contributors.
                let v = match modrm {
                    0xe8 => F80::ONE,
                    0xe9 => F80::new(0x4000, 0xd49a_784b_cd1b_8afe + (rc == rc::UP) as u64),
                    0xea => F80::new(0x3fff, 0xb8aa_3b29_5c17_f0bc - down as u64),
                    0xeb => F80::new(0x4000, 0xc90f_daa2_2168_c235 - down as u64),
                    0xec => F80::new(0x3ffd, 0x9a20_9a84_fbcf_f799 - down as u64),
                    0xed => F80::new(0x3ffe, 0xb172_17f7_d1cf_79ac - down as u64),
                    _ => F80::ZERO,
                };
                self.set_c1(false);
                self.fpu_push(v);
            }
            (1, 0xf0..=0xff) => self.ftrans(modrm),
            (2 | 3, 0xc0..=0xdf) => {
                let f = |m| self.cpu.flag(m);
                let c = match reg & 3 {
                    0 => f(flags::CF),
                    1 => f(flags::ZF),
                    2 => f(flags::CF) || f(flags::ZF),
                    _ => f(flags::PF),
                } ^ (op == 3);
                self.set_c1(false);
                match (self.st_opt(0), self.st_opt(i)) {
                    (Some(_), Some(v)) => {
                        if c {
                            self.cpu.fpu.set_st(0, v);
                        }
                    }
                    _ => {
                        if self.fpu_underflow() {
                            self.cpu.fpu.set_st(0, F80::INDEFINITE);
                        }
                    }
                }
            }
            (2, 0xe9) => {
                if self.fcom(self.st_opt(0), self.st_opt(1), true, true, 0) {
                    self.fpu_pop();
                }
            }
            (3, 0xe0 | 0xe1 | 0xe4) => {}
            (3, 0xe2) => {
                self.cpu.fpu.sw &= !(exc::ALL | sw::SF | sw::ES | sw::B);
                self.fpu_sync_ferr();
            }
            (3, 0xe3) => {
                self.cpu.fpu.init();
                self.fpu_sync_ferr();
            }
            (3, 0xe8..=0xf7) | (7, 0xe8..=0xf7) => {
                let quiet = modrm < 0xf0;
                if let Some(r) = self.fcompare(self.st_opt(0), self.st_opt(i), quiet, 0) {
                    let (zf, pf, cf) = match r {
                        GT => (false, false, false),
                        LT => (false, false, true),
                        EQ => (true, false, false),
                        _ => (true, true, true),
                    };
                    let fl = &mut self.cpu.eflags;
                    *fl &= !(flags::ZF | flags::PF | flags::CF | flags::OF | flags::SF | flags::AF);
                    *fl |= ((zf as u32) * flags::ZF) | ((pf as u32) * flags::PF) | ((cf as u32) * flags::CF);
                    if op == 7 {
                        self.fpu_pop();
                    }
                }
            }
            (5, 0xc0..=0xc7) | (7, 0xc0..=0xc7) => {
                let p = self.cpu.fpu.phys(i);
                self.cpu.fpu.tags &= !(1 << p);
                if op == 7 {
                    self.fpu_pop();
                }
            }
            (5, 0xe0..=0xef) => {
                self.fcom(self.st_opt(0), self.st_opt(i), true, modrm >= 0xe8, 0);
            }
            (7, 0xe0) => {
                let v = self.cpu.fpu.sw;
                self.cpu.set_reg16(EAX, v);
            }
            _ => return ud(),
        }
        Ok(())
    }

    /// `reg` is the ModRM operation (add, mul, -, -, sub, subr, div, divr) on
    /// `a` = ST(0) and `b`; the result goes to ST(`dst`).
    fn farith(&mut self, reg: usize, a: Option<F80>, b: Option<F80>, dst: usize, pop: bool, e_in: u16) {
        let (r, e) = match (a, b) {
            (Some(a), Some(b)) => {
                let (x, y) = if reg == 5 || reg == 7 { (b, a) } else { (a, b) };
                let op = match reg {
                    0 => Op::Add,
                    1 => Op::Mul,
                    4 | 5 => Op::Sub,
                    _ => Op::Div,
                };
                let (r, e) = self.x87_arith(op, x, y);
                (r, e | e_in)
            }
            _ => {
                self.set_c1(false);
                (F80::INDEFINITE, exc::IE | sw::SF)
            }
        };
        if !self.fpu_exc(e) {
            return;
        }
        if e & sw::SF == 0 {
            self.set_c1(false);
        }
        self.cpu.fpu.set_st(dst, r);
        if pop {
            self.fpu_pop();
        }
    }

    fn x87_arith(&self, op: Op, a: F80, b: F80) -> (F80, u16) {
        let unary = op == Op::Sqrt;
        if a.is_unsupported() || (!unary && b.is_unsupported()) {
            return (F80::INDEFINITE, exc::IE);
        }
        if a.is_nan() || (!unary && b.is_nan()) {
            return nan_result(a, if unary { a } else { b });
        }
        let mut e = if a.is_denormal() || (!unary && b.is_denormal()) { exc::DE } else { 0 };
        let rc = self.cpu.fpu.rc();
        let (fa, _) = a.to_f64(rc::NEAREST);
        let (fb, _) = b.to_f64(rc::NEAREST);
        let (r, x) = fparith::arith(op, fa, fb, rc);
        e |= x;
        if x & exc::IE != 0 {
            return (F80::INDEFINITE, e);
        }
        (self.apply_precision(r, &mut e), e)
    }

    fn apply_precision(&self, r: f64, e: &mut u16) -> F80 {
        let v = F80::from_f64(r);
        if self.cpu.fpu.pc() != 0 || r.is_nan() {
            return v;
        }
        let (b, x) = v.to_f32_bits(self.cpu.fpu.rc());
        *e |= x & !exc::DE;
        F80::from_f32_bits(b)
    }

    /// FCOM-family compare that sets C3, C2, C0. Returns whether the
    /// instruction completed (so a pop form may pop).
    fn fcom(&mut self, a: Option<F80>, b: Option<F80>, quiet: bool, pop: bool, e_in: u16) -> bool {
        let Some(r) = self.fcompare(a, b, quiet, e_in) else { return false };
        let (c3, c2, c0) = match r {
            GT => (false, false, false),
            LT => (false, false, true),
            EQ => (true, false, false),
            _ => (true, true, true),
        };
        self.set_cc(c3, c2, false, c0);
        if pop {
            self.fpu_pop();
        }
        true
    }

    fn fcompare(&mut self, a: Option<F80>, b: Option<F80>, quiet: bool, e_in: u16) -> Option<u8> {
        let (r, e) = match (a, b) {
            (Some(a), Some(b)) => {
                if a.is_unsupported() || b.is_unsupported() {
                    (UNORDERED, exc::IE)
                } else if a.is_nan() || b.is_nan() {
                    (UNORDERED, if !quiet || a.is_snan() || b.is_snan() { exc::IE } else { 0 })
                } else {
                    let de = if a.is_denormal() || b.is_denormal() { exc::DE } else { 0 };
                    let r = match a.cmp_num(b) {
                        Ordering::Less => LT,
                        Ordering::Equal => EQ,
                        Ordering::Greater => GT,
                    };
                    (r, de | e_in)
                }
            }
            _ => {
                self.set_c1(false);
                (UNORDERED, exc::IE | sw::SF)
            }
        };
        self.fpu_exc(e).then_some(r)
    }

    fn ftrans(&mut self, modrm: u8) {
        match modrm {
            0xf6 | 0xf7 => {
                let t = self.cpu.fpu.top();
                self.cpu.fpu.set_top(if modrm == 0xf6 { t + 7 } else { t + 1 });
                self.set_c1(false);
                return;
            }
            0xf1 | 0xf3 | 0xf5 | 0xf8 | 0xf9 | 0xfd => return self.ftrans2(modrm),
            _ => {}
        }
        let Some(x) = self.st_opt(0) else {
            if self.fpu_underflow() {
                self.cpu.fpu.set_st(0, F80::INDEFINITE);
                if matches!(modrm, 0xf2 | 0xf4 | 0xfb) {
                    self.fpu_push(F80::INDEFINITE);
                }
            }
            return;
        };
        self.set_c1(false);
        let rc = self.cpu.fpu.rc();
        if matches!(modrm, 0xf2 | 0xfb | 0xfe | 0xff) {
            self.cpu.fpu.sw &= !sw::C2;
            if !x.is_nan() && !x.is_unsupported() && !x.is_inf() && x.exp() >= 0x3fff + 63 {
                self.cpu.fpu.sw |= sw::C2;
                return;
            }
        }
        match modrm {
            0xf4 => {
                let (expo, sig, e) = if x.is_unsupported() {
                    (F80::INDEFINITE, F80::INDEFINITE, exc::IE)
                } else if x.is_nan() {
                    let (n, e) = nan_result(x, x);
                    (n, n, e)
                } else if x.is_zero() {
                    (F80::inf(true), x, exc::ZE)
                } else if x.is_inf() {
                    (F80::inf(false), x, 0)
                } else {
                    let (s, ex, m) = x.unpack();
                    let de = if x.is_denormal() { exc::DE } else { 0 };
                    (F80::from_i64(ex as i64), F80 { mant: m, se: 0x3fff | (s as u16) << 15 }, de)
                };
                if self.fpu_exc(e) {
                    self.cpu.fpu.set_st(0, expo);
                    self.fpu_push(sig);
                }
            }
            0xfa => {
                let (r, e) = self.x87_arith(Op::Sqrt, x, x);
                if self.fpu_exc(e) {
                    self.cpu.fpu.set_st(0, r);
                }
            }
            0xfc => {
                let (r, e) = if x.is_unsupported() {
                    (F80::INDEFINITE, exc::IE)
                } else if x.is_nan() {
                    nan_result(x, x)
                } else {
                    let (r, inexact) = x.round_to_integral(rc);
                    (r, if inexact { exc::PE } else { 0 } | if x.is_denormal() { exc::DE } else { 0 })
                };
                if self.fpu_exc(e) {
                    self.cpu.fpu.set_st(0, r);
                }
            }
            _ => {
                let f = |v: f64| -> (f64, f64) {
                    let op = match modrm {
                        0xf2 => Some(trig::Op::Tan),
                        0xfb => Some(trig::Op::SinCos),
                        0xfe => Some(trig::Op::Sin),
                        0xff => Some(trig::Op::Cos),
                        _ => None,
                    };
                    if let Some(r) = op.filter(|_| trig::fast(v)).and_then(|op| trig::eval(v, op)) {
                        return r;
                    }
                    match modrm {
                        0xf0 => ((v * std::f64::consts::LN_2).exp_m1(), 0.0),
                        0xf2 => (v.tan(), 1.0),
                        0xfb => (v.sin(), v.cos()),
                        0xfe => (v.sin(), 0.0),
                        _ => (v.cos(), 0.0),
                    }
                };
                let (r, second, e) = libm1(x, f);
                if !self.fpu_exc(e) {
                    return;
                }
                self.cpu.fpu.set_st(0, r);
                match modrm {
                    0xf2 => self.fpu_push(if r.is_nan() { r } else { F80::ONE }),
                    0xfb => self.fpu_push(second),
                    _ => {}
                }
            }
        }
    }

    fn ftrans2(&mut self, modrm: u8) {
        let (Some(a), Some(b)) = (self.st_opt(0), self.st_opt(1)) else {
            if self.fpu_underflow() {
                if matches!(modrm, 0xf1 | 0xf3 | 0xf9) {
                    self.cpu.fpu.set_st(1, F80::INDEFINITE);
                    self.fpu_pop();
                } else {
                    self.cpu.fpu.set_st(0, F80::INDEFINITE);
                }
            }
            return;
        };
        self.set_c1(false);
        match modrm {
            0xf5 | 0xf8 => self.fprem(a, b, modrm == 0xf5),
            0xfd => {
                let (r, e) = fscale(a, b);
                if self.fpu_exc(e) {
                    self.cpu.fpu.set_st(0, r);
                }
            }
            _ => {
                let (r, e) = if a.is_unsupported() || b.is_unsupported() {
                    (F80::INDEFINITE, exc::IE)
                } else if a.is_nan() || b.is_nan() {
                    nan_result(a, b)
                } else {
                    let (x, _) = a.to_f64(rc::NEAREST);
                    let (y, _) = b.to_f64(rc::NEAREST);
                    let mut e = if a.is_denormal() || b.is_denormal() { exc::DE } else { 0 };
                    let r = match modrm {
                        0xf1 => {
                            if x == 0.0 && y.is_finite() && y != 0.0 {
                                e |= exc::ZE;
                            }
                            y * x.log2()
                        }
                        0xf9 => y * x.ln_1p() * std::f64::consts::LOG2_E,
                        _ => y.atan2(x),
                    };
                    if r.is_nan() {
                        (F80::INDEFINITE, e | exc::IE)
                    } else {
                        (F80::from_f64(r), e | exc::PE)
                    }
                };
                if self.fpu_exc(e) {
                    self.cpu.fpu.set_st(1, r);
                    self.fpu_pop();
                }
            }
        }
    }

    /// FPREM and FPREM1, exact on the 64-bit significands. A partial
    /// remainder reduces the exponent difference by 63.
    fn fprem(&mut self, a: F80, b: F80, ieee: bool) {
        let mut q: u128 = 0;
        let mut partial = false;
        let (r, e) = if a.is_unsupported() || b.is_unsupported() {
            (F80::INDEFINITE, exc::IE)
        } else if a.is_nan() || b.is_nan() {
            nan_result(a, b)
        } else if a.is_inf() || b.is_zero() {
            (F80::INDEFINITE, exc::IE)
        } else if b.is_inf() || a.is_zero() {
            (a, if a.is_denormal() || b.is_denormal() { exc::DE } else { 0 })
        } else {
            let de = if a.is_denormal() || b.is_denormal() { exc::DE } else { 0 };
            let (sa, ea, ma) = a.unpack();
            let (_, eb, mb) = b.unpack();
            let d = ea - eb;
            let (ma, mb) = (ma as u128, mb as u128);
            let r = if d >= 64 {
                partial = true;
                F80::pack(sa, ea - 63, ((ma << 63) % mb) as u64)
            } else if d >= 0 {
                let n = ma << d;
                q = n / mb;
                let mut r = n % mb;
                let mut s = sa;
                if ieee && (2 * r > mb || (2 * r == mb && q & 1 != 0)) {
                    q += 1;
                    r = mb - r;
                    s = !s;
                }
                F80::pack(s, eb, r as u64)
            } else if ieee && d == -1 && ma > mb {
                q = 1;
                F80::pack(!sa, eb - 1, (2 * mb - ma) as u64)
            } else {
                a
            };
            (r, de)
        };
        if !self.fpu_exc(e) {
            return;
        }
        self.cpu.fpu.set_st(0, r);
        if partial {
            self.cpu.fpu.sw |= sw::C2;
        } else {
            self.set_cc(q & 2 != 0, false, q & 1 != 0, q & 4 != 0);
        }
    }

    fn fstenv(&mut self, seg: usize, off: u32, with_regs: bool) -> Res<()> {
        let f = &self.cpu.fpu;
        let (cw, sw, tw) = (f.cw as u32, f.sw as u32, f.full_tag_word() as u32);
        let (fip, fdp, fop) = (f.fip, f.fdp, f.fop as u32);
        let (fcs, fds) = (f.fcs as u32, f.fds as u32);
        let hi = 0xffff_0000;
        // Layouts follow Bochs cpu/fpu/fpu.cc (fpu_save_environment), by Stanislav Shwartsman.
        let (words, wsz): ([u32; 7], u32) = match (self.pm(), self.insn.op32) {
            (true, true) => ([hi | cw, hi | sw, hi | tw, fip, fcs | fop << 16, fdp, hi | fds], 4),
            (true, false) => ([cw, sw, tw, fip & 0xffff, fcs, fdp & 0xffff, fds], 2),
            (false, op32) => {
                let ip = (fcs << 4).wrapping_add(fip);
                let dp = (fds << 4).wrapping_add(fdp);
                if op32 {
                    let w = [hi | cw, hi | sw, hi | tw, hi | (ip & 0xffff), (ip & 0xffff_0000) >> 4 | fop];
                    ([w[0], w[1], w[2], w[3], w[4], hi | (dp & 0xffff), (dp & 0xffff_0000) >> 4], 4)
                } else {
                    ([cw, sw, tw, ip & 0xffff, (ip & 0xf_0000) >> 4 | fop, dp & 0xffff, (dp & 0xf_0000) >> 4], 2)
                }
            }
        };
        let total = 7 * wsz + if with_regs { 80 } else { 0 };
        self.probe_span(seg, off, total)?;
        for (k, w) in words.iter().enumerate() {
            self.write_seg(seg, self.off_add(off, k as u32 * wsz), wsz, *w)?;
        }
        if with_regs {
            for i in 0..8 {
                let v = self.cpu.fpu.st(i);
                let at = self.off_add(off, 7 * wsz + 10 * i as u32);
                self.write_wide(seg, at, 10, f80_to_wide(v))?;
            }
        }
        Ok(())
    }

    fn fldenv(&mut self, seg: usize, off: u32, with_regs: bool) -> Res<()> {
        let wsz = if self.insn.op32 { 4 } else { 2 };
        let mut w = [0u32; 7];
        for (k, slot) in w.iter_mut().enumerate() {
            *slot = self.read_seg(seg, self.off_add(off, k as u32 * wsz), wsz)?;
        }
        let mut regs = [F80::ZERO; 8];
        if with_regs {
            for (i, r) in regs.iter_mut().enumerate() {
                let at = self.off_add(off, 7 * wsz + 10 * i as u32);
                *r = f80_from_wide(self.read_wide(seg, at, 10)?);
            }
        }
        let pm = self.pm();
        let f = &mut self.cpu.fpu;
        f.cw = w[0] as u16 | 0x40;
        f.sw = w[1] as u16;
        f.set_full_tag_word(w[2] as u16);
        if pm {
            let lo16 = |v: u32| if wsz == 4 { v } else { v & 0xffff };
            f.fip = lo16(w[3]);
            f.fcs = w[4] as u16;
            f.fop = if wsz == 4 { ((w[4] >> 16) & 0x7ff) as u16 } else { 0 };
            f.fdp = lo16(w[5]);
            f.fds = w[6] as u16;
        } else {
            let hi_mask = if wsz == 4 { 0x0fff_f000 } else { 0xf000 };
            f.fip = (w[3] & 0xffff) | (w[4] & hi_mask) << 4;
            f.fdp = (w[5] & 0xffff) | (w[6] & hi_mask) << 4;
            f.fop = (w[4] & 0x7ff) as u16;
            f.fcs = 0;
            f.fds = 0;
        }
        if with_regs {
            for (i, r) in regs.into_iter().enumerate() {
                let p = f.phys(i);
                f.regs[p] = r;
            }
        }
        f.update_es();
        self.fpu_sync_ferr();
        Ok(())
    }
}

pub(super) fn pad16(b: [u8; 10]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..10].copy_from_slice(&b);
    out
}

pub(super) fn f80_to_wide(v: F80) -> u128 {
    u128::from_le_bytes(pad16(v.to_bytes()))
}

pub(super) fn f80_from_wide(v: u128) -> F80 {
    let b = v.to_le_bytes();
    F80::from_bytes(b[..10].try_into().unwrap())
}

fn valid(op: u8, reg: usize, modrm: u8, mem: bool) -> bool {
    if mem {
        return !matches!((op, reg), (1, 1) | (3, 1 | 4 | 6) | (5, 1 | 5) | (7, 1));
    }
    match op {
        0 | 4 => true,
        1 => !matches!(modrm, 0xd1..=0xd7 | 0xe2 | 0xe3 | 0xe6 | 0xe7 | 0xef),
        2 => modrm < 0xe0 || modrm == 0xe9,
        3 => !matches!(modrm, 0xe5..=0xe7 | 0xf8..=0xff),
        5 => modrm < 0xf0,
        6 => !matches!(modrm, 0xd8 | 0xda..=0xdf),
        _ => modrm < 0xe1 || (0xe8..=0xf7).contains(&modrm),
    }
}

/// x87 NaN propagation: the NaN with the larger significand wins, quieted.
fn nan_result(a: F80, b: F80) -> (F80, u16) {
    let e = if a.is_snan() || b.is_snan() { exc::IE } else { 0 };
    let r = match (a.is_nan(), b.is_nan()) {
        (true, true) => {
            if (b.mant | 1 << 62) > (a.mant | 1 << 62) { b } else { a }
        }
        (true, false) => a,
        _ => b,
    };
    (r.quiet(), e)
}

/// One-operand transcendental through `f64`; `f` returns the result and a
/// second value for FSINCOS.
fn libm1(x: F80, f: impl Fn(f64) -> (f64, f64)) -> (F80, F80, u16) {
    if x.is_unsupported() || x.is_inf() {
        return (F80::INDEFINITE, F80::INDEFINITE, exc::IE);
    }
    if x.is_nan() {
        let (n, e) = nan_result(x, x);
        return (n, n, e);
    }
    let de = if x.is_denormal() { exc::DE } else { 0 };
    if x.is_zero() {
        let (r, s) = f(0.0);
        let r = if r == 0.0 { x } else { F80::from_f64(r) };
        return (r, F80::from_f64(s), de);
    }
    let (v, _) = x.to_f64(rc::NEAREST);
    let (r, s) = f(v);
    if r.is_nan() {
        return (F80::INDEFINITE, F80::INDEFINITE, de | exc::IE);
    }
    (F80::from_f64(r), F80::from_f64(s), de | exc::PE)
}

fn fscale(a: F80, b: F80) -> (F80, u16) {
    if a.is_unsupported() || b.is_unsupported() {
        return (F80::INDEFINITE, exc::IE);
    }
    if a.is_nan() || b.is_nan() {
        return nan_result(a, b);
    }
    let de = if a.is_denormal() || b.is_denormal() { exc::DE } else { 0 };
    if b.is_inf() {
        return match (b.sign(), a.is_zero(), a.is_inf()) {
            (false, true, _) | (true, _, true) => (F80::INDEFINITE, exc::IE),
            (false, false, _) => (F80::inf(a.sign()), de),
            (true, _, false) => (F80::ZERO.with_sign(a.sign()), de),
        };
    }
    if a.is_zero() || a.is_inf() {
        return (a, de);
    }
    let (s, mag, _, _) = b.round_int(rc::ZERO).unwrap();
    let n = mag.min(1 << 20) as i32;
    let n = if s { -n } else { n };
    let (sa, ea, ma) = a.unpack();
    let ne = ea + n;
    let biased = ne + 16383;
    if biased >= 0x7fff {
        return (F80::inf(sa), de | exc::OE | exc::PE);
    }
    let lost = biased <= 0 && (biased <= -63 || ma & ((1u64 << (1 - biased)) - 1) != 0);
    (F80::pack(sa, ne, ma), de | if lost { exc::UE | exc::PE } else { 0 })
}

//! MMX, SSE and SSE2 instructions, plus the `0F AE` group (FXSAVE, FXRSTOR,
//! LDMXCSR, STMXCSR, fences, CLFLUSH).
//!
//! Approximations: RCPPS/RSQRTPS return exactly rounded results instead of
//! the hardware's 12-bit approximations, and when an unmasked SIMD exception
//! fires, the flags of every lane are reported, not only the
//! pre-computation ones.

use super::mem::Access;
use super::{Ctx, Exception, Res, gp, ud, vector};
use crate::bus::Bus;
use crate::cpu::{DS, EDI, cr0, cr4, flags, mxcsr};
use crate::decode::Rep;
use crate::fparith::{self, Float, Op};
use crate::fpu::{F80, exc, rc};

const NP: u8 = 0;
const P66: u8 = 1;
const F3: u8 = 2;
const F2: u8 = 3;

/// MXCSR state for one instruction: control bits in, exception flags out.
struct Mx {
    csr: u32,
    flags: u16,
}

impl Mx {
    fn rc(&self) -> u8 {
        ((self.csr >> mxcsr::RC_SHIFT) & 3) as u8
    }

    fn daz<F: Float>(&self, x: F) -> F {
        if self.csr & mxcsr::DAZ != 0 && x.subnormal() { F::signed_zero(x.negative()) } else { x }
    }

    fn input<F: Float>(&mut self, x: F) -> F {
        let x = self.daz(x);
        if x.subnormal() {
            self.flags |= exc::DE;
        }
        x
    }

    fn ftz<F: Float>(&mut self, r: F) -> F {
        let um = 1 << (mxcsr::MASK_SHIFT + 4);
        if r.subnormal() && self.csr & mxcsr::FZ != 0 && self.csr & um != 0 {
            self.flags |= exc::UE | exc::PE;
            return F::signed_zero(r.negative());
        }
        r
    }

    fn arith<F: Float>(&mut self, op: Op, a: F, b: F) -> F {
        let a = self.input(a);
        let b = if op == Op::Sqrt { a } else { self.input(b) };
        if a.nan() || b.nan() {
            if a.is_snan() || b.is_snan() {
                self.flags |= exc::IE;
            }
            return if a.nan() { a.quieted() } else { b.quieted() };
        }
        let (r, e) = fparith::arith(op, a, b, self.rc());
        self.flags |= e;
        self.ftz(r)
    }

    /// MINPS/MAXPS: the second operand wins on NaNs and on equal values.
    fn minmax<F: Float>(&mut self, a: F, b: F, max: bool) -> F {
        let a = self.input(a);
        let b = self.input(b);
        if a.nan() || b.nan() {
            self.flags |= exc::IE;
            return b;
        }
        let take_a = if max { a > b } else { a < b };
        if take_a { a } else { b }
    }

    fn cmp<F: Float>(&mut self, a: F, b: F, pred: u8) -> bool {
        let a = self.input(a);
        let b = self.input(b);
        let unord = a.nan() || b.nan();
        let signaling = matches!(pred, 1 | 2 | 5 | 6);
        if (signaling && unord) || a.is_snan() || b.is_snan() {
            self.flags |= exc::IE;
        }
        match pred {
            0 => !unord && a == b,
            1 => !unord && a < b,
            2 => !unord && a <= b,
            3 => unord,
            4 => unord || a != b,
            5 => unord || a >= b,
            6 => unord || a > b,
            _ => !unord,
        }
    }

    fn cvt_int<F: Float>(&mut self, x: F, truncate: bool) -> u64 {
        let rc = if truncate { rc::ZERO } else { self.rc() };
        let (v, e) = fparith::to_i32(self.daz(x), rc);
        self.flags |= e;
        v as u32 as u64
    }

    fn int_to_f32(&mut self, v: i32) -> u64 {
        let (b, e) = F80::from_i64(v as i64).to_f32_bits(self.rc());
        self.flags |= e;
        b as u64
    }

    fn widen(&mut self, x: f32) -> u64 {
        let x = self.input(x);
        let (b, e) = F80::from_f32_bits(x.to_bits()).to_f64_bits(rc::NEAREST);
        self.flags |= e;
        b
    }

    fn narrow(&mut self, x: f64) -> u64 {
        let x = self.input(x);
        let (b, e) = F80::from_f64(x).to_f32_bits(self.rc());
        self.flags |= e & !exc::DE;
        if x.nan() {
            return b as u64;
        }
        self.ftz(f32::from_bits(b)).to_bits() as u64
    }
}

fn lane_mask(bits: u32) -> u64 {
    if bits == 64 { !0 } else { (1 << bits) - 1 }
}

fn lane(v: u128, i: u32, bits: u32) -> u64 {
    (v >> (i * bits)) as u64 & lane_mask(bits)
}

fn set_lane(v: &mut u128, i: u32, bits: u32, x: u64) {
    let m = (lane_mask(bits) as u128) << (i * bits);
    *v = (*v & !m) | ((x & lane_mask(bits)) as u128) << (i * bits);
}

fn sx(x: u64, bits: u32) -> i64 {
    ((x << (64 - bits)) as i64) >> (64 - bits)
}

fn sat_s(v: i64, bits: u32) -> u64 {
    let lim = 1i64 << (bits - 1);
    v.clamp(-lim, lim - 1) as u64
}

fn sat_u(v: i64, bits: u32) -> u64 {
    v.clamp(0, (1i64 << bits) - 1) as u64
}

fn map2(a: u128, b: u128, bits: u32, width: u32, f: impl Fn(u64, u64) -> u64) -> u128 {
    let mut r = 0;
    for i in 0..width / bits {
        set_lane(&mut r, i, bits, f(lane(a, i, bits), lane(b, i, bits)));
    }
    r
}

fn unpack(a: u128, b: u128, bits: u32, width: u32, high: bool) -> u128 {
    let half = width / bits / 2;
    let base = if high { half } else { 0 };
    let mut r = 0;
    for i in 0..half {
        set_lane(&mut r, 2 * i, bits, lane(a, base + i, bits));
        set_lane(&mut r, 2 * i + 1, bits, lane(b, base + i, bits));
    }
    r
}

fn pack(a: u128, b: u128, bits: u32, width: u32, unsigned: bool) -> u128 {
    let n = width / bits;
    let to = bits / 2;
    let sat = |x: u64| if unsigned { sat_u(sx(x, bits), to) } else { sat_s(sx(x, bits), to) };
    let mut r = 0;
    for i in 0..n {
        set_lane(&mut r, i, to, sat(lane(a, i, bits)));
        set_lane(&mut r, n + i, to, sat(lane(b, i, bits)));
    }
    r
}

/// Shifts each lane: `kind` 0 is logical right, 1 arithmetic right, 2 left.
fn shift(a: u128, bits: u32, width: u32, count: u64, kind: u8) -> u128 {
    map2(a, 0, bits, width, |x, _| {
        if count >= bits as u64 {
            if kind == 1 { (sx(x, bits) >> 63) as u64 } else { 0 }
        } else {
            match kind {
                0 => x >> count,
                1 => (sx(x, bits) >> count) as u64,
                _ => x << count,
            }
        }
    })
}

fn shuffle(src: u128, bits: u32, first: u32, count: u32, imm: u8) -> u128 {
    let mut r = src;
    for i in 0..count {
        let sel = ((imm >> (2 * i)) & 3) as u32;
        set_lane(&mut r, first + i, bits, lane(src, first + sel, bits));
    }
    r
}

/// Two-operand integer ops shared by the MMX (`width` 64) and SSE2 (128) forms.
fn int_op(op: u8, a: u128, b: u128, w: u32) -> Option<u128> {
    let m = |bits, f: &dyn Fn(u64, u64) -> u64| map2(a, b, bits, w, f);
    let count = b as u64;
    Some(match op {
        0x60..=0x62 => unpack(a, b, 8 << (op - 0x60), w, false),
        0x68..=0x6a => unpack(a, b, 8 << (op - 0x68), w, true),
        0x6c if w == 128 => unpack(a, b, 64, w, false),
        0x6d if w == 128 => unpack(a, b, 64, w, true),
        0x63 => pack(a, b, 16, w, false),
        0x6b => pack(a, b, 32, w, false),
        0x67 => pack(a, b, 16, w, true),
        0x64..=0x66 => {
            let bits = 8 << (op - 0x64);
            m(bits, &|x, y| if sx(x, bits) > sx(y, bits) { !0 } else { 0 })
        }
        0x74..=0x76 => m(8 << (op - 0x74), &|x, y| if x == y { !0 } else { 0 }),
        0xd1..=0xd3 => shift(a, 16 << (op - 0xd1), w, count, 0),
        0xe1 | 0xe2 => shift(a, 16 << (op - 0xe1), w, count, 1),
        0xf1..=0xf3 => shift(a, 16 << (op - 0xf1), w, count, 2),
        0xd4 => m(64, &|x, y| x.wrapping_add(y)),
        0xfb => m(64, &|x, y| x.wrapping_sub(y)),
        0xfc..=0xfe => m(8 << (op - 0xfc), &|x, y| x.wrapping_add(y)),
        0xf8..=0xfa => m(8 << (op - 0xf8), &|x, y| x.wrapping_sub(y)),
        0xec | 0xed => {
            let bits = 8 << (op - 0xec);
            m(bits, &|x, y| sat_s(sx(x, bits) + sx(y, bits), bits))
        }
        0xe8 | 0xe9 => {
            let bits = 8 << (op - 0xe8);
            m(bits, &|x, y| sat_s(sx(x, bits) - sx(y, bits), bits))
        }
        0xdc | 0xdd => {
            let bits = 8 << (op - 0xdc);
            m(bits, &|x, y| sat_u(x as i64 + y as i64, bits))
        }
        0xd8 | 0xd9 => {
            let bits = 8 << (op - 0xd8);
            m(bits, &|x, y| sat_u(x as i64 - y as i64, bits))
        }
        0xd5 => m(16, &|x, y| x.wrapping_mul(y)),
        0xe5 => m(16, &|x, y| ((sx(x, 16) * sx(y, 16)) >> 16) as u64),
        0xe4 => m(16, &|x, y| (x * y) >> 16),
        0xf4 => m(64, &|x, y| (x & 0xffff_ffff) * (y & 0xffff_ffff)),
        0xf5 => m(32, &|x, y| {
            let p = |s: u32| sx((x >> s) & 0xffff, 16) * sx((y >> s) & 0xffff, 16);
            (p(0) + p(16)) as u64
        }),
        0xda => m(8, &|x, y| x.min(y)),
        0xde => m(8, &|x, y| x.max(y)),
        0xea => m(16, &|x, y| if sx(x, 16) < sx(y, 16) { x } else { y }),
        0xee => m(16, &|x, y| if sx(x, 16) > sx(y, 16) { x } else { y }),
        0xdb => a & b,
        0xdf => !a & b & (u128::MAX >> (128 - w)),
        0xeb => a | b,
        0xef => a ^ b,
        0xe0 => m(8, &|x, y| (x + y + 1) >> 1),
        0xe3 => m(16, &|x, y| (x + y + 1) >> 1),
        0xf6 => m(64, &|x, y| (0..8).map(|i| ((x >> (8 * i)) as u8).abs_diff((y >> (8 * i)) as u8) as u64).sum()),
        _ => return None,
    })
}

macro_rules! fp_lanes {
    ($double:expr, $scalar:expr, $a:expr, $b:expr, |$x:ident, $y:ident| $body:expr) => {
        if $double {
            fp_map::<f64>($a, $b, $scalar, |$x, $y| $body)
        } else {
            fp_map::<f32>($a, $b, $scalar, |$x, $y| $body)
        }
    };
}

/// Applies `f` to the float lanes of `a` and `b` (only lane 0 when `scalar`;
/// the other lanes keep `a`). `f` returns raw lane bits.
fn fp_map<F: Float>(a: u128, b: u128, scalar: bool, mut f: impl FnMut(F, F) -> u64) -> u128 {
    let bits = F::BITS;
    let n = if scalar { 1 } else { 128 / bits };
    let mut r = a;
    for i in 0..n {
        let x = F::from_bits64(lane(a, i, bits));
        let y = F::from_bits64(lane(b, i, bits));
        set_lane(&mut r, i, bits, f(x, y));
    }
    r
}

fn recip<F: Float>(x: F, sqrt: bool) -> F {
    if x.nan() {
        return x.quieted();
    }
    let x = if x.subnormal() { F::signed_zero(x.negative()) } else { x };
    if sqrt && x.negative() && x != F::ZERO {
        return F::indefinite();
    }
    let r = if sqrt { F::ONE / x.root() } else { F::ONE / x };
    if r.subnormal() { F::signed_zero(r.negative()) } else { r }
}

impl<B: Bus> Ctx<'_, B> {
    fn mand_prefix(&self) -> u8 {
        match self.insn.rep {
            Rep::Rep => F3,
            Rep::Repne => F2,
            Rep::None if self.insn.opsize_prefix => P66,
            Rep::None => NP,
        }
    }

    fn mmx_enter(&mut self) -> Res<()> {
        if self.cpu.cr0 & cr0::EM != 0 {
            return ud();
        }
        if self.cpu.cr0 & cr0::TS != 0 {
            return Err(Exception::new(vector::NM).into());
        }
        self.fpu_check_pending()?;
        self.cpu.fpu.enter_mmx();
        Ok(())
    }

    fn sse_enter(&mut self) -> Res<()> {
        if self.cpu.cr0 & cr0::EM != 0 || self.cpu.cr4 & cr4::OSFXSR == 0 {
            return ud();
        }
        if self.cpu.cr0 & cr0::TS != 0 {
            return Err(Exception::new(vector::NM).into());
        }
        Ok(())
    }

    fn mmx_transition(&mut self) -> Res<()> {
        self.fpu_check_pending()?;
        self.cpu.fpu.enter_mmx();
        Ok(())
    }

    fn mem_operand(&self) -> Option<(usize, u32)> {
        self.insn.mem.map(|m| (self.insn.seg(&m), self.ea(&m)))
    }

    fn check_align16(&self, seg: usize, off: u32) -> Res<()> {
        if self.cpu.segs[seg].base.wrapping_add(off) & 15 != 0 { gp(0) } else { Ok(()) }
    }

    /// The `rm` operand as an XMM value: `n` bytes from memory, or the whole
    /// register.
    fn xmm_rm(&mut self, n: u32, aligned: bool) -> Res<u128> {
        match self.mem_operand() {
            Some((seg, off)) => {
                if aligned {
                    self.check_align16(seg, off)?;
                }
                self.read_wide(seg, off, n)
            }
            None => Ok(self.cpu.xmm[self.insn.rm()]),
        }
    }

    fn xmm_store(&mut self, n: u32, aligned: bool, v: u128) -> Res<()> {
        match self.mem_operand() {
            Some((seg, off)) => {
                if aligned {
                    self.check_align16(seg, off)?;
                }
                self.write_wide(seg, off, n, v)
            }
            None => {
                let r = self.insn.rm();
                let keep = if n == 16 { 0 } else { u128::MAX << (8 * n) };
                self.cpu.xmm[r] = (self.cpu.xmm[r] & keep) | (v & !keep);
                Ok(())
            }
        }
    }

    fn mm_rm(&mut self) -> Res<u64> {
        match self.mem_operand() {
            Some((seg, off)) => Ok(self.read_wide(seg, off, 8)? as u64),
            None => Ok(self.cpu.fpu.mm(self.insn.rm())),
        }
    }

    fn sse_commit(&mut self, m: &Mx) -> Res<()> {
        let f = (m.flags & exc::ALL) as u32;
        self.cpu.mxcsr |= f;
        if f & !(self.cpu.mxcsr >> mxcsr::MASK_SHIFT) & 0x3f != 0 {
            if self.cpu.cr4 & cr4::OSXMMEXCPT != 0 {
                return Err(Exception::new(vector::XM).into());
            }
            return ud();
        }
        Ok(())
    }

    fn mx(&self) -> Mx {
        Mx { csr: self.cpu.mxcsr, flags: 0 }
    }

    pub(super) fn simd(&mut self) -> Res<()> {
        let op = self.insn.op as u8;
        let pfx = self.mand_prefix();
        match op {
            0x10..=0x17 | 0x28..=0x2f | 0x50..=0x5f | 0xc2 | 0xc6 | 0xe6 => self.sse_fp(op, pfx),
            0xc3 => {
                let (seg, off) = self.mem_operand().ok_or(()).or_else(|_| ud())?;
                if pfx != NP {
                    return ud();
                }
                self.write_seg(seg, off, 4, self.cpu.regs[self.insn.reg()])
            }
            _ => self.simd_int(op, pfx),
        }
    }

    fn sse_fp(&mut self, op: u8, pfx: u8) -> Res<()> {
        let reg = self.insn.reg();
        let mem = self.insn.mem.is_some();
        let scalar = pfx >= F3;
        let double = pfx == P66 || pfx == F2;
        self.sse_enter()?;
        match (op, pfx) {
            (0x10, NP | P66) => self.cpu.xmm[reg] = self.xmm_rm(16, false)?,
            (0x10, _) => {
                let n = if pfx == F3 { 4 } else { 8 };
                let v = self.xmm_rm(n, false)?;
                let low = u128::MAX >> (128 - 8 * n);
                self.cpu.xmm[reg] = if mem { v & low } else { (self.cpu.xmm[reg] & !low) | (v & low) };
            }
            (0x11, _) => {
                let n = match pfx {
                    F3 => 4,
                    F2 => 8,
                    _ => 16,
                };
                self.xmm_store(n, false, self.cpu.xmm[reg])?;
            }
            (0x12 | 0x16, NP | P66) => {
                if pfx == P66 && !mem {
                    return ud();
                }
                let src = self.xmm_rm(8, false)?;
                let v = if mem { src as u64 } else if op == 0x12 { (src >> 64) as u64 } else { src as u64 };
                let mut d = self.cpu.xmm[reg];
                set_lane(&mut d, (op == 0x16) as u32, 64, v);
                self.cpu.xmm[reg] = d;
            }
            (0x13 | 0x17, NP | P66) => {
                let (seg, off) = self.mem_operand().ok_or(()).or_else(|_| ud())?;
                let v = lane(self.cpu.xmm[reg], (op == 0x17) as u32, 64);
                self.write_wide(seg, off, 8, v as u128)?;
            }
            (0x14 | 0x15, NP | P66) => {
                let b = self.xmm_rm(16, true)?;
                self.cpu.xmm[reg] = unpack(self.cpu.xmm[reg], b, if double { 64 } else { 32 }, 128, op == 0x15);
            }
            (0x28, NP | P66) => self.cpu.xmm[reg] = self.xmm_rm(16, true)?,
            (0x29, NP | P66) => self.xmm_store(16, true, self.cpu.xmm[reg])?,
            (0x2b, NP | P66) => {
                if !mem {
                    return ud();
                }
                self.xmm_store(16, true, self.cpu.xmm[reg])?;
            }
            (0x2a, _) => self.cvt_from_int(pfx)?,
            (0x2c | 0x2d, _) => self.cvt_to_int(pfx, op == 0x2c)?,
            (0x2e | 0x2f, NP | P66) => {
                let b = self.xmm_rm(if pfx == P66 { 8 } else { 4 }, false)?;
                let a = self.cpu.xmm[reg];
                let mut m = self.mx();
                let quiet = op == 0x2e;
                let (lt, eq, un) = if pfx == P66 {
                    let (x, y) = (m.input(f64::from_bits(a as u64)), m.input(f64::from_bits(b as u64)));
                    (m.cmp(x, y, 1) && !x.nan() && !y.nan(), x == y, x.nan() || y.nan())
                } else {
                    let (x, y) = (m.input(f32::from_bits(a as u32)), m.input(f32::from_bits(b as u32)));
                    (m.cmp(x, y, 1) && !x.nan() && !y.nan(), x == y, x.nan() || y.nan())
                };
                if quiet {
                    m.flags &= !exc::IE;
                    let snan = if pfx == P66 {
                        f64::from_bits(a as u64).is_snan() || f64::from_bits(b as u64).is_snan()
                    } else {
                        f32::from_bits(a as u32).is_snan() || f32::from_bits(b as u32).is_snan()
                    };
                    if snan {
                        m.flags |= exc::IE;
                    }
                }
                self.sse_commit(&m)?;
                let fl = &mut self.cpu.eflags;
                *fl &= !(flags::ZF | flags::PF | flags::CF | flags::OF | flags::SF | flags::AF);
                if un {
                    *fl |= flags::ZF | flags::PF | flags::CF;
                } else if lt {
                    *fl |= flags::CF;
                } else if eq {
                    *fl |= flags::ZF;
                }
            }
            (0x50, NP | P66) => {
                if mem {
                    return ud();
                }
                let v = self.cpu.xmm[self.insn.rm()];
                let (bits, n) = if double { (64, 2) } else { (32, 4) };
                let mask = (0..n).fold(0, |acc, i| acc | ((lane(v, i, bits) >> (bits - 1)) as u32) << i);
                self.cpu.regs[reg] = mask;
            }
            (0x51 | 0x58 | 0x59 | 0x5c | 0x5e, _) => {
                let fop = match op {
                    0x51 => Op::Sqrt,
                    0x58 => Op::Add,
                    0x59 => Op::Mul,
                    0x5c => Op::Sub,
                    _ => Op::Div,
                };
                let b = self.fp_src(scalar, double)?;
                let a = self.cpu.xmm[reg];
                let mut m = self.mx();
                let r = if fop == Op::Sqrt {
                    let r = fp_lanes!(double, scalar, b, b, |x, _y| m.arith(Op::Sqrt, x, x).to_bits64());
                    if scalar { (a & !lane_low(double)) | (r & lane_low(double)) } else { r }
                } else {
                    fp_lanes!(double, scalar, a, b, |x, y| m.arith(fop, x, y).to_bits64())
                };
                self.sse_commit(&m)?;
                self.cpu.xmm[reg] = r;
            }
            (0x52 | 0x53, NP | F3) => {
                let b = self.fp_src(scalar, false)?;
                let a = self.cpu.xmm[reg];
                let r = fp_map::<f32>(b, b, scalar, |x, _| recip(x, op == 0x52).to_bits() as u64);
                self.cpu.xmm[reg] = if scalar { (a & !0xffff_ffff) | (r & 0xffff_ffff) } else { r };
            }
            (0x54..=0x57, NP | P66) => {
                let b = self.xmm_rm(16, true)?;
                let a = self.cpu.xmm[reg];
                self.cpu.xmm[reg] = match op {
                    0x54 => a & b,
                    0x55 => !a & b,
                    0x56 => a | b,
                    _ => a ^ b,
                };
            }
            (0x5d | 0x5f, _) => {
                let b = self.fp_src(scalar, double)?;
                let a = self.cpu.xmm[reg];
                let mut m = self.mx();
                let r = fp_lanes!(double, scalar, a, b, |x, y| m.minmax(x, y, op == 0x5f).to_bits64());
                self.sse_commit(&m)?;
                self.cpu.xmm[reg] = r;
            }
            (0x5a, _) => {
                let mut m = self.mx();
                let a = self.cpu.xmm[reg];
                let r = match pfx {
                    NP => {
                        let b = self.xmm_rm(8, false)?;
                        (m.widen(f32::from_bits(b as u32)) as u128) | (m.widen(f32::from_bits((b >> 32) as u32)) as u128) << 64
                    }
                    P66 => {
                        let b = self.xmm_rm(16, true)?;
                        (m.narrow(f64::from_bits(b as u64)) as u128) | (m.narrow(f64::from_bits((b >> 64) as u64)) as u128) << 32
                    }
                    F3 => {
                        let b = self.xmm_rm(4, false)?;
                        (a & !(u64::MAX as u128)) | m.widen(f32::from_bits(b as u32)) as u128
                    }
                    _ => {
                        let b = self.xmm_rm(8, false)?;
                        (a & !0xffff_ffff) | m.narrow(f64::from_bits(b as u64)) as u128
                    }
                };
                self.sse_commit(&m)?;
                self.cpu.xmm[reg] = r;
            }
            (0x5b, NP | P66 | F3) => {
                let b = self.xmm_rm(16, true)?;
                let mut m = self.mx();
                let r = fp_map::<f32>(b, b, false, |x, _| match pfx {
                    NP => m.int_to_f32(x.to_bits() as i32),
                    _ => m.cvt_int(x, pfx == F3),
                });
                self.sse_commit(&m)?;
                self.cpu.xmm[reg] = r;
            }
            (0xc2, _) => {
                let pred = self.insn.imm as u8 & 7;
                let b = self.fp_src(scalar, double)?;
                let a = self.cpu.xmm[reg];
                let mut m = self.mx();
                let r = fp_lanes!(double, scalar, a, b, |x, y| if m.cmp(x, y, pred) { !0 } else { 0 });
                self.sse_commit(&m)?;
                self.cpu.xmm[reg] = r;
            }
            (0xc6, NP | P66) => {
                let b = self.xmm_rm(16, true)?;
                let a = self.cpu.xmm[reg];
                let imm = self.insn.imm;
                self.cpu.xmm[reg] = if double {
                    (lane(a, imm & 1, 64) as u128) | (lane(b, (imm >> 1) & 1, 64) as u128) << 64
                } else {
                    let s = |v, i: u32| lane(v, (imm >> (2 * i)) & 3, 32) as u128;
                    s(a, 0) | s(a, 1) << 32 | s(b, 2) << 64 | s(b, 3) << 96
                };
            }
            (0xe6, P66 | F2) => {
                let b = self.xmm_rm(16, true)?;
                let mut m = self.mx();
                let lo = m.cvt_int(f64::from_bits(b as u64), pfx == P66);
                let hi = m.cvt_int(f64::from_bits((b >> 64) as u64), pfx == P66);
                self.sse_commit(&m)?;
                self.cpu.xmm[reg] = (lo | hi << 32) as u128;
            }
            (0xe6, F3) => {
                let b = self.xmm_rm(8, false)?;
                let f = |v: u64| (v as u32 as i32 as f64).to_bits() as u128;
                self.cpu.xmm[reg] = f(b as u64) | f((b >> 32) as u64) << 64;
            }
            _ => return ud(),
        }
        Ok(())
    }

    fn fp_src(&mut self, scalar: bool, double: bool) -> Res<u128> {
        if scalar { self.xmm_rm(if double { 8 } else { 4 }, false) } else { self.xmm_rm(16, true) }
    }

    fn cvt_from_int(&mut self, pfx: u8) -> Res<()> {
        let reg = self.insn.reg();
        let a = self.cpu.xmm[reg];
        let mut m = self.mx();
        let r = match pfx {
            NP | P66 => {
                let b = self.mm_rm()?;
                if self.insn.mem.is_none() {
                    self.mmx_transition()?;
                }
                let (x, y) = (b as u32 as i32, (b >> 32) as u32 as i32);
                if pfx == NP {
                    (a & !(u64::MAX as u128)) | m.int_to_f32(x) as u128 | (m.int_to_f32(y) as u128) << 32
                } else {
                    ((x as f64).to_bits() as u128) | ((y as f64).to_bits() as u128) << 64
                }
            }
            F3 => {
                let v = self.read(self.rm_loc(), 4)? as i32;
                (a & !0xffff_ffff) | m.int_to_f32(v) as u128
            }
            _ => {
                let v = self.read(self.rm_loc(), 4)? as i32;
                (a & !(u64::MAX as u128)) | (v as f64).to_bits() as u128
            }
        };
        self.sse_commit(&m)?;
        self.cpu.xmm[reg] = r;
        Ok(())
    }

    fn cvt_to_int(&mut self, pfx: u8, truncate: bool) -> Res<()> {
        let reg = self.insn.reg();
        let mut m = self.mx();
        match pfx {
            NP | P66 => {
                let b = if pfx == NP { self.xmm_rm(8, false)? } else { self.xmm_rm(16, true)? };
                self.mmx_transition()?;
                let r = if pfx == NP {
                    m.cvt_int(f32::from_bits(b as u32), truncate) | m.cvt_int(f32::from_bits((b >> 32) as u32), truncate) << 32
                } else {
                    m.cvt_int(f64::from_bits(b as u64), truncate) | m.cvt_int(f64::from_bits((b >> 64) as u64), truncate) << 32
                };
                self.sse_commit(&m)?;
                self.cpu.fpu.set_mm(reg, r);
            }
            _ => {
                let r = if pfx == F3 {
                    let b = self.xmm_rm(4, false)?;
                    m.cvt_int(f32::from_bits(b as u32), truncate)
                } else {
                    let b = self.xmm_rm(8, false)?;
                    m.cvt_int(f64::from_bits(b as u64), truncate)
                };
                self.sse_commit(&m)?;
                self.cpu.regs[reg] = r as u32;
            }
        }
        Ok(())
    }

    fn simd_int(&mut self, op: u8, pfx: u8) -> Res<()> {
        let reg = self.insn.reg();
        let rm = self.insn.rm();
        let mem = self.insn.mem.is_some();
        let imm = self.insn.imm as u8;
        match (op, pfx) {
            (0x77, NP) => {
                if self.cpu.cr0 & cr0::EM != 0 {
                    return ud();
                }
                if self.cpu.cr0 & cr0::TS != 0 {
                    return Err(Exception::new(vector::NM).into());
                }
                self.fpu_check_pending()?;
                self.cpu.fpu.tags = 0;
            }
            (0x6e, NP) => {
                self.mmx_enter()?;
                let v = self.read(self.rm_loc(), 4)?;
                self.cpu.fpu.set_mm(reg, v as u64);
            }
            (0x6e, P66) => {
                self.sse_enter()?;
                self.cpu.xmm[reg] = self.read(self.rm_loc(), 4)? as u128;
            }
            (0x7e, NP) => {
                self.mmx_enter()?;
                self.write(self.rm_loc(), 4, self.cpu.fpu.mm(reg) as u32)?;
            }
            (0x7e, P66) => {
                self.sse_enter()?;
                self.write(self.rm_loc(), 4, self.cpu.xmm[reg] as u32)?;
            }
            (0x7e, F3) => {
                self.sse_enter()?;
                self.cpu.xmm[reg] = self.xmm_rm(8, false)? as u64 as u128;
            }
            (0x6f, NP) => {
                self.mmx_enter()?;
                let v = self.mm_rm()?;
                self.cpu.fpu.set_mm(reg, v);
            }
            (0x6f, P66 | F3) => {
                self.sse_enter()?;
                self.cpu.xmm[reg] = self.xmm_rm(16, pfx == P66)?;
            }
            (0x7f, NP) | (0xe7, NP) => {
                if op == 0xe7 && !mem {
                    return ud();
                }
                self.mmx_enter()?;
                let v = self.cpu.fpu.mm(reg);
                match self.mem_operand() {
                    Some((seg, off)) => self.write_wide(seg, off, 8, v as u128)?,
                    None => self.cpu.fpu.set_mm(rm, v),
                }
            }
            (0x7f, P66 | F3) | (0xe7, P66) => {
                if op == 0xe7 && !mem {
                    return ud();
                }
                self.sse_enter()?;
                self.xmm_store(16, pfx == P66, self.cpu.xmm[reg])?;
            }
            (0xd6, P66) => {
                self.sse_enter()?;
                let v = self.cpu.xmm[reg] as u64 as u128;
                match self.mem_operand() {
                    Some((seg, off)) => self.write_wide(seg, off, 8, v)?,
                    None => self.cpu.xmm[rm] = v,
                }
            }
            (0xd6, F3 | F2) => {
                if mem {
                    return ud();
                }
                self.sse_enter()?;
                self.mmx_transition()?;
                if pfx == F3 {
                    self.cpu.xmm[reg] = self.cpu.fpu.mm(rm) as u128;
                } else {
                    self.cpu.fpu.set_mm(reg, self.cpu.xmm[rm] as u64);
                }
            }
            (0xf7, NP | P66) => {
                if mem {
                    return ud();
                }
                let (data, mask, n) = if pfx == NP {
                    self.mmx_enter()?;
                    (self.cpu.fpu.mm(reg) as u128, self.cpu.fpu.mm(rm) as u128, 8)
                } else {
                    self.sse_enter()?;
                    (self.cpu.xmm[reg], self.cpu.xmm[rm], 16)
                };
                let seg = self.insn.seg_override.map_or(DS, |s| s as usize);
                let base = if self.insn.addr32 { self.cpu.regs[EDI] } else { self.cpu.regs[EDI] & 0xffff };
                let sel: Vec<u32> = (0..n).filter(|&i| (mask >> (8 * i + 7)) & 1 != 0).collect();
                if let (Some(&first), Some(&last)) = (sel.first(), sel.last()) {
                    self.probe_span(seg, self.off_add(base, first), last - first + 1)?;
                }
                for i in sel {
                    self.write_seg(seg, self.off_add(base, i), 1, (data >> (8 * i)) as u32 & 0xff)?;
                }
            }
            (0x70, NP) => {
                self.mmx_enter()?;
                let v = self.mm_rm()? as u128;
                self.cpu.fpu.set_mm(reg, shuffle(v, 16, 0, 4, imm) as u64);
            }
            (0x70, _) => {
                self.sse_enter()?;
                let v = self.xmm_rm(16, true)?;
                self.cpu.xmm[reg] = match pfx {
                    P66 => shuffle(v, 32, 0, 4, imm),
                    F3 => shuffle(v, 16, 4, 4, imm),
                    _ => shuffle(v, 16, 0, 4, imm),
                };
            }
            (0x71..=0x73, NP | P66) => {
                if mem {
                    return ud();
                }
                let w = if pfx == NP { 64 } else { 128 };
                let bits = 16 << (op - 0x71);
                let count = imm as u64;
                let kind = match (op, reg) {
                    (0x71 | 0x72, 2) | (0x73, 2) => 0,
                    (0x71 | 0x72, 4) => 1,
                    (0x71 | 0x72, 6) | (0x73, 6) => 2,
                    (0x73, 3 | 7) if pfx == P66 => 3,
                    _ => return ud(),
                };
                let src = if pfx == NP {
                    self.mmx_enter()?;
                    self.cpu.fpu.mm(rm) as u128
                } else {
                    self.sse_enter()?;
                    self.cpu.xmm[rm]
                };
                let r = if kind == 3 {
                    let sh = 8 * count.min(16) as u32;
                    match (reg, sh) {
                        (_, 128) => 0,
                        (3, _) => src >> sh,
                        _ => src << sh,
                    }
                } else {
                    shift(src, bits, w, count, kind)
                };
                if pfx == NP {
                    self.cpu.fpu.set_mm(rm, r as u64);
                } else {
                    self.cpu.xmm[rm] = r;
                }
            }
            (0xc4, NP | P66) => {
                let v = self.read(self.rm_loc(), 2)? as u64;
                if pfx == NP {
                    self.mmx_enter()?;
                    let mut d = self.cpu.fpu.mm(reg) as u128;
                    set_lane(&mut d, imm as u32 & 3, 16, v);
                    self.cpu.fpu.set_mm(reg, d as u64);
                } else {
                    self.sse_enter()?;
                    set_lane(&mut self.cpu.xmm[reg], imm as u32 & 7, 16, v);
                }
            }
            (0xc5 | 0xd7, NP | P66) => {
                if mem {
                    return ud();
                }
                let (src, n) = if pfx == NP {
                    self.mmx_enter()?;
                    (self.cpu.fpu.mm(rm) as u128, 8)
                } else {
                    self.sse_enter()?;
                    (self.cpu.xmm[rm], 16)
                };
                self.cpu.regs[reg] = if op == 0xc5 {
                    lane(src, imm as u32 & (n / 2 - 1), 16) as u32
                } else {
                    (0..n).fold(0, |acc, i| acc | (((src >> (8 * i + 7)) & 1) as u32) << i)
                };
            }
            (_, NP) => {
                self.mmx_enter()?;
                let b = self.mm_rm()? as u128;
                let a = self.cpu.fpu.mm(reg) as u128;
                let Some(r) = int_op(op, a, b, 64) else { return ud() };
                self.cpu.fpu.set_mm(reg, r as u64);
            }
            (_, P66) => {
                if int_op(op, 0, 0, 128).is_none() {
                    return ud();
                }
                self.sse_enter()?;
                let b = self.xmm_rm(16, true)?;
                let a = self.cpu.xmm[reg];
                self.cpu.xmm[reg] = int_op(op, a, b, 128).unwrap();
            }
            _ => return ud(),
        }
        Ok(())
    }

    /// `0F AE`: FXSAVE, FXRSTOR, LDMXCSR, STMXCSR, LFENCE, MFENCE, SFENCE and
    /// CLFLUSH.
    pub(super) fn group15(&mut self) -> Res<()> {
        let mem = self.mem_operand();
        match (self.insn.reg(), mem) {
            (5..=7, None) => Ok(()),
            (7, Some((seg, off))) => {
                let lin = self.seg_check(seg, off, 1, Access::Read)?;
                self.translate(lin, Access::Read, self.cpu.cpl() == 3)?;
                Ok(())
            }
            (0, Some((seg, off))) => self.fxsave(seg, off),
            (1, Some((seg, off))) => self.fxrstor(seg, off),
            (2, Some((seg, off))) => {
                self.sse_enter()?;
                let v = self.read_seg(seg, off, 4)?;
                if v & !mxcsr::WRITABLE != 0 {
                    return gp(0);
                }
                self.cpu.mxcsr = v;
                Ok(())
            }
            (3, Some((seg, off))) => {
                self.sse_enter()?;
                self.write_seg(seg, off, 4, self.cpu.mxcsr)
            }
            _ => ud(),
        }
    }

    fn fx_checks(&self, seg: usize, off: u32) -> Res<()> {
        if self.cpu.cr0 & (cr0::EM | cr0::TS) != 0 {
            return Err(Exception::new(vector::NM).into());
        }
        self.check_align16(seg, off)
    }

    // Image layout and the OSFXSR rule follow Bochs cpu/sse_move.cc and
    // cpu/xsave.cc (FXSAVE, xsave_x87_state), by Stanislav Shwartsman.
    fn fxsave(&mut self, seg: usize, off: u32) -> Res<()> {
        self.fx_checks(seg, off)?;
        let f = &self.cpu.fpu;
        let header = f.cw as u128
            | (f.sw as u128) << 16
            | (f.tags as u128) << 32
            | (f.fop as u128) << 48
            | (f.fip as u128) << 64
            | (f.fcs as u128) << 96;
        let header2 = f.fdp as u128 | (f.fds as u128) << 32 | (self.cpu.mxcsr as u128) << 64 | (mxcsr::WRITABLE as u128) << 96;
        let osfxsr = self.cpu.cr4 & cr4::OSFXSR != 0;
        self.probe_span(seg, off, 512)?;
        self.write_wide(seg, off, 16, header)?;
        self.write_wide(seg, self.off_add(off, 16), 16, header2)?;
        for i in 0..8 {
            let v = super::x87::f80_to_wide(self.cpu.fpu.st(i));
            self.write_wide(seg, self.off_add(off, 32 + 16 * i as u32), 16, v)?;
        }
        if osfxsr {
            for i in 0..8 {
                self.write_wide(seg, self.off_add(off, 160 + 16 * i as u32), 16, self.cpu.xmm[i])?;
            }
        }
        Ok(())
    }

    fn fxrstor(&mut self, seg: usize, off: u32) -> Res<()> {
        self.fx_checks(seg, off)?;
        let header = self.read_wide(seg, off, 16)?;
        let header2 = self.read_wide(seg, self.off_add(off, 16), 16)?;
        let mut regs = [F80::ZERO; 8];
        for (i, r) in regs.iter_mut().enumerate() {
            *r = super::x87::f80_from_wide(self.read_wide(seg, self.off_add(off, 32 + 16 * i as u32), 10)?);
        }
        let osfxsr = self.cpu.cr4 & cr4::OSFXSR != 0;
        let mut xmm = self.cpu.xmm;
        if osfxsr {
            for (i, x) in xmm.iter_mut().enumerate() {
                *x = self.read_wide(seg, self.off_add(off, 160 + 16 * i as u32), 16)?;
            }
        }
        let new_mxcsr = (header2 >> 64) as u32;
        if new_mxcsr & !mxcsr::WRITABLE != 0 {
            return gp(0);
        }
        let f = &mut self.cpu.fpu;
        f.cw = header as u16 | 0x40;
        f.sw = (header >> 16) as u16;
        f.tags = (header >> 32) as u8;
        f.fop = (header >> 48) as u16 & 0x7ff;
        f.fip = (header >> 64) as u32;
        f.fcs = (header >> 96) as u16;
        f.fdp = header2 as u32;
        f.fds = (header2 >> 32) as u16;
        for (i, r) in regs.into_iter().enumerate() {
            let p = f.phys(i);
            f.regs[p] = r;
        }
        f.update_es();
        self.fpu_sync_ferr();
        self.cpu.mxcsr = new_mxcsr;
        self.cpu.xmm = xmm;
        Ok(())
    }
}

fn lane_low(double: bool) -> u128 {
    if double { u64::MAX as u128 } else { 0xffff_ffff }
}

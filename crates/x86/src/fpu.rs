//! x87 register file, the 80-bit extended format, and the rounding helpers
//! shared by the x87 and SSE paths.
//!
//! Registers hold raw 80-bit values so that FSAVE, FXSAVE and MMX aliasing are
//! exact. Loads, stores, compares, integer conversions, FRNDINT, FSCALE,
//! FXTRACT and FPREM work on the 80-bit values directly; other arithmetic goes
//! through `f64`.

/// Exception and status bits shared by the x87 status word and MXCSR.
pub mod exc {
    pub const IE: u16 = 1 << 0;
    pub const DE: u16 = 1 << 1;
    pub const ZE: u16 = 1 << 2;
    pub const OE: u16 = 1 << 3;
    pub const UE: u16 = 1 << 4;
    pub const PE: u16 = 1 << 5;
    pub const ALL: u16 = 0x3f;
}

/// x87 status word bits outside the exception flags.
pub mod sw {
    pub const SF: u16 = 1 << 6;
    pub const ES: u16 = 1 << 7;
    pub const C0: u16 = 1 << 8;
    pub const C1: u16 = 1 << 9;
    pub const C2: u16 = 1 << 10;
    pub const TOP: u16 = 7 << 11;
    pub const C3: u16 = 1 << 14;
    pub const B: u16 = 1 << 15;
    pub const CC: u16 = C0 | C1 | C2 | C3;
}

/// Rounding control values, as in the x87 control word and MXCSR.
pub mod rc {
    pub const NEAREST: u8 = 0;
    pub const DOWN: u8 = 1;
    pub const UP: u8 = 2;
    pub const ZERO: u8 = 3;
}

/// An 80-bit extended-precision value: `se` holds the sign (bit 15) and the
/// biased exponent; `mant` holds the significand with its explicit integer bit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, snapshot::Snap)]
pub struct F80 {
    pub mant: u64,
    pub se: u16,
}

use std::cmp::Ordering;

const BIAS: i32 = 16383;
const INT_BIT: u64 = 1 << 63;
const QUIET_BIT: u64 = 1 << 62;

impl F80 {
    pub const ZERO: F80 = F80 { mant: 0, se: 0 };
    pub const ONE: F80 = F80 { mant: INT_BIT, se: 0x3fff };
    /// The "real indefinite" QNaN.
    pub const INDEFINITE: F80 = F80 { mant: 0xc000_0000_0000_0000, se: 0xffff };

    pub const fn new(se: u16, mant: u64) -> F80 {
        F80 { mant, se }
    }

    pub fn from_bytes(b: [u8; 10]) -> F80 {
        let mut m = [0u8; 8];
        m.copy_from_slice(&b[..8]);
        F80 { mant: u64::from_le_bytes(m), se: u16::from_le_bytes([b[8], b[9]]) }
    }

    pub fn to_bytes(self) -> [u8; 10] {
        let mut b = [0u8; 10];
        b[..8].copy_from_slice(&self.mant.to_le_bytes());
        b[8..].copy_from_slice(&self.se.to_le_bytes());
        b
    }

    pub fn sign(self) -> bool {
        self.se & 0x8000 != 0
    }

    pub fn exp(self) -> u16 {
        self.se & 0x7fff
    }

    pub fn negated(self) -> F80 {
        F80 { se: self.se ^ 0x8000, ..self }
    }

    pub fn abs(self) -> F80 {
        F80 { se: self.se & 0x7fff, ..self }
    }

    pub fn with_sign(self, s: bool) -> F80 {
        F80 { se: (self.se & 0x7fff) | (s as u16) << 15, ..self }
    }

    pub fn is_zero(self) -> bool {
        self.exp() == 0 && self.mant == 0
    }

    pub fn is_denormal(self) -> bool {
        self.exp() == 0 && self.mant != 0
    }

    pub fn is_inf(self) -> bool {
        self.exp() == 0x7fff && self.mant == INT_BIT
    }

    pub fn is_nan(self) -> bool {
        self.exp() == 0x7fff && self.mant & INT_BIT != 0 && self.mant << 1 != 0
    }

    pub fn is_snan(self) -> bool {
        self.is_nan() && self.mant & QUIET_BIT == 0
    }

    /// Pseudo-NaNs, pseudo-infinities and unnormals: an x87 raises #IA on them.
    pub fn is_unsupported(self) -> bool {
        self.exp() != 0 && self.mant & INT_BIT == 0
    }

    pub fn quiet(self) -> F80 {
        F80 { mant: self.mant | QUIET_BIT, ..self }
    }

    pub fn inf(sign: bool) -> F80 {
        F80 { mant: INT_BIT, se: 0x7fff | (sign as u16) << 15 }
    }

    /// Splits a finite nonzero value into `(sign, e, m)` with `m` normalised
    /// (bit 63 set) and value `m * 2^(e - 63)`.
    pub fn unpack(self) -> (bool, i32, u64) {
        let e = self.exp().max(1) as i32 - BIAS;
        let lz = self.mant.leading_zeros();
        (self.sign(), e - lz as i32, self.mant << lz)
    }

    /// Builds a value from `m * 2^(e - 63)`, normalising `m`. The result must
    /// be exact in the extended format: values below the normal range become
    /// denormals with the low bits dropped, values above it become infinity.
    pub fn pack(sign: bool, e: i32, m: u64) -> F80 {
        if m == 0 {
            return F80::ZERO.with_sign(sign);
        }
        let lz = m.leading_zeros();
        let m = m << lz;
        let be = e - lz as i32 + BIAS;
        if be >= 0x7fff {
            return F80::inf(sign);
        }
        if be <= 0 {
            let sh = 1 - be;
            let mant = if sh >= 64 { 0 } else { m >> sh };
            return F80 { mant, se: (sign as u16) << 15 };
        }
        F80 { mant: m, se: be as u16 | (sign as u16) << 15 }
    }

    pub fn from_f64(x: f64) -> F80 {
        let bits = x.to_bits();
        let sign = bits >> 63 != 0;
        let be = ((bits >> 52) & 0x7ff) as i32;
        let frac = bits & ((1 << 52) - 1);
        match be {
            0x7ff => F80 { mant: INT_BIT | frac << 11, se: 0x7fff | (sign as u16) << 15 },
            0 if frac == 0 => F80::ZERO.with_sign(sign),
            0 => F80::pack(sign, -1074 + 63, frac),
            _ => F80 { mant: INT_BIT | frac << 11, se: (be - 1023 + BIAS) as u16 | (sign as u16) << 15 },
        }
    }

    pub fn from_f32_bits(bits: u32) -> F80 {
        let sign = bits >> 31 != 0;
        let be = ((bits >> 23) & 0xff) as i32;
        let frac = (bits & 0x7f_ffff) as u64;
        match be {
            0xff => F80 { mant: INT_BIT | frac << 40, se: 0x7fff | (sign as u16) << 15 },
            0 if frac == 0 => F80::ZERO.with_sign(sign),
            0 => F80::pack(sign, -149 + 63, frac),
            _ => F80 { mant: INT_BIT | frac << 40, se: (be - 127 + BIAS) as u16 | (sign as u16) << 15 },
        }
    }

    pub fn from_f64_bits(bits: u64) -> F80 {
        F80::from_f64(f64::from_bits(bits))
    }

    pub fn from_i64(v: i64) -> F80 {
        F80::pack(v < 0, 63, v.unsigned_abs())
    }

    /// Rounds to `f64` bits. Returns the bits and the exceptions raised.
    pub fn to_f64_bits(self, rc: u8) -> (u64, u16) {
        self.to_binary(rc, &BIN64)
    }

    pub fn to_f32_bits(self, rc: u8) -> (u32, u16) {
        let (b, e) = self.to_binary(rc, &BIN32);
        (b as u32, e)
    }

    /// Rounds the significand to `bits` bits by `rc` and keeps the exponent,
    /// as x87 precision control does; PE when bits were lost.
    pub fn round_significand(self, bits: u32, rc: u8) -> (F80, u16) {
        let exp = self.exp();
        if exp == 0 || exp == 0x7fff {
            return (self, 0);
        }
        let (kept, lost, half) = split(self.mant as u128, 64 - bits);
        if !lost {
            return (self, 0);
        }
        let mut m = kept + round_up(rc, self.sign(), half, kept) as u128;
        let mut exp = exp;
        if m >> bits != 0 {
            m >>= 1;
            exp += 1;
        }
        debug_assert!(exp < 0x7fff, "rounding a significand overflowed the exponent, which precision control cannot do to values from f64");
        (F80 { mant: (m as u64) << (64 - bits), se: (self.se & 0x8000) | exp }, exc::PE)
    }

    pub fn to_f64(self, rc: u8) -> (f64, u16) {
        let (b, e) = self.to_f64_bits(rc);
        (f64::from_bits(b), e)
    }

    fn to_binary(self, rc: u8, f: &Binary) -> (u64, u16) {
        let sign_bit = (self.sign() as u64) << (f.frac + f.exp_bits);
        let exp_all = ((1u64 << f.exp_bits) - 1) << f.frac;
        if self.is_unsupported() {
            return (f.indefinite, exc::IE);
        }
        if self.exp() == 0x7fff {
            if self.mant << 1 == 0 {
                return (sign_bit | exp_all, 0);
            }
            let payload = (self.mant << 1) >> (64 - f.frac);
            let quiet = 1u64 << (f.frac - 1);
            let e = if self.is_snan() { exc::IE } else { 0 };
            return (sign_bit | exp_all | payload | quiet, e);
        }
        if self.is_zero() {
            return (sign_bit, 0);
        }
        let (sign, e, m) = self.unpack();
        let (bits, mut x) = round_pack(sign, e, m, rc, f);
        if self.is_denormal() {
            x |= exc::DE;
        }
        (bits, x)
    }

    /// Rounds to an integer by `rc`. Returns `None` for NaNs and infinities,
    /// otherwise the rounded magnitude (saturated to `u128::MAX` when huge),
    /// whether it was inexact, and whether it was rounded away from zero.
    pub fn round_int(self, rc: u8) -> Option<(bool, u128, bool, bool)> {
        if self.exp() == 0x7fff || self.is_unsupported() {
            return None;
        }
        if self.is_zero() {
            return Some((self.sign(), 0, false, false));
        }
        let (sign, e, m) = self.unpack();
        let m = (m as u128) << 64;
        let sh = 127 - e;
        if sh < 0 {
            return Some((sign, u128::MAX, false, false));
        }
        let (int, lost, half) = split(m, sh as u32);
        if !lost {
            return Some((sign, int, false, false));
        }
        let up = round_up(rc, sign, half, int);
        Some((sign, int + up as u128, true, up))
    }

    /// Rounds to an integral value in extended format (FRNDINT).
    pub fn round_to_integral(self, rc: u8) -> (F80, bool) {
        if self.exp() == 0x7fff || self.is_zero() || self.is_unsupported() {
            return (self, false);
        }
        let (sign, e, _) = self.unpack();
        if e >= 63 {
            return (self, false);
        }
        let (_, mag, inexact, _) = self.round_int(rc).unwrap();
        (F80::pack(sign, 63, mag as u64), inexact)
    }

    /// x87 total ordering for non-NaN values: `Less`, `Equal` or `Greater`.
    pub fn cmp_num(self, b: F80) -> Ordering {
        use Ordering::*;
        if self.is_zero() && b.is_zero() {
            return Equal;
        }
        let key = |v: F80| (v.exp().max(1), v.mant);
        let ka = if self.is_zero() { (0, 0) } else { key(self) };
        let kb = if b.is_zero() { (0, 0) } else { key(b) };
        match (self.sign() && !self.is_zero(), b.sign() && !b.is_zero()) {
            (false, true) => Greater,
            (true, false) => Less,
            (false, false) => ka.cmp(&kb),
            (true, true) => kb.cmp(&ka),
        }
    }

    /// Classification used by the full tag word: 0 valid, 1 zero, 2 special.
    pub fn tag(self) -> u16 {
        if self.is_zero() {
            1
        } else if self.exp() == 0x7fff || self.exp() == 0 || self.mant & INT_BIT == 0 {
            2
        } else {
            0
        }
    }
}

/// An IEEE binary interchange format.
pub(crate) struct Binary {
    pub frac: u32,
    pub exp_bits: u32,
    pub bias: i32,
    pub indefinite: u64,
}

pub(crate) const BIN32: Binary = Binary { frac: 23, exp_bits: 8, bias: 127, indefinite: 0xffc0_0000 };
pub(crate) const BIN64: Binary = Binary { frac: 52, exp_bits: 11, bias: 1023, indefinite: 0xfff8_0000_0000_0000 };

/// Splits `m` at bit `sh`: the kept high part, whether any low bit was set,
/// and how the dropped part compares with one half.
fn split(m: u128, sh: u32) -> (u128, bool, Ordering) {
    match sh {
        0 => (m, false, Ordering::Less),
        1..=127 => {
            let lost = m & ((1u128 << sh) - 1);
            (m >> sh, lost != 0, lost.cmp(&(1u128 << (sh - 1))))
        }
        128 => (0, m != 0, m.cmp(&(1u128 << 127))),
        _ => (0, m != 0, Ordering::Less),
    }
}

/// Whether an inexact value gets rounded away from zero.
fn round_up(rc: u8, sign: bool, half: Ordering, kept: u128) -> bool {
    match rc {
        rc::NEAREST => half.is_gt() || (half.is_eq() && kept & 1 != 0),
        rc::DOWN => sign,
        rc::UP => !sign,
        _ => false,
    }
}

/// Whether a value just below the smallest normal of `f` rounds up to it when
/// rounded with unbounded exponent range: x86 detects tininess after rounding.
fn rounds_to_normal(sign: bool, m: u64, rc: u8, f: &Binary) -> bool {
    let (kept, lost, half) = split((m as u128) << 64, 63 - f.frac + 64);
    lost && round_up(rc, sign, half, kept) && (kept + 1) >> (f.frac + 1) != 0
}

/// Rounds `m * 2^(e - 63)`, where `m` has bit 63 set, to format `f` by `rc`.
pub(crate) fn round_pack(sign: bool, e: i32, m: u64, rc: u8, f: &Binary) -> (u64, u16) {
    let mut be = e + f.bias;
    let mut shift = 63 - f.frac;
    let tiny = be <= 0;
    let tiny_after_rounding = tiny && (be < 0 || !rounds_to_normal(sign, m, rc, f));
    if tiny {
        shift += (1 - be).min(100) as u32;
        be = 0;
    }
    let (kept, lost, half) = split((m as u128) << 64, shift + 64);
    let up = lost && round_up(rc, sign, half, kept);
    let kept = (kept + up as u128) as u64;
    let mut bits = if tiny { kept } else { ((be as u64 - 1) << f.frac) + kept };
    let mut x = if lost { exc::PE } else { 0 };
    if tiny_after_rounding && lost {
        x |= exc::UE;
    }
    let max_exp = (1u64 << f.exp_bits) - 1;
    if bits >= max_exp << f.frac {
        x |= exc::OE | exc::PE;
        let to_inf = match rc {
            rc::NEAREST => true,
            rc::DOWN => sign,
            rc::UP => !sign,
            _ => false,
        };
        bits = if to_inf { max_exp << f.frac } else { (max_exp << f.frac) - 1 };
    }
    (bits | (sign as u64) << (f.frac + f.exp_bits), x)
}

/// x87 data registers and control state.
#[derive(Clone, Debug, PartialEq, Eq, snapshot::Snap)]
pub struct Fpu {
    /// Physical registers R0..R7; ST(i) is `regs[(top + i) & 7]`.
    pub regs: [F80; 8],
    pub cw: u16,
    /// Status word, including TOP in bits 11..=13.
    pub sw: u16,
    /// Bit i set when physical register i is not empty (the FXSAVE abridged tag).
    pub tags: u8,
    /// Last non-control instruction: offset, CS selector and opcode (11 bits).
    pub fip: u32,
    pub fcs: u16,
    pub fop: u16,
    /// Last memory operand of a non-control instruction.
    pub fdp: u32,
    pub fds: u16,
}

impl Default for Fpu {
    fn default() -> Self {
        let mut f = Fpu { regs: [F80::ZERO; 8], cw: 0, sw: 0, tags: 0, fip: 0, fcs: 0, fop: 0, fdp: 0, fds: 0 };
        f.init();
        f
    }
}

impl Fpu {
    /// FNINIT. The register contents are kept.
    pub fn init(&mut self) {
        self.cw = 0x037f;
        self.sw = 0;
        self.tags = 0;
        self.fip = 0;
        self.fcs = 0;
        self.fop = 0;
        self.fdp = 0;
        self.fds = 0;
    }

    pub fn top(&self) -> usize {
        ((self.sw >> 11) & 7) as usize
    }

    pub fn set_top(&mut self, t: usize) {
        self.sw = (self.sw & !sw::TOP) | ((t as u16 & 7) << 11);
    }

    pub fn phys(&self, i: usize) -> usize {
        (self.top() + i) & 7
    }

    pub fn st(&self, i: usize) -> F80 {
        self.regs[self.phys(i)]
    }

    pub fn is_empty(&self, i: usize) -> bool {
        self.tags & (1 << self.phys(i)) == 0
    }

    pub fn set_st(&mut self, i: usize, v: F80) {
        let p = self.phys(i);
        self.regs[p] = v;
        self.tags |= 1 << p;
    }

    pub fn rc(&self) -> u8 {
        ((self.cw >> 10) & 3) as u8
    }

    /// Precision control: 0 single, 2 double, 3 extended.
    pub fn pc(&self) -> u8 {
        ((self.cw >> 8) & 3) as u8
    }

    /// Recomputes ES and B from the exception flags and masks.
    pub fn update_es(&mut self) {
        if self.sw & !self.cw & exc::ALL != 0 {
            self.sw |= sw::ES | sw::B;
        } else {
            self.sw &= !(sw::ES | sw::B);
        }
    }

    /// The 16-bit tag word, with tags recomputed from register contents.
    pub fn full_tag_word(&self) -> u16 {
        (0..8).fold(0, |tw, p| {
            let t = if self.tags & (1 << p) == 0 { 3 } else { self.regs[p].tag() };
            tw | t << (2 * p)
        })
    }

    pub fn set_full_tag_word(&mut self, tw: u16) {
        self.tags = (0..8).fold(0, |t, p| if (tw >> (2 * p)) & 3 == 3 { t } else { t | 1 << p });
    }

    /// MMX register `n` (the significand of physical register `n`).
    pub fn mm(&self, n: usize) -> u64 {
        self.regs[n].mant
    }

    pub fn set_mm(&mut self, n: usize, v: u64) {
        self.regs[n] = F80 { mant: v, se: 0xffff };
    }

    /// The x87-to-MMX transition every MMX instruction except EMMS performs.
    pub fn enter_mmx(&mut self) {
        self.set_top(0);
        self.tags = 0xff;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f64_roundtrip_is_exact() {
        for x in [1.0f64, -2.5, 1e-310, f64::MAX, f64::MIN_POSITIVE, 0.1, -0.0, f64::INFINITY] {
            let (y, e) = F80::from_f64(x).to_f64(rc::NEAREST);
            assert_eq!(y.to_bits(), x.to_bits());
            assert_eq!(e & !exc::DE, 0);
        }
    }

    #[test]
    fn store_to_f32_honours_rounding() {
        let third = F80::from_f64(1.0 / 3.0);
        let (n, e) = third.to_f32_bits(rc::NEAREST);
        let (u, _) = third.to_f32_bits(rc::UP);
        let (d, _) = third.to_f32_bits(rc::DOWN);
        assert_eq!(f32::from_bits(n), 1.0f32 / 3.0);
        assert_eq!(u, d + 1);
        assert!(e & exc::PE != 0);
        let (big, e) = F80::from_f64(1e300).to_f32_bits(rc::ZERO);
        assert_eq!(f32::from_bits(big), f32::MAX);
        assert!(e & exc::OE != 0);
    }

    #[test]
    fn precision_control_rounds_the_significand_but_keeps_the_exponent_range() {
        let big = F80::from_f64(f32::MAX as f64 * 2.0);
        let (r, e) = big.round_significand(24, rc::NEAREST);
        assert_eq!(r, big, "a value with a short significand past f32's range must stay as it is");
        assert_eq!(e, 0);
        let (r, e) = F80::from_f64(1e300 / 3.0).round_significand(24, rc::NEAREST);
        assert_eq!(r.mant & ((1 << 40) - 1), 0, "only 24 significand bits may be left");
        assert_eq!(e, exc::PE);
        assert!(r.to_f64(rc::NEAREST).0.is_finite());
        let tiny = 1e-300f64 / 3.0;
        let (r, _) = F80::from_f64(tiny).round_significand(24, rc::NEAREST);
        assert_eq!(r.to_f64(rc::NEAREST).0, f64::from_bits((tiny.to_bits() + (1 << 28)) >> 29 << 29), "values below f32's range keep 24 bits too");
        let (up, _) = F80::from_f64(1.0 + f64::EPSILON).round_significand(24, rc::UP);
        assert_eq!(up.to_f64(rc::NEAREST).0, 1.0 + f32::EPSILON as f64);
        let (all_ones, _) = F80 { mant: u64::MAX, se: 0x3fff }.round_significand(24, rc::NEAREST);
        assert_eq!(all_ones, F80 { mant: 1 << 63, se: 0x4000 }, "a carry out of the significand bumps the exponent");
    }

    #[test]
    fn tininess_is_detected_after_rounding() {
        let just_below = F80 { mant: u64::MAX, se: 0x3fff - 127 };
        let (bits, e) = just_below.to_f32_bits(rc::UP);
        assert_eq!(bits, 0x0080_0000);
        assert_eq!(e, exc::PE);
        let (bits, e) = just_below.to_f32_bits(rc::ZERO);
        assert_eq!(bits, 0x007f_ffff);
        assert_eq!(e, exc::PE | exc::UE);
    }

    #[test]
    fn integer_rounding() {
        let r = |x: f64, rc| {
            let (s, m, _, _) = F80::from_f64(x).round_int(rc).unwrap();
            if s { -(m as i64) } else { m as i64 }
        };
        assert_eq!(r(2.5, rc::NEAREST), 2);
        assert_eq!(r(3.5, rc::NEAREST), 4);
        assert_eq!(r(-2.5, rc::DOWN), -3);
        assert_eq!(r(-2.5, rc::UP), -2);
        assert_eq!(r(2.7, rc::ZERO), 2);
        assert_eq!(r(0.5, rc::NEAREST), 0);
        assert_eq!(r(0.75, rc::NEAREST), 1);
        assert_eq!(r(1e-30, rc::UP), 1);
        assert_eq!(r(-1e-30, rc::UP), 0);
    }

    #[test]
    fn tag_word_conversion() {
        let mut f = Fpu::default();
        f.regs[0] = F80::ONE;
        f.regs[1] = F80::ZERO;
        f.regs[2] = F80::INDEFINITE;
        f.regs[3] = F80::new(0, 1);
        f.tags = 0b1111;
        assert_eq!(f.full_tag_word(), 0xff00 | 2 << 6 | 2 << 4 | 1 << 2);
        f.set_full_tag_word(0xfff0 | 0b1001);
        assert_eq!(f.tags, 0b11);
    }

    #[test]
    fn compare_orders_denormals_and_signs() {
        use std::cmp::Ordering::*;
        let den = F80::new(0, 5);
        assert_eq!(den.cmp_num(F80::ZERO), Greater);
        assert_eq!(F80::ZERO.negated().cmp_num(F80::ZERO), Equal);
        assert_eq!(F80::ONE.negated().cmp_num(den.negated()), Less);
        assert_eq!(F80::inf(false).cmp_num(F80::from_f64(f64::MAX)), Greater);
    }
}

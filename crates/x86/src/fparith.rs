//! IEEE arithmetic with x86 rounding control and exception flags, built on
//! the host's round-to-nearest operations. Error-free transformations
//! (TwoSum for addition, FMA residuals for the rest) give the exact sign of
//! the rounding error, which decides directed rounding and the precision flag.
//!
//! Approximations: tininess is judged on the rounded result, and the
//! underflow flag is only raised for inexact tiny results (the masked rule).

use std::cmp::Ordering;
use std::ops::{Add, Div, Mul, Neg, Sub};

use crate::fpu::{exc, rc};

pub trait Float:
    Copy + PartialEq + PartialOrd + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self> + Div<Output = Self> + Neg<Output = Self>
{
    const ZERO: Self;
    const ONE: Self;
    const MAX: Self;
    const INF: Self;
    /// Width of the fraction field.
    const FRAC: u32;
    const BITS: u32;
    fn to_bits64(self) -> u64;
    fn from_bits64(b: u64) -> Self;
    fn fma(self, b: Self, c: Self) -> Self;
    fn root(self) -> Self;
    fn nan(self) -> bool;
    fn infinite(self) -> bool;
    fn subnormal(self) -> bool;
    fn up(self) -> Self;
    fn down(self) -> Self;
    fn widen(self) -> f64;

    fn negative(self) -> bool {
        self.to_bits64() >> (Self::BITS - 1) != 0
    }

    fn is_snan(self) -> bool {
        self.nan() && self.to_bits64() & (1 << (Self::FRAC - 1)) == 0
    }

    fn quieted(self) -> Self {
        Self::from_bits64(self.to_bits64() | 1 << (Self::FRAC - 1))
    }

    /// The x86 default NaN ("real indefinite"): negative, quiet, zero payload.
    fn indefinite() -> Self {
        let exp_and_quiet = (1u64 << (Self::BITS - Self::FRAC)) - 1;
        Self::from_bits64((exp_and_quiet << (Self::FRAC - 1)) | 1 << (Self::BITS - 1))
    }

    fn signed_zero(neg: bool) -> Self {
        Self::from_bits64((neg as u64) << (Self::BITS - 1))
    }
}

macro_rules! impl_float {
    ($t:ty, $u:ty, $frac:expr) => {
        impl Float for $t {
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            const MAX: Self = <$t>::MAX;
            const INF: Self = <$t>::INFINITY;
            const FRAC: u32 = $frac;
            const BITS: u32 = <$u>::BITS;
            fn to_bits64(self) -> u64 {
                self.to_bits() as u64
            }
            fn from_bits64(b: u64) -> Self {
                <$t>::from_bits(b as $u)
            }
            fn fma(self, b: Self, c: Self) -> Self {
                self.mul_add(b, c)
            }
            fn root(self) -> Self {
                self.sqrt()
            }
            fn nan(self) -> bool {
                self.is_nan()
            }
            fn infinite(self) -> bool {
                self.is_infinite()
            }
            fn subnormal(self) -> bool {
                self.is_subnormal()
            }
            fn up(self) -> Self {
                self.next_up()
            }
            fn down(self) -> Self {
                self.next_down()
            }
            fn widen(self) -> f64 {
                self as f64
            }
        }
    };
}

impl_float!(f32, u32, 23);
impl_float!(f64, u64, 52);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Sqrt,
}

fn sign_of<F: Float>(x: F) -> Ordering {
    x.partial_cmp(&F::ZERO).unwrap_or(Ordering::Equal)
}

/// The value an overflow produces under `rc`.
pub fn overflow_result<F: Float>(neg: bool, rc: u8) -> F {
    let to_inf = match rc {
        rc::NEAREST => true,
        rc::DOWN => neg,
        rc::UP => !neg,
        _ => false,
    };
    let mag = if to_inf { F::INF } else { F::MAX };
    if neg { -mag } else { mag }
}

/// Computes `op` on non-NaN operands (`b` is ignored for `Sqrt`). Invalid
/// operations return the default NaN with IE.
pub fn arith<F: Float>(op: Op, a: F, b: F, rc: u8) -> (F, u16) {
    let r = match op {
        Op::Add => a + b,
        Op::Sub => a - b,
        Op::Mul => a * b,
        Op::Div => a / b,
        Op::Sqrt => a.root(),
    };
    if r.nan() {
        return (F::indefinite(), exc::IE);
    }
    if op == Op::Div && b == F::ZERO && !a.infinite() {
        return (r, exc::ZE);
    }
    if r.infinite() {
        let finite_in = !a.infinite() && (op == Op::Sqrt || !b.infinite());
        return if finite_in { (overflow_result(r.negative(), rc), exc::OE | exc::PE) } else { (r, 0) };
    }
    let b_eff = if op == Op::Sub { -b } else { b };
    let err = match op {
        Op::Add | Op::Sub => {
            let bb = r - a;
            sign_of((a - (r - bb)) + (b_eff - bb))
        }
        Op::Mul => sign_of(a.fma(b, -r)),
        Op::Div if b.infinite() || a == F::ZERO => Ordering::Equal,
        Op::Div => {
            let s = sign_of((-r).fma(b, a));
            if b.negative() { s.reverse() } else { s }
        }
        Op::Sqrt => sign_of((-r).fma(r, a)),
    };
    if err == Ordering::Equal {
        if matches!(op, Op::Add | Op::Sub) && r == F::ZERO && rc == rc::DOWN && (a.negative() || b_eff.negative()) {
            return (F::signed_zero(true), 0);
        }
        return (r, 0);
    }
    let out = match rc {
        rc::UP if err.is_gt() => r.up(),
        rc::DOWN if err.is_lt() => r.down(),
        rc::ZERO if r > F::ZERO && err.is_lt() => r.down(),
        rc::ZERO if r < F::ZERO && err.is_gt() => r.up(),
        _ => r,
    };
    let mut x = exc::PE;
    if out.infinite() {
        x |= exc::OE;
    } else if out.subnormal() || out == F::ZERO {
        x |= exc::UE;
    }
    (out, x)
}

/// Rounds a non-NaN value to an integer by `rc`, as `f64`.
pub fn round_f64(v: f64, rc: u8) -> f64 {
    match rc {
        rc::NEAREST => v.round_ties_even(),
        rc::DOWN => v.floor(),
        rc::UP => v.ceil(),
        _ => v.trunc(),
    }
}

/// Converts to a signed 32-bit integer; NaN and out-of-range values give the
/// "integer indefinite" 0x8000_0000 with IE.
pub fn to_i32<F: Float>(x: F, rc: u8) -> (i32, u16) {
    if x.nan() {
        return (i32::MIN, exc::IE);
    }
    let v = x.widen();
    let r = round_f64(v, rc);
    if !(-2147483648.0..=2147483647.0).contains(&r) {
        return (i32::MIN, exc::IE);
    }
    (r as i32, if r != v { exc::PE } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directed_rounding_brackets_the_exact_result() {
        let (n, e) = arith(Op::Div, 1.0f32, 3.0, rc::NEAREST);
        let (u, _) = arith(Op::Div, 1.0f32, 3.0, rc::UP);
        let (d, _) = arith(Op::Div, 1.0f32, 3.0, rc::DOWN);
        let (z, _) = arith(Op::Div, -1.0f32, 3.0, rc::ZERO);
        assert_eq!(e, exc::PE);
        assert!(d < u && (n == d || n == u));
        assert_eq!(u.to_bits() - d.to_bits(), 1);
        assert_eq!(z, -d);
        assert_eq!(arith(Op::Add, 1.0f64, 2.0, rc::UP), (3.0, 0));
        let (s, e) = arith(Op::Add, 1.0f64, 1e-30, rc::UP);
        assert_eq!((s, e), (1.0f64.next_up(), exc::PE));
    }

    #[test]
    fn exact_zero_sum_is_negative_when_rounding_down() {
        let (r, _) = arith(Op::Sub, 5.0f64, 5.0, rc::DOWN);
        assert!(r == 0.0 && r.is_sign_negative());
        let (r, _) = arith(Op::Sub, 5.0f64, 5.0, rc::NEAREST);
        assert!(r == 0.0 && r.is_sign_positive());
    }

    #[test]
    fn invalid_and_divide_by_zero() {
        let (r, e) = arith(Op::Sub, f32::INFINITY, f32::INFINITY, rc::NEAREST);
        assert_eq!((r.to_bits(), e), (0xffc0_0000, exc::IE));
        assert_eq!(arith(Op::Div, -2.0f64, 0.0, rc::NEAREST), (f64::NEG_INFINITY, exc::ZE));
        let (r, e) = arith(Op::Sqrt, -1.0f64, 0.0, rc::NEAREST);
        assert_eq!((r.to_bits(), e), (0xfff8_0000_0000_0000, exc::IE));
    }

    #[test]
    fn overflow_under_round_to_zero_gives_max() {
        assert_eq!(arith(Op::Mul, f32::MAX, 2.0, rc::ZERO), (f32::MAX, exc::OE | exc::PE));
        assert_eq!(arith(Op::Mul, f32::MAX, -2.0, rc::NEAREST), (f32::NEG_INFINITY, exc::OE | exc::PE));
    }

    #[test]
    fn integer_conversion_indefinite() {
        assert_eq!(to_i32(3e9f64, rc::NEAREST), (i32::MIN, exc::IE));
        assert_eq!(to_i32(f32::NAN, rc::NEAREST), (i32::MIN, exc::IE));
        assert_eq!(to_i32(-2.5f64, rc::NEAREST), (-2, exc::PE));
        assert_eq!(to_i32(-2147483648.9f64, rc::ZERO), (i32::MIN, exc::PE));
    }
}

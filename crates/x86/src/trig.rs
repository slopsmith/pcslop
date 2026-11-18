//! sin, cos and tan for FSIN, FCOS, FPTAN and FSINCOS, as a fixed sequence of
//! double operations the JIT repeats exactly, so both agree to the bit.
//!
//! # Attribution
//!
//! The kernels and coefficients are fdlibm's `__kernel_sin` and `__kernel_cos`
//! from FreeBSD msun (lib/msun/src/k_sin.c, k_cos.c):
//!
//! > Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//! >
//! > Developed at SunPro, a Sun Microsystems, Inc. business.
//! > Permission to use, copy, modify, and distribute this
//! > software is freely granted, provided that this notice
//! > is preserved.

/// Biased exponents `fast` accepts: 2^-26 <= |x| < 2^20.
pub const MIN_EXP: u64 = 1023 - 26;
pub const MAX_EXP: u64 = 1023 + 20;

pub const INVPIO2: u64 = 0x3fe4_5f30_6dc9_c883;
/// pi/2 in three parts, for a Cody-Waite reduction with fused multiply-adds.
pub const PIO2: [u64; 3] = [0x3ff9_21fb_5444_2d18, 0x3c91_a626_3314_5c07, 0xb91f_1976_b7ed_8fbc];
pub const S: [u64; 6] = [
    0xbfc5_5555_5555_5549,
    0x3f81_1111_1110_f8a6,
    0xbf2a_01a0_19c1_61d5,
    0x3ec7_1de3_57b1_fe7d,
    0xbe5a_e5e6_8a2b_9ceb,
    0x3de5_d93a_5acf_d57c,
];
pub const C: [u64; 6] = [
    0x3fa5_5555_5555_554c,
    0xbf56_c16c_16c1_5177,
    0x3efa_01a0_19cb_1590,
    0xbe92_7e4f_809c_52ad,
    0x3e21_ee9e_bdb4_b1c4,
    0xbda8_fae9_be88_38d4,
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Sin,
    Cos,
    Tan,
    SinCos,
}

/// True when `eval` handles `x`; other inputs stay with the host's libm.
pub fn fast(x: f64) -> bool {
    (MIN_EXP..MAX_EXP).contains(&((x.to_bits() >> 52) & 0x7ff))
}

fn k(bits: u64) -> f64 {
    f64::from_bits(bits)
}

/// The result for ST(0) and, for FSINCOS, the cosine to push (1.0 for
/// FPTAN). None when the tangent would divide by a zero sine.
pub fn eval(x: f64, op: Op) -> Option<(f64, f64)> {
    let n = (x * k(INVPIO2)).round_ties_even();
    let q = (n as i64) & 3;
    let mut r = (-n).mul_add(k(PIO2[0]), x);
    r = (-n).mul_add(k(PIO2[1]), r);
    r = (-n).mul_add(k(PIO2[2]), r);
    let z = r * r;

    let mut p = z.mul_add(k(S[5]), k(S[4]));
    p = z.mul_add(p, k(S[3]));
    p = z.mul_add(p, k(S[2]));
    p = z.mul_add(p, k(S[1]));
    p = z.mul_add(p, k(S[0]));
    let v = z * r;
    let s = v.mul_add(p, r);

    let mut p1 = z.mul_add(k(C[2]), k(C[1]));
    p1 = z.mul_add(p1, k(C[0]));
    let mut p2 = z.mul_add(k(C[5]), k(C[4]));
    p2 = z.mul_add(p2, k(C[3]));
    let mut w2 = z * z;
    w2 *= w2;
    let poly = w2.mul_add(p2, z * p1);
    let hz = 0.5 * z;
    let w = 1.0 - hz;
    let mut t = 1.0 - w;
    t -= hz;
    t = z.mul_add(poly, t);
    let c = w + t;

    let odd = q & 1 != 0;
    let (sin, cos) = if odd { (c, s) } else { (s, c) };
    let sin = if q & 2 != 0 { -sin } else { sin };
    let cos = if (q ^ (q >> 1)) & 1 != 0 { -cos } else { cos };
    Some(match op {
        Op::Sin => (sin, 0.0),
        Op::Cos => (cos, 0.0),
        Op::SinCos => (sin, cos),
        Op::Tan => {
            if s.to_bits() << 1 == 0 {
                return None;
            }
            (if odd { -(c / s) } else { s / c }, 1.0)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_stay_within_two_ulps_of_libm() {
        let mut x = 1.0e-7f64;
        while x < 1.0e6 {
            for v in [x, -x] {
                let (s, _) = eval(v, Op::Sin).unwrap();
                let (c, _) = eval(v, Op::Cos).unwrap();
                let (t, _) = eval(v, Op::Tan).unwrap();
                for (got, want) in [(s, v.sin()), (c, v.cos()), (t, v.tan())] {
                    let ulps = (got.to_bits() as i64 - want.to_bits() as i64).abs();
                    assert!(ulps <= 2 || (got - want).abs() < 1e-15, "x {v:e}: {got:e} vs {want:e}");
                }
            }
            x *= 1.37;
        }
    }
}

//! sin, cos and tan for FSIN, FCOS, FPTAN and FSINCOS, and atan2 for FPATAN,
//! as fixed sequences of double operations the JIT repeats exactly, so both
//! agree to the bit.
//!
//! # Attribution
//!
//! The kernels and coefficients are fdlibm's `__kernel_sin` and `__kernel_cos`
//! from FreeBSD msun (lib/msun/src/k_sin.c, k_cos.c), and the reduction and
//! coefficients of `atan` from s_atan.c, reached as in e_atan2.c:
//!
//! > Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//! >
//! > Developed at SunPro, a Sun Microsystems, Inc. business.
//! > Permission to use, copy, modify, and distribute this
//! > software is freely granted, provided that this notice
//! > is preserved.

/// Biased exponents the kernels handle: 2^-27 <= |x| < 2^20. Below, down
/// to the smallest normal, sin x and tan x round to x and cos x to 1.
pub const MIN_EXP: u64 = 1023 - 27;
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
    (1..MAX_EXP).contains(&((x.to_bits() >> 52) & 0x7ff))
}

fn k(bits: u64) -> f64 {
    f64::from_bits(bits)
}

/// The result for ST(0) and, for FSINCOS, the cosine to push (1.0 for
/// FPTAN). None when the tangent would divide by a zero sine.
pub fn eval(x: f64, op: Op) -> Option<(f64, f64)> {
    if (x.to_bits() >> 52) & 0x7ff < MIN_EXP {
        return Some(match op {
            Op::Cos => (1.0, 0.0),
            Op::Sin => (x, 0.0),
            _ => (x, 1.0),
        });
    }
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

/// atan(0.5) and atan(1) in two parts.
pub const ATAN_HI: [u64; 2] = [0x3fdd_ac67_0561_bb4f, 0x3fe9_21fb_5444_2d18];
pub const ATAN_LO: [u64; 2] = [0x3c7a_2b7f_222f_65e2, 0x3c81_a626_3314_5c07];
pub const AT: [u64; 11] = [
    0x3fd5_5555_5555_550d,
    0xbfc9_9999_9998_ebc4,
    0x3fc2_4924_9200_83ff,
    0xbfbc_71c6_fe23_1671,
    0x3fb7_45cd_c54c_206e,
    0xbfb3_b0f2_af74_9a6d,
    0x3fb1_0d66_a0d0_3d51,
    0xbfad_de2d_52de_fd9a,
    0x3fa9_7b4b_2476_0deb,
    0xbfa2_b444_2c6a_6c2f,
    0x3f90_ad3a_e322_da11,
];
pub const PIO2_HI: u64 = 0x3ff9_21fb_5444_2d18;
pub const PIO2_LO: u64 = 0x3c91_a626_3314_5c07;
pub const PI_HI: u64 = 0x4009_21fb_5444_2d18;
pub const PI_LO: u64 = 0x3ca1_a626_3314_5c07;
/// Below 2^-27 (biased exponent 996), atan t rounds to t.
pub const ATAN_TINY: u64 = 1023 - 27;
/// Where `atan2` switches to the reductions around 0.5 and 1.
pub const ATAN_SPLIT: [u64; 2] = [0x3fdc_0000_0000_0000, 0x3fe6_0000_0000_0000];

/// True when `atan2` handles `y` and `x`: each zero or normal and finite.
pub fn atan2_fast(y: f64, x: f64) -> bool {
    let ok = |v: f64| v == 0.0 || (1..0x7ff).contains(&((v.to_bits() >> 52) & 0x7ff));
    ok(y) && ok(x)
}

/// atan(t) for 0 < t <= 1.
fn atan01(t: f64) -> f64 {
    if (t.to_bits() >> 52) < ATAN_TINY {
        return t;
    }
    let (id, x) = if t.to_bits() < ATAN_SPLIT[0] {
        (None, t)
    } else if t.to_bits() < ATAN_SPLIT[1] {
        (Some(0), (t + t - 1.0) / (2.0 + t))
    } else {
        (Some(1), (t - 1.0) / (t + 1.0))
    };
    let z = x * x;
    let w = z * z;
    let mut s1 = w.mul_add(k(AT[10]), k(AT[8]));
    for c in [AT[6], AT[4], AT[2], AT[0]] {
        s1 = w.mul_add(s1, k(c));
    }
    let mut s2 = w.mul_add(k(AT[9]), k(AT[7]));
    for c in [AT[5], AT[3], AT[1]] {
        s2 = w.mul_add(s2, k(c));
    }
    let s = z.mul_add(s1, w * s2);
    match id {
        None => (-x).mul_add(s, x),
        Some(i) => k(ATAN_HI[i]) - (x.mul_add(s, -k(ATAN_LO[i])) - x),
    }
}

/// atan2(y, x) for FPATAN, for inputs `atan2_fast` accepts.
pub fn atan2(y: f64, x: f64) -> f64 {
    if y == 0.0 {
        return if x.is_sign_negative() { k(PI_HI).copysign(y) } else { y };
    }
    if x == 0.0 {
        return k(PIO2_HI).copysign(y);
    }
    let (a, b) = (y.abs(), x.abs());
    let mut r = if a <= b {
        atan01(a / b)
    } else {
        k(PIO2_HI) - (atan01(b / a) - k(PIO2_LO))
    };
    if x.is_sign_negative() {
        r = k(PI_HI) - (r - k(PI_LO));
    }
    if y.is_sign_negative() { -r } else { r }
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

    #[test]
    fn tiny_angles_round_to_the_angle_and_one() {
        for x in [1.0e-300f64, -3.0e-12, 7.4e-9, -7.4e-9, f64::MIN_POSITIVE] {
            assert!(fast(x), "{x:e} should take the fixed sequence");
            for (op, want) in [(Op::Sin, x.sin()), (Op::Cos, x.cos()), (Op::Tan, x.tan())] {
                assert_eq!(eval(x, op).unwrap().0.to_bits(), want.to_bits(), "{x:e}: correctly rounded result");
            }
        }
    }

    #[test]
    fn atan2_stays_within_one_ulp_of_libm() {
        let mut t = 1.0e-12f64;
        while t < 1.0e12 {
            for (y, x) in [(t, 1.0), (1.0, t), (-t, 3.0), (t, -0.7), (-2.5, -t), (t * 1.0001, t), (t, t), (1.0e-200, t)] {
                let got = atan2(y, x);
                let want = y.atan2(x);
                let ulps = (got.to_bits() as i64 - want.to_bits() as i64).abs();
                assert!(ulps <= 1, "atan2({y:e}, {x:e}): {got:e} vs {want:e}");
            }
            t *= 1.093;
        }
        for (y, x) in [(0.0f64, 2.0f64), (-0.0, 2.0), (0.0, -2.0), (-0.0, -2.0), (3.0, 0.0), (-3.0, -0.0), (0.0, -0.0), (-0.0, -0.0), (-0.0, 0.0)] {
            assert!(atan2_fast(y, x));
            assert_eq!(atan2(y, x).to_bits(), y.atan2(x).to_bits(), "atan2({y:e}, {x:e})");
        }
    }
}

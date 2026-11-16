//! Arithmetic with eagerly computed flags. Every function returns the result
//! and the new values of the six arithmetic flags.

use super::mem::{mask, sign};
use crate::cpu::flags::*;

fn szp(r: u32, sz: u32) -> u32 {
    let mut f = 0;
    if r & mask(sz) == 0 {
        f |= ZF;
    }
    if r & sign(sz) != 0 {
        f |= SF;
    }
    if (r as u8).count_ones().is_multiple_of(2) {
        f |= PF;
    }
    f
}

pub fn add(a: u32, b: u32, carry: bool, sz: u32) -> (u32, u32) {
    let wide = a as u64 + b as u64 + carry as u64;
    let r = wide as u32 & mask(sz);
    let mut f = szp(r, sz);
    if wide > mask(sz) as u64 {
        f |= CF;
    }
    if (a ^ b ^ r) & 0x10 != 0 {
        f |= AF;
    }
    if (a ^ r) & (b ^ r) & sign(sz) != 0 {
        f |= OF;
    }
    (r, f)
}

pub fn sub(a: u32, b: u32, borrow: bool, sz: u32) -> (u32, u32) {
    let r = a.wrapping_sub(b).wrapping_sub(borrow as u32) & mask(sz);
    let mut f = szp(r, sz);
    if (a as u64) < b as u64 + borrow as u64 {
        f |= CF;
    }
    if (a ^ b ^ r) & 0x10 != 0 {
        f |= AF;
    }
    if (a ^ b) & (a ^ r) & sign(sz) != 0 {
        f |= OF;
    }
    (r, f)
}

pub fn logic(r: u32, sz: u32) -> (u32, u32) {
    let r = r & mask(sz);
    (r, szp(r, sz))
}

/// The eight group-1 operations, in ModRM `reg` order. `cf` is the incoming carry.
pub fn group1(op: usize, a: u32, b: u32, cf: bool, sz: u32) -> (u32, u32) {
    match op {
        0 => add(a, b, false, sz),
        1 => logic(a | b, sz),
        2 => add(a, b, cf, sz),
        3 => sub(a, b, cf, sz),
        4 => logic(a & b, sz),
        5 | 7 => sub(a, b, false, sz),
        _ => logic(a ^ b, sz),
    }
}

/// INC/DEC keep CF; the caller merges it back.
pub fn inc(a: u32, sz: u32) -> (u32, u32) {
    add(a, 1, false, sz)
}

pub fn dec(a: u32, sz: u32) -> (u32, u32) {
    sub(a, 1, false, sz)
}

/// Group-2 shifts and rotates. Returns `None` when the masked count is zero,
/// in which case neither the operand nor the flags change.
pub fn shift(op: usize, a: u32, count: u32, cf_in: bool, old: u32, sz: u32) -> Option<(u32, u32)> {
    let bits = sz * 8;
    let count = count & 0x1f;
    if count == 0 {
        return None;
    }
    let a = a & mask(sz);
    let msb = |v: u32| v & sign(sz) != 0;
    let keep = old & (ARITH & !(CF | OF));
    match op {
        0 | 1 => {
            let n = count % bits;
            let r = if op == 0 {
                if n == 0 { a } else { ((a << n) | (a >> (bits - n))) & mask(sz) }
            } else if n == 0 {
                a
            } else {
                ((a >> n) | (a << (bits - n))) & mask(sz)
            };
            let cf = if op == 0 { r & 1 != 0 } else { msb(r) };
            let of = if op == 0 { msb(r) ^ cf } else { msb(r) ^ (r & (sign(sz) >> 1) != 0) };
            Some((r, keep | (cf as u32 * CF) | (of as u32 * OF)))
        }
        2 | 3 => {
            let n = count % (bits + 1);
            let mut r = a;
            let mut cf = cf_in;
            let of;
            if op == 2 {
                for _ in 0..n {
                    let out = msb(r);
                    r = ((r << 1) | cf as u32) & mask(sz);
                    cf = out;
                }
                of = msb(r) ^ cf;
            } else {
                of = msb(a) ^ cf_in;
                for _ in 0..n {
                    let out = r & 1 != 0;
                    r = (r >> 1) | if cf { sign(sz) } else { 0 };
                    cf = out;
                }
            }
            let of = if n == 0 { old & OF != 0 } else { of };
            Some((r, keep | (cf as u32 * CF) | (of as u32 * OF)))
        }
        4 | 6 => {
            let wide = (a as u64) << count;
            let r = wide as u32 & mask(sz);
            let cf = count <= bits && (wide >> bits) & 1 != 0;
            let of = msb(r) ^ cf;
            let mut f = szp(r, sz) | (cf as u32 * CF) | (of as u32 * OF);
            f |= old & AF;
            Some((r, f))
        }
        5 => {
            let r = if count >= bits { 0 } else { a >> count };
            let cf = count <= bits && (a >> (count - 1)) & 1 != 0;
            let of = msb(a);
            let f = szp(r, sz) | (cf as u32 * CF) | (of as u32 * OF) | (old & AF);
            Some((r, f))
        }
        _ => {
            let s = sext(a, sz) as i32;
            let n = count.min(31);
            let r = (s >> n) as u32 & mask(sz);
            let cf = if count > bits { s < 0 } else { (s >> (count - 1)) & 1 != 0 };
            let f = szp(r, sz) | (cf as u32 * CF) | (old & AF);
            Some((r, f))
        }
    }
}

fn sext(v: u32, sz: u32) -> u32 {
    super::mem::sext(v, sz)
}

/// SHLD (`left`) / SHRD. Returns `None` for a zero count.
pub fn shift_double(left: bool, dst: u32, src: u32, count: u32, old: u32, sz: u32) -> Option<(u32, u32)> {
    let bits = sz * 8;
    let count = count & 0x1f;
    if count == 0 {
        return None;
    }
    let (r, cf) = if sz == 2 {
        // Counts above 16 are undefined; P6-family parts shift through dst:src:dst.
        let v = ((dst as u128) << 32) | ((src as u128) << 16) | dst as u128;
        if left {
            (((v << count) >> 32) as u32 & 0xffff, (v << (count - 1)) >> 47 & 1 != 0)
        } else {
            ((v >> count) as u32 & 0xffff, (v >> (count - 1)) & 1 != 0)
        }
    } else if left {
        let wide = ((dst as u64) << 32) | src as u64;
        (((wide << count) >> 32) as u32, (dst >> (bits - count)) & 1 != 0)
    } else {
        let wide = ((src as u64) << 32) | dst as u64;
        ((wide >> count) as u32, (dst >> (count - 1)) & 1 != 0)
    };
    let of = (r ^ dst) & sign(sz) != 0;
    Some((r, szp(r, sz) | (cf as u32 * CF) | (of as u32 * OF) | (old & AF)))
}

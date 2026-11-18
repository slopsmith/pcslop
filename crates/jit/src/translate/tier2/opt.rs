//! Passes over a region's IR.

use super::ir::*;

pub const CF: u8 = 1;
pub const PF: u8 = 2;
pub const AF: u8 = 4;
pub const ZF: u8 = 8;
pub const SF: u8 = 16;
pub const OF: u8 = 32;
pub const ALL: u8 = 63;

/// Flags x86 condition `cc` reads.
pub fn cond_reads(cc: u8) -> u8 {
    match cc >> 1 {
        0 => OF,
        1 => CF,
        2 => ZF,
        3 => CF | ZF,
        4 => SF,
        5 => PF,
        6 => SF | OF,
        _ => ZF | SF | OF,
    }
}

pub fn run(f: &mut Func) {
    fold(f);
    simplify(f);
    demand_flags(f);
    dce(f);
}

/// Replaces values by equal earlier ones: identities (`x & x`, `x + 0`) and
/// repeated pure operations within a block.
fn simplify(f: &mut Func) {
    let n = f.insts.len();
    let mut repl: Vec<Val> = (0..n as Val).collect();
    let find = |repl: &Vec<Val>, mut v: Val| {
        while repl[v as usize] != v {
            v = repl[v as usize];
        }
        v
    };
    for b in 0..f.blocks.len() {
        let mut seen: std::collections::HashMap<Op, Val> = std::collections::HashMap::new();
        for i in 0..f.blocks[b].insts.len() {
            let v = f.blocks[b].insts[i];
            let mut op = f.insts[v as usize].op;
            op.map_args(|a| find(&repl, a));
            let konst = |x: Val| match f.insts[x as usize].op {
                Op::Const(c) => Some(c),
                _ => None,
            };
            let same = match op {
                Op::Bin(Alu::And | Alu::Or, x, y, None) if x == y => Some(x),
                Op::Bin(Alu::Add | Alu::Sub | Alu::Or | Alu::Xor | Alu::Shl | Alu::Shr | Alu::Sar, x, y, None) if konst(y) == Some(0) => Some(x),
                Op::Bin(Alu::Add | Alu::Or | Alu::Xor, x, y, None) if konst(x) == Some(0) => Some(y),
                Op::Bin(Alu::And, x, y, None) if konst(y) == Some(u32::MAX) => Some(x),
                _ => None,
            };
            if let Op::Bin(Alu::Xor | Alu::Sub, x, y, None) = op
                && x == y
            {
                op = Op::Const(0);
            }
            f.insts[v as usize].op = op;
            if let Some(x) = same {
                repl[v as usize] = x;
                continue;
            }
            if op.pure() {
                match seen.get(&op) {
                    Some(&w) => repl[v as usize] = w,
                    None => {
                        seen.insert(op, v);
                    }
                }
            }
        }
    }
    let map = |repl: &Vec<Val>, v: Val| find(repl, v);
    for b in &mut f.blocks {
        for o in &mut b.outs {
            *o = map(&repl, *o);
        }
        if let Term::Branch { cond, .. } = &mut b.term {
            *cond = map(&repl, *cond);
        }
    }
    for snap in f.snaps.values_mut() {
        for s in snap.iter_mut() {
            *s = map(&repl, *s);
        }
    }
    for b in 0..f.blocks.len() {
        f.blocks[b].insts.retain(|&v| repl[v as usize] == v);
    }
}

/// Folds operations on constants and drops identities.
fn fold(f: &mut Func) {
    let konst = |f: &Func, v: Val| match f.op(v) {
        Op::Const(c) => Some(c),
        _ => None,
    };
    for b in 0..f.blocks.len() {
        for i in 0..f.blocks[b].insts.len() {
            let v = f.blocks[b].insts[i];
            let new = match f.op(v) {
                Op::Bin(op, a, bb, None) => match (konst(f, a), konst(f, bb)) {
                    (Some(x), Some(y)) => eval(op, x, y).map(Op::Const),
                    _ => None,
                },
                Op::Extract { v: src, lsb, width, signed } => konst(f, src).map(|x| {
                    let field = (x >> lsb) & mask(width);
                    Op::Const(if signed { sign_extend(field, width) } else { field })
                }),
                Op::Insert { dst, src, lsb, width } => match (konst(f, dst), konst(f, src)) {
                    (Some(d), Some(s)) => {
                        let m = mask(width) << lsb;
                        Some(Op::Const((d & !m) | ((s << lsb) & m)))
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(op) = new {
                f.insts[v as usize].op = op;
            }
        }
    }
}

fn mask(width: u8) -> u32 {
    if width >= 32 { u32::MAX } else { (1 << width) - 1 }
}

fn sign_extend(v: u32, width: u8) -> u32 {
    let sh = 32 - width as u32;
    (((v << sh) as i32) >> sh) as u32
}

pub fn eval(op: Alu, x: u32, y: u32) -> Option<u32> {
    Some(match op {
        Alu::Add => x.wrapping_add(y),
        Alu::Sub => x.wrapping_sub(y),
        Alu::And => x & y,
        Alu::Or => x | y,
        Alu::Xor => x ^ y,
        Alu::Shl => x.checked_shl(y).unwrap_or(0),
        Alu::Shr => x.checked_shr(y).unwrap_or(0),
        Alu::Sar => ((x as i32) >> y.min(31)) as u32,
        Alu::Adc | Alu::Sbb => return None,
    })
}

/// Narrows every flags value to the bits something reads: conditions, the
/// carry ADC, SBB and INC/DEC pass on, successors, and exits (all of them).
fn demand_flags(f: &mut Func) {
    let n = f.insts.len();
    let mut want = vec![0u8; n];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..f.blocks.len()).rev() {
            let blk = &f.blocks[b];
            let out = blk.outs[FLAGS] as usize;
            let mut need = match blk.term {
                Term::Exit(_) => ALL,
                _ => 0,
            };
            for s in f.succs(b as Blk) {
                need |= want[f.blocks[s as usize].params[FLAGS] as usize];
            }
            if want[out] | need != want[out] {
                want[out] |= need;
                changed = true;
            }
            for &v in blk.insts.iter().rev() {
                let add: Vec<(Val, u8)> = match f.op(v) {
                    Op::Cond(cc, fl) => vec![(fl, cond_reads(cc))],
                    Op::Bin(_, _, _, Some(c)) => vec![(c, CF)],
                    Op::Flags { c: Some(c), .. } if want[v as usize] != 0 => vec![(c, CF)],
                    Op::IncDecFlags { old, .. } if want[v as usize] & CF != 0 => vec![(old, CF)],
                    _ => vec![],
                };
                for (src, bits) in add {
                    if want[src as usize] | bits != want[src as usize] {
                        want[src as usize] |= bits;
                        changed = true;
                    }
                }
            }
        }
        for b in 0..f.blocks.len() {
            let p = f.blocks[b].params[FLAGS] as usize;
            let w = want[p];
            for pred in 0..f.blocks.len() {
                if f.succs(pred as Blk).contains(&(b as Blk)) {
                    let out = f.blocks[pred].outs[FLAGS] as usize;
                    if want[out] | w != want[out] {
                        want[out] |= w;
                        changed = true;
                    }
                }
            }
        }
    }
    for (v, inst) in f.insts.iter_mut().enumerate() {
        if let Op::Flags { bits, .. } = &mut inst.op {
            *bits = want[v];
        }
    }
}

/// Removes values nothing uses, keeping memory accesses.
fn dce(f: &mut Func) {
    let n = f.insts.len();
    let mut live = vec![false; n];
    let mut stack: Vec<Val> = Vec::new();
    for b in &f.blocks {
        stack.extend(b.outs);
        if let Term::Branch { cond, .. } = b.term {
            stack.push(cond);
        }
        for &v in &b.insts {
            let op = f.insts[v as usize].op;
            if op.has_effect() {
                stack.push(v);
            }
            if let Op::Flags { bits, .. } = op
                && bits != 0
                && live[v as usize]
            {
                stack.push(v);
            }
        }
        for (_, snap) in f.snaps.iter().filter(|(v, _)| b.insts.contains(v)) {
            stack.extend(snap[..FLAGS].iter().copied());
        }
    }
    while let Some(v) = stack.pop() {
        if std::mem::replace(&mut live[v as usize], true) {
            continue;
        }
        stack.extend(f.insts[v as usize].op.args());
    }
    for b in &mut f.blocks {
        b.insts.retain(|&v| live[v as usize]);
    }
}

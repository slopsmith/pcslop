//! Which x86 arithmetic flags each instruction of a block must produce. A
//! backward pass over the decoded block drops flags that a later instruction
//! overwrites before anything reads them; at the end of the block and at
//! conditional side exits the flags the following code reads are live.
//! Instructions not listed here count as reading every flag.
//!
//! The flags a guest exception handler sees after a fault in the middle of a
//! block can therefore be stale; the handler restores them, the faulting
//! instruction runs again and the same later instruction overwrites them.

use x86::decode::{Insn, Rep};

pub const CF: u8 = 1;
pub const PF: u8 = 2;
pub const AF: u8 = 4;
pub const ZF: u8 = 8;
pub const SF: u8 = 16;
pub const OF: u8 = 32;
pub const ALL: u8 = 63;
/// The flags kept in A64 NZCV.
pub const NZCV: u8 = CF | ZF | SF | OF;
const SHIFTED: u8 = CF | PF | ZF | SF | OF;

/// How an instruction affects the block's flow, for the pass.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Next,
    /// The block ends after this instruction.
    End,
    /// The block ends after the next instruction (STI's interrupt shadow).
    EndAfterNext,
    /// A conditional branch: the block goes on at the next instruction.
    Branch,
    /// An instruction outside the table: it reads and may write every flag.
    Unknown,
}

/// Flags an instruction reads, always writes and may write (only the last
/// are produced but not killed), and how it ends the block.
#[derive(Clone, Copy)]
pub struct Effect {
    pub reads: u8,
    pub writes: u8,
    pub may: u8,
    pub flow: Flow,
}

pub fn effect(insn: &Insn) -> Effect {
    let (reads, writes, flow) = known(insn);
    let may = if flow == Flow::Unknown { ALL } else { 0 };
    let may = match insn.op {
        0xd2 | 0xd3 => SHIFTED,
        0xa6 | 0xa7 | 0xae | 0xaf => ALL,
        _ => may,
    };
    let flow = if flow == Flow::Unknown { Flow::End } else { flow };
    Effect { reads, writes, may, flow }
}

fn known(insn: &Insn) -> (u8, u8, Flow) {
    use Flow::*;
    let (op, reg) = (insn.op, insn.reg());
    let alu = |aop: usize| -> (u8, u8, Flow) {
        let reads = if matches!(aop, 2 | 3) { CF } else { 0 };
        (reads, ALL, Next)
    };
    let shift = |count: Option<u32>| -> (u8, u8, Flow) {
        let carry_in = if matches!(reg, 2 | 3) { CF } else { 0 };
        match count.map(|n| n & 31) {
            Some(0) => (0, 0, Next),
            Some(_) if reg < 4 => (carry_in, CF | OF, Next),
            Some(_) => (0, SHIFTED, Next),
            None => (carry_in, 0, Next),
        }
    };
    match op {
        0x00..=0x3f if op & 7 < 6 => alu((op >> 3) as usize & 7),
        0x40..=0x4f => (0, ALL & !CF, Next),
        0x80..=0x83 => alu(reg),
        0x84 | 0x85 | 0xa8 | 0xa9 => (0, ALL, Next),
        0x50..=0x5f | 0x88..=0x8b | 0x8d | 0x90 | 0x91..=0x97 | 0x98 | 0x99 | 0xb0..=0xbf | 0xc6 | 0xc7 | 0x86 | 0x87 => (0, 0, Next),
        0x68 | 0x6a | 0x1b6 | 0x1b7 | 0x1be | 0x1bf | 0x1c8..=0x1cf => (0, 0, Next),
        0xa4 | 0xa5 | 0xaa..=0xad => (0, 0, Next),
        0x06 | 0x07 | 0x0e | 0x16 | 0x17 | 0x1e | 0x1f | 0x1a0 | 0x1a1 | 0x1a8 | 0x1a9 | 0x8c | 0x8e | 0x8f | 0x60 | 0x61 => (0, 0, Next),
        0x1bc | 0x1bd => (0, ZF, Next),
        0x121 | 0x123 | 0x100 | 0x120 | 0x122 | 0x106 | 0x131 | 0x1ae | 0xed => (0, 0, Next),
        0x134 | 0x135 => (0, 0, End),
        0xcf => (0, ALL, End),
        0xa6 | 0xa7 | 0xae | 0xaf if insn.rep == Rep::None => (0, ALL, Next),
        0xa6 | 0xa7 | 0xae | 0xaf => (0, 0, Next),
        0xc0 | 0xc1 => shift(Some(insn.imm)),
        0xd0 | 0xd1 => shift(Some(1)),
        0xd2 | 0xd3 => shift(None),
        0xf6 | 0xf7 => match reg {
            0 | 1 | 3 | 4 | 5 => (0, ALL, Next),
            2 => (0, 0, Next),
            _ => (ALL, 0, Unknown),
        },
        0xfe | 0xff if reg < 2 => (0, ALL & !CF, Next),
        0x1af | 0x69 | 0x6b => (0, ALL, Next),
        0x1a3 | 0x1ab | 0x1b3 | 0x1bb | 0x1ba => (0, CF, Next),
        0xf5 => (CF, CF, Next),
        0xf8 | 0xf9 => (0, CF, Next),
        0xfa | 0xfc | 0xfd => (0, 0, Next),
        0xfb => (0, 0, EndAfterNext),
        0x9c => (ALL, 0, Next),
        0x9e => (0, CF | PF | AF | ZF | SF, Next),
        0x9f => (CF | PF | AF | ZF | SF, 0, Next),
        0x1c0 | 0x1c1 | 0x1b0 | 0x1b1 => (0, ALL, Next),
        0x140..=0x14f | 0x190..=0x19f => (cond_reads(op), 0, Next),
        0x70..=0x7f | 0x180..=0x18f => (cond_reads(op), 0, Branch),
        0xe9 | 0xeb => (0, 0, End),
        0xe0..=0xe3 | 0xe8 | 0xc2 | 0xc3 | 0x9d => (ALL, 0, End),
        0xff if matches!(reg, 2 | 4) => (ALL, 0, End),
        0xd8..=0xdf => match (op & 7, insn.mem.is_some(), insn.modrm) {
            (3 | 7, false, 0xe8..=0xf7) => (0, ALL, Next),
            (2 | 3, false, 0xc0..=0xdf) => (ALL, 0, Next),
            _ => (0, 0, Next),
        },
        0x12e | 0x12f => (0, ALL, Next),
        0x110..=0x12d | 0x150..=0x17f | 0x1c2..=0x1c6 | 0x1d0..=0x1ff => (0, 0, Next),
        _ => (ALL, 0, Unknown),
    }
}

/// The flags condition code `op & 15` tests.
pub fn cond_reads(op: u16) -> u8 {
    match op & 15 {
        0 | 1 => OF,
        2 | 3 => CF,
        4 | 5 => ZF,
        6 | 7 => CF | ZF,
        8 | 9 => SF,
        10 | 11 => PF,
        12 | 13 => SF | OF,
        _ => ZF | SF | OF,
    }
}

/// The flags each instruction must produce, given the effects of the
/// instructions of the block in order and the flags live after the block.
pub fn needed(effects: &[Effect], live_out: u8) -> Vec<u8> {
    let mut live = live_out;
    let mut need = vec![ALL; effects.len()];
    for (i, e) in effects.iter().enumerate().rev() {
        need[i] = (e.writes | e.may) & live;
        live = (live & !e.writes) | e.reads;
    }
    need
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(reads: u8, writes: u8, may: u8) -> Effect {
        Effect { reads, writes, may, flow: Flow::Next }
    }

    #[test]
    fn flags_overwritten_before_any_read_are_dead() {
        assert_eq!(needed(&[e(0, ALL, 0), e(0, ALL & !CF, 0), e(0, ALL, 0), e(ALL, 0, 0)], ALL), [0, 0, ALL, 0]);
        assert_eq!(needed(&[e(0, ALL, 0), e(0, ALL & !CF, 0), e(0, 0, 0)], ALL), [CF, ALL & !CF, 0]);
    }

    #[test]
    fn a_shift_by_cl_keeps_earlier_flags_and_produces_its_own() {
        assert_eq!(needed(&[e(0, ALL, 0), e(0, 0, SHIFTED), e(0, ALL, 0)], ALL), [0, 0, ALL]);
        assert_eq!(needed(&[e(0, ALL, 0), e(0, 0, SHIFTED)], ALL), [ALL, SHIFTED]);
    }
}

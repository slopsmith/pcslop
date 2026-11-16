//! The block of memory shared between the host and the runtime in the VM.

use std::mem::offset_of;

/// Exit reasons the runtime stores in `State::reason`.
pub mod reason {
    /// Exit to translate the block at `State::eip`.
    pub const MISS: u32 = 1;
    /// Exit to run the instruction at `State::eip` in the interpreter.
    pub const INTERP: u32 = 2;
    /// Exit requested by the host through `State::pending`.
    pub const PENDING: u32 = 3;
    /// Exit for a synchronous exception in the VM; `State::info` holds ESR, FAR, ELR, SPSR.
    pub const FAULT: u32 = 4;
    /// Exit for an exception the runtime cannot handle; `State::info` as for `FAULT`.
    pub const UNEXPECTED: u32 = 5;
}

/// Maintenance the runtime performs before continuing (`State::ctl`).
pub mod ctl {
    pub const TLBI_ALL: u32 = 1;
    pub const IC_IALLU: u32 = 2;
    pub const TLBI_PAGE: u32 = 4;
}

/// How the runtime continues after a FAULT exit (`State::resume`).
pub mod resume {
    /// Re-execute the faulting ARM instruction.
    pub const RETRY: u32 = 0;
    /// Abandon the block and dispatch at `State::eip`.
    pub const DISPATCH: u32 = 1;
}

#[repr(C)]
#[derive(Debug)]
pub struct State {
    /// EAX..EDI, in x86 encoding order.
    pub regs: [u32; 8],
    pub eip: u32,
    /// ARM NZCV as read by MRS: N, Z and V are x86 SF, ZF and OF; C is the inverse of CF.
    pub nzcv: u32,
    /// Value whose low byte decides PF.
    pub pf_result: u32,
    /// Value whose bit 4 is AF.
    pub af_aux: u32,
    pub reason: u32,
    pub ctl: u32,
    pub resume: u32,
    pub pending: u32,
    pub fs_base: u32,
    pub gs_base: u32,
    pub tlbi_page: u64,
    pub lookup: u64,
    /// x86 instructions retired by translated code.
    pub icount: u64,
    /// When non-zero, counts dispatches down and exits with PENDING at zero.
    pub budget: u32,
    /// ESR, FAR, ELR, SPSR for FAULT exits.
    pub info: [u64; 4],
    /// x0..x17 saved across FAULT exits.
    pub scratch: [u64; 18],
}

pub const REGS: usize = offset_of!(State, regs);
pub const EIP: usize = offset_of!(State, eip);
pub const NZCV: usize = offset_of!(State, nzcv);
pub const PF_RESULT: usize = offset_of!(State, pf_result);
pub const AF_AUX: usize = offset_of!(State, af_aux);
pub const REASON: usize = offset_of!(State, reason);
pub const CTL: usize = offset_of!(State, ctl);
pub const RESUME: usize = offset_of!(State, resume);
pub const PENDING: usize = offset_of!(State, pending);
pub const FS_BASE: usize = offset_of!(State, fs_base);
pub const GS_BASE: usize = offset_of!(State, gs_base);
pub const TLBI_PAGE: usize = offset_of!(State, tlbi_page);
pub const LOOKUP: usize = offset_of!(State, lookup);
pub const ICOUNT: usize = offset_of!(State, icount);
pub const BUDGET: usize = offset_of!(State, budget);
pub const INFO: usize = offset_of!(State, info);
pub const SCRATCH: usize = offset_of!(State, scratch);

const NZCV_N: u32 = 1 << 31;
const NZCV_Z: u32 = 1 << 30;
const NZCV_C: u32 = 1 << 29;
const NZCV_V: u32 = 1 << 28;

impl State {
    /// Loads the x86 arithmetic flags into the runtime's representation.
    pub fn set_eflags(&mut self, eflags: u32) {
        use x86::cpu::flags::*;
        let mut n = 0;
        if eflags & SF != 0 {
            n |= NZCV_N;
        }
        if eflags & ZF != 0 {
            n |= NZCV_Z;
        }
        if eflags & CF == 0 {
            n |= NZCV_C;
        }
        if eflags & OF != 0 {
            n |= NZCV_V;
        }
        self.nzcv = n;
        self.pf_result = if eflags & PF != 0 { 0 } else { 1 };
        self.af_aux = eflags & AF;
    }

    /// Returns `eflags` with its arithmetic flags replaced by the runtime's.
    pub fn merge_eflags(&self, eflags: u32) -> u32 {
        use x86::cpu::flags::*;
        let mut f = eflags & !ARITH;
        if self.nzcv & NZCV_N != 0 {
            f |= SF;
        }
        if self.nzcv & NZCV_Z != 0 {
            f |= ZF;
        }
        if self.nzcv & NZCV_C == 0 {
            f |= CF;
        }
        if self.nzcv & NZCV_V != 0 {
            f |= OF;
        }
        if (self.pf_result as u8).count_ones().is_multiple_of(2) {
            f |= PF;
        }
        f | (self.af_aux & AF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eflags_round_trip() {
        let s = std::mem::MaybeUninit::<State>::zeroed();
        let mut s = unsafe { s.assume_init() };
        for f in 0..64u32 {
            let bits = [1, 4, 0x10, 0x40, 0x80, 0x800];
            let eflags = bits.iter().enumerate().filter(|(i, _)| f & (1 << i) != 0).fold(0x202, |a, (_, b)| a | b);
            s.set_eflags(eflags);
            assert_eq!(s.merge_eflags(0x202), eflags);
        }
    }
}

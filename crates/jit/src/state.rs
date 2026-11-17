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
    /// The code bytes of the block at `State::eip` changed; drop it and dispatch again.
    pub const STALE: u32 = 6;
}

/// Maintenance the runtime performs before continuing (`State::ctl`).
pub mod ctl {
    pub const TLBI_ALL: u32 = 1;
    pub const IC_IALLU: u32 = 2;
    pub const TLBI_PAGE: u32 = 4;
}

/// Bits of `State::walk_flags`.
pub mod walk {
    pub const PSE: u32 = 1;
    pub const PGE: u32 = 2;
    pub const WP: u32 = 4;
    /// Non-PAE paging is on, so the in-VM fill may run.
    pub const ON: u32 = 8;
}

/// Bits in the third word of a fill log entry, next to the physical page.
pub mod fill {
    pub const WRITABLE: u32 = 1;
    pub const GLOBAL: u32 = 2;
    pub const BIG: u32 = 4;
    /// Filled in the CPL 3 tree.
    pub const USER: u32 = 8;
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
    /// Set by the host while the interrupt controller has a request; the
    /// dispatcher exits on it once IF is set.
    pub irq: u32,
    /// x86 IF (0 or 1), so CLI and STI need no exit.
    pub if_flag: u32,
    /// x86 DF (0 or 1).
    pub df: u32,
    /// 1 while running CPL 3 code; part of the block lookup key.
    pub user: u32,
    pub fs_base: u32,
    /// Current CS selector, so a RETF to the same segment needs no exit.
    pub cs_sel: u32,
    pub gs_base: u32,
    /// EFLAGS without the arithmetic flags, IF and DF; POPF exits if it would change them.
    pub eflags_rest: u32,
    pub tlbi_page: u64,
    pub lookup: u64,
    /// `layout::VLOOKUP` and the epoch its entries must carry to be valid; the
    /// host bumps it whenever a mapping or a block they could name goes away.
    pub vlookup: u64,
    pub vepoch: u32,
    /// x86 instructions retired by translated code.
    pub icount: u64,
    /// When non-zero, counts dispatches down and exits with PENDING at zero.
    pub budget: u32,
    /// ESR, FAR, ELR, SPSR for FAULT exits.
    pub info: [u64; 4],
    /// x0..x17 saved across FAULT exits.
    pub scratch: [u64; 18],
    /// Virtual address of the root of the shadow tree TTBR0 points at.
    pub shadow_root: u64,
    /// CR3 page-directory address, for the in-VM fill.
    pub walk_cr3: u32,
    /// `walk` bits: what the in-VM fill may assume about x86 paging.
    pub walk_flags: u32,
    pub ram_size: u32,
    /// Entries in `layout::FILL_LOG` the host has not taken in yet.
    pub fill_count: u32,
    /// MMX registers (the x87 significands), and whether MMX instructions may
    /// run here: CR0.EM and CR0.TS clear, no x87 exception pending.
    pub mm: [u64; 8],
    pub mmx_ok: u32,
    /// Bit n: MM n was written; bit 8: an MMX instruction ran.
    pub mm_used: u32,
    /// Sign and exponent of each physical x87 register; `mm` holds the significands.
    pub fse: [u16; 8],
    pub fcw: u32,
    pub fsw: u32,
    /// Bit n: physical x87 register n is not empty.
    pub ftags: u32,
    /// CR0.EM and CR0.TS clear and no x87 exception pending.
    pub x87_base: u32,
    /// `x87_base`, all x87 exceptions masked and precision 53 or 64 bits:
    /// translated x87 code may run.
    pub x87_ok: u32,
    pub fip: u32,
    pub fdp: u32,
    /// FOP of the last x87 instruction translated code ran, with bit 31 set; 0 if none.
    pub fnote: u32,
    /// 0x100 plus the segment register index of FDS once translated code set FDP; 0 if not.
    pub fdseg: u32,
    /// 1 when SSE instructions may run: CR0.TS and CR0.EM clear, CR4.OSFXSR set.
    pub sse_ok: u32,
    pub xmm: [u128; 8],
    pub mxcsr: u32,
    /// `sse_ok`, all SIMD exceptions masked and FPCR set up for MXCSR: SSE
    /// arithmetic may run on the A64 FPU.
    pub sse_fp_ok: u32,
    /// FPSR exception flags gathered at each exit from the VM.
    pub fpsr: u32,
    pub ds_sel: u32,
    pub es_sel: u32,
    pub fs_sel: u32,
    pub gs_sel: u32,
    pub gdt_base: u32,
    pub gdt_limit: u32,
    pub sysenter_cs: u32,
    pub sysenter_esp: u32,
    pub sysenter_eip: u32,
    pub cr0: u32,
    pub cr2: u32,
    pub cr3: u32,
    pub cr4: u32,
    /// TTBR0 value and root VA of the CPL 0 and CPL 3 shadow trees, so
    /// SYSENTER and SYSEXIT switch trees without an exit.
    pub ttbr0: [u64; 2],
    pub roots: [u64; 2],
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
pub const IRQ: usize = offset_of!(State, irq);
pub const IF_FLAG: usize = offset_of!(State, if_flag);
pub const DF: usize = offset_of!(State, df);
pub const USER: usize = offset_of!(State, user);
pub const CS_SEL: usize = offset_of!(State, cs_sel);
pub const FS_BASE: usize = offset_of!(State, fs_base);
pub const GS_BASE: usize = offset_of!(State, gs_base);
pub const EFLAGS_REST: usize = offset_of!(State, eflags_rest);
pub const TLBI_PAGE: usize = offset_of!(State, tlbi_page);
pub const LOOKUP: usize = offset_of!(State, lookup);
pub const VLOOKUP: usize = offset_of!(State, vlookup);
pub const VEPOCH: usize = offset_of!(State, vepoch);
pub const ICOUNT: usize = offset_of!(State, icount);
pub const BUDGET: usize = offset_of!(State, budget);
pub const INFO: usize = offset_of!(State, info);
pub const SCRATCH: usize = offset_of!(State, scratch);
pub const SSE_OK: usize = offset_of!(State, sse_ok);
pub const MM: usize = offset_of!(State, mm);
pub const MMX_OK: usize = offset_of!(State, mmx_ok);
pub const MM_USED: usize = offset_of!(State, mm_used);
pub const SHADOW_ROOT: usize = offset_of!(State, shadow_root);
pub const WALK_CR3: usize = offset_of!(State, walk_cr3);
pub const WALK_FLAGS: usize = offset_of!(State, walk_flags);
pub const RAM_SIZE: usize = offset_of!(State, ram_size);
pub const FILL_COUNT: usize = offset_of!(State, fill_count);
pub const XMM: usize = offset_of!(State, xmm);
pub const MXCSR: usize = offset_of!(State, mxcsr);
pub const SSE_FP_OK: usize = offset_of!(State, sse_fp_ok);
pub const FPSR: usize = offset_of!(State, fpsr);
pub const DS_SEL: usize = offset_of!(State, ds_sel);
pub const ES_SEL: usize = offset_of!(State, es_sel);
pub const FS_SEL: usize = offset_of!(State, fs_sel);
pub const GS_SEL: usize = offset_of!(State, gs_sel);
pub const GDT_BASE: usize = offset_of!(State, gdt_base);
pub const GDT_LIMIT: usize = offset_of!(State, gdt_limit);
pub const SYSENTER_CS: usize = offset_of!(State, sysenter_cs);
pub const SYSENTER_ESP: usize = offset_of!(State, sysenter_esp);
pub const SYSENTER_EIP: usize = offset_of!(State, sysenter_eip);
pub const CR0: usize = offset_of!(State, cr0);
pub const TTBR0: usize = offset_of!(State, ttbr0);
pub const ROOTS: usize = offset_of!(State, roots);
pub const FSE: usize = offset_of!(State, fse);
pub const FCW: usize = offset_of!(State, fcw);
pub const FSW: usize = offset_of!(State, fsw);
pub const FTAGS: usize = offset_of!(State, ftags);
pub const X87_BASE: usize = offset_of!(State, x87_base);
pub const X87_OK: usize = offset_of!(State, x87_ok);
pub const FIP: usize = offset_of!(State, fip);
pub const FDP: usize = offset_of!(State, fdp);
pub const FNOTE: usize = offset_of!(State, fnote);
pub const FDSEG: usize = offset_of!(State, fdseg);

/// FPCR for SSE arithmetic under `mxcsr` (with FEAT_AFP): AH for x86 NaN and
/// MIN/MAX rules, FIZ for DAZ, FZ for FTZ with underflow masked, RMode from RC.
/// FIZ flushes without raising IDC, so translated code never sets MXCSR.DE.
pub fn fpcr_for(mxcsr: u32) -> u64 {
    let rc = (mxcsr >> 13) & 3;
    let rmode = (rc & 1) << 1 | rc >> 1;
    let daz = mxcsr & 0x40 != 0;
    let fz = mxcsr & 0x8000 != 0 && mxcsr & 0x800 != 0;
    daz as u64 | 2 | (rmode as u64) << 22 | (fz as u64) << 24
}

/// MXCSR exception flags for the FPSR bits in `fpsr`.
pub fn mxcsr_flags(fpsr: u32) -> u32 {
    let bit = |b: u32, to: u32| (fpsr >> b & 1) << to;
    bit(0, 0) | bit(1, 2) | bit(2, 3) | bit(3, 4) | bit(4, 5)
}

/// Whether translated x87 code may run with this control word.
pub fn x87_ok(base: bool, cw: u16) -> bool {
    base && cw & 0x3f == 0x3f && cw & 0x200 != 0
}

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

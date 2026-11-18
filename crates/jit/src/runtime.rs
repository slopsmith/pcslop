//! Code that runs at EL1 inside the VM: exception vectors, the dispatcher and
//! the paths that hand control back to the host.

use a64::{Asm, Cond, Extend, Label, Logic, Reg, Shift, Size, Width, ZR, r, sysreg};

use crate::layout;
use crate::state::{self, fill, reason, walk};

/// x86 EAX..EDI live in w19..w26 while translated code runs.
pub const fn guest_reg(n: usize) -> Reg {
    Reg(19 + n as u8)
}

pub const PF_RESULT: Reg = Reg(27);
pub const AF_AUX: Reg = Reg(18);
pub const STATE: Reg = Reg(28);
pub const ICOUNT: Reg = Reg(17);

/// 2^64 / golden ratio, for the block lookup's Fibonacci hash.
pub const HASH_MUL: u64 = 0x9e37_79b9_7f4a_7c15;

/// Offsets (from the start of the runtime region) of entry points that
/// translated code branches to.
#[derive(Clone, Copy, Debug)]
pub struct Entries {
    /// Reloads guest state from `State` and dispatches at `State::eip`.
    pub resume: usize,
    /// Dispatches at the EIP in w0.
    pub dispatch: usize,
    /// Saves guest state and exits with EIP in w0 and the reason in w1.
    pub exit: usize,
}

fn ctl_maintenance(a: &mut Asm) {
    let done = a.label();
    let no_all = a.label();
    let no_page = a.label();
    let no_ic = a.label();
    a.ldr(Width::W, r(1), STATE, state::CTL as i32);
    a.cbz(Size::W, r(1), done);
    a.tbz(r(1), 0, no_all);
    a.tlbi_vmalle1();
    a.bind(no_all);
    a.tbz(r(1), 2, no_page);
    a.ldr(Width::X, r(2), STATE, state::TLBI_PAGE as i32);
    a.tlbi_vaae1(r(2));
    a.bind(no_page);
    a.dsb(0xb);
    a.tbz(r(1), 1, no_ic);
    a.ic_iallu();
    a.dsb(0xb);
    a.bind(no_ic);
    a.isb();
    a.str(Width::W, ZR, STATE, state::CTL as i32);
    a.bind(done);
}

fn store_guest(a: &mut Asm, nzcv_from_spsr: Option<Reg>) {
    for i in (0..8).step_by(2) {
        a.stp(Size::W, guest_reg(i), guest_reg(i + 1), STATE, (state::REGS + i * 4) as i32);
    }
    match nzcv_from_spsr {
        Some(spsr) => {
            let ok = a.logic_imm(Logic::And, Size::W, r(9), spsr, 0xf000_0000);
            assert!(ok);
        }
        None => a.mrs(r(9), sysreg::NZCV),
    }
    a.str(Width::W, r(9), STATE, state::NZCV as i32);
    a.str(Width::W, PF_RESULT, STATE, state::PF_RESULT as i32);
    a.str(Width::W, AF_AUX, STATE, state::AF_AUX as i32);
    a.str(Width::X, ICOUNT, STATE, state::ICOUNT as i32);
    // After x87 code the live FPSR holds x87 flags; the MXCSR ones are saved.
    let (live, merge) = (a.label(), a.label());
    a.ldr(Width::W, r(9), STATE, state::X87_TAINT as i32);
    a.cbz(Size::W, r(9), live);
    a.ldr(Width::W, r(9), STATE, state::FPSR_SAVED as i32);
    a.str(Width::W, ZR, STATE, state::FPSR_SAVED as i32);
    a.b(merge);
    a.bind(live);
    a.mrs(r(9), sysreg::FPSR);
    a.bind(merge);
    a.ldr(Width::W, r(10), STATE, state::FPSR as i32);
    a.orr(Size::W, r(10), r(10), r(9));
    a.str(Width::W, r(10), STATE, state::FPSR as i32);
    a.msr(sysreg::FPSR, ZR);
}

fn load_guest(a: &mut Asm) {
    for i in (0..8).step_by(2) {
        a.ldp(Size::W, guest_reg(i), guest_reg(i + 1), STATE, (state::REGS + i * 4) as i32);
    }
    a.ldr(Width::W, r(9), STATE, state::NZCV as i32);
    a.msr(sysreg::NZCV, r(9));
    a.ldr(Width::W, PF_RESULT, STATE, state::PF_RESULT as i32);
    a.ldr(Width::W, AF_AUX, STATE, state::AF_AUX as i32);
    a.ldr(Width::X, ICOUNT, STATE, state::ICOUNT as i32);
}

fn save_scratch(a: &mut Asm) {
    for i in (0..18).step_by(2) {
        a.stp(Size::X, r(i as u8), r(i as u8 + 1), STATE, (state::SCRATCH + i * 8) as i32);
    }
}

fn restore_scratch(a: &mut Asm) {
    for i in (0..18).step_by(2) {
        a.ldp(Size::X, r(i as u8), r(i as u8 + 1), STATE, (state::SCRATCH + i * 8) as i32);
    }
}

/// `dst` = `base` | 1 << 33 | GEPOCH << 34, the dispatcher tag of code on a global page.
fn global_tag(a: &mut Asm, dst: Reg, base: Reg) {
    a.ldr(Width::W, r(2), STATE, state::GEPOCH as i32);
    a.logic_shift(Logic::Orr, false, Size::X, dst, base, r(2), Shift::Lsl, 34);
    let ok = a.logic_imm(Logic::Orr, Size::X, dst, dst, 1 << 33);
    assert!(ok);
}

/// x1 = the current shadow tree's leaf for the page of EIP (x0), or a branch
/// to `absent`. Clobbers x2 and x4.
fn shadow_leaf(a: &mut Asm, absent: Label) {
    a.ldr(Width::X, r(1), STATE, state::SHADOW_ROOT as i32);
    for (shift, bits) in [(30, 2), (21, 9)] {
        a.ubfx(Size::X, r(2), r(0), shift, bits);
        a.add_shift(Size::X, r(1), r(1), r(2), Shift::Lsl, 3);
        a.ldr(Width::X, r(2), r(1), 0);
        a.tbz(r(2), 0, absent);
        let ok = a.logic_imm(Logic::And, Size::X, r(2), r(2), 0x0000_ffff_ffff_f000);
        assert!(ok);
        a.mov_imm(Size::X, r(4), layout::RUNTIME_VA.wrapping_sub(layout::RUNTIME_IPA));
        a.add(Size::X, r(1), r(2), r(4));
    }
    a.ubfx(Size::X, r(2), r(0), 12, 9);
    a.add_shift(Size::X, r(1), r(1), r(2), Shift::Lsl, 3);
    a.ldr(Width::X, r(1), r(1), 0);
    a.tbz(r(1), 0, absent);
}

/// Fills a shadow leaf for the current fault from the x86 page tables; cases
/// for the host branch to `fail`, with x0..x17 saved in `State::scratch`.
fn try_fill(a: &mut Asm, fail: Label) {
    let (esr, far, flags, count, leaf, t6, t7, phys_va, ram) = (r(0), r(1), r(3), r(4), r(5), r(6), r(7), r(8), r(9));
    let (pde_va, pde, user, write, eff, pte_va, pte) = (r(10), r(11), r(12), r(13), r(2), r(15), r(16));
    let phys = r(14);
    let logic = |a: &mut Asm, op: Logic, sz: Size, rd: Reg, rn: Reg, imm: u64| {
        let ok = a.logic_imm(op, sz, rd, rn, imm);
        assert!(ok);
    };
    a.mrs(esr, sysreg::ESR_EL1);
    a.ubfx(Size::X, t6, esr, 26, 6);
    a.cmp_imm(Size::W, t6, 0x25);
    a.b_cond(Cond::Ne, fail);
    logic(a, Logic::And, Size::W, t6, esr, 0x3f);
    let kind_ok = a.label();
    a.cmp_imm(Size::W, t6, 0x07);
    a.b_cond(Cond::Eq, kind_ok);
    a.cmp_imm(Size::W, t6, 0x0f);
    a.b_cond(Cond::Ne, fail);
    a.bind(kind_ok);
    a.mrs(far, sysreg::FAR_EL1);
    a.lsr_imm(Size::X, t6, far, 32);
    a.cbnz(Size::X, t6, fail);
    a.ldr(Width::W, flags, STATE, state::WALK_FLAGS as i32);
    a.tbz(flags, walk::ON.trailing_zeros(), fail);
    a.ldr(Width::W, count, STATE, state::FILL_COUNT as i32);
    a.cmp_imm(Size::W, count, layout::FILL_LOG_ENTRIES as u32);
    a.b_cond(Cond::Hs, fail);

    let delta = layout::RUNTIME_VA.wrapping_sub(layout::RUNTIME_IPA);
    a.ldr(Width::X, leaf, STATE, state::SHADOW_ROOT as i32);
    for (shift, bits) in [(30, 2), (21, 9)] {
        a.ubfx(Size::X, t6, far, shift, bits);
        a.add_shift(Size::X, leaf, leaf, t6, Shift::Lsl, 3);
        a.ldr(Width::X, t7, leaf, 0);
        a.tbz(t7, 0, fail);
        logic(a, Logic::And, Size::X, t7, t7, 0x0000_ffff_ffff_f000);
        a.mov_imm(Size::X, t6, delta);
        a.add(Size::X, leaf, t7, t6);
    }
    a.ubfx(Size::X, t6, far, 12, 9);
    a.add_shift(Size::X, leaf, leaf, t6, Shift::Lsl, 3);
    let leaf_free = a.label();
    let was_valid = r(30);
    a.ldr(Width::X, t7, leaf, 0);
    a.ubfx(Size::X, was_valid, t7, 0, 1);
    a.tbz(t7, 0, leaf_free);
    logic(a, Logic::And, Size::W, t6, esr, 0x3f);
    a.cmp_imm(Size::W, t6, 0x07);
    a.b_cond(Cond::Eq, fail);
    a.tbz(t7, crate::shadow::LEAF_RO.trailing_zeros(), fail);
    a.bind(leaf_free);

    a.mov_imm(Size::X, phys_va, layout::PHYS_VA);
    a.ldr(Width::W, ram, STATE, state::RAM_SIZE as i32);
    a.ldr(Width::W, t6, STATE, state::WALK_CR3 as i32);
    a.lsr_imm(Size::W, t7, far, 22);
    a.add_shift(Size::W, t6, t6, t7, Shift::Lsl, 2);
    a.subs(Size::W, ZR, t6, ram);
    a.b_cond(Cond::Hs, fail);
    a.addsub_ext(Size::X, false, false, pde_va, phys_va, t6, Extend::Uxtw, 0);
    a.ldr(Width::W, pde, pde_va, 0);
    a.tbz(pde, 0, fail);
    a.ldr(Width::W, user, STATE, state::USER as i32);
    a.ubfx(Size::X, write, esr, 6, 1);
    a.mov(Size::W, pte, ZR);

    let small = a.label();
    let have_eff = a.label();
    let big = a.label();
    a.tbz(flags, walk::PSE.trailing_zeros(), small);
    a.tbz(pde, 7, small);
    a.mov(Size::W, eff, pde);
    a.b(big);
    a.bind(small);
    logic(a, Logic::And, Size::W, t6, pde, 0xffff_f000);
    a.ubfx(Size::W, t7, far, 12, 10);
    a.add_shift(Size::W, t6, t6, t7, Shift::Lsl, 2);
    a.subs(Size::W, ZR, t6, ram);
    a.b_cond(Cond::Hs, fail);
    a.addsub_ext(Size::X, false, false, pte_va, phys_va, t6, Extend::Uxtw, 0);
    a.ldr(Width::W, pte, pte_va, 0);
    a.tbz(pte, 0, fail);
    a.and(Size::W, eff, pde, pte);
    a.b(have_eff);
    a.bind(big);
    a.bind(have_eff);

    let kernel = a.label();
    let perm_ok = a.label();
    a.cbz(Size::W, user, kernel);
    a.tbz(eff, 2, fail);
    a.cbz(Size::X, write, perm_ok);
    a.tbz(eff, 1, fail);
    a.b(perm_ok);
    a.bind(kernel);
    a.cbz(Size::X, write, perm_ok);
    a.tbz(flags, walk::WP.trailing_zeros(), perm_ok);
    a.tbz(eff, 1, fail);
    a.bind(perm_ok);

    // Accessed (and Dirty on writes) go into the entry that maps the page, as on x86.
    let is_small = a.label();
    let bits_done = a.label();
    a.cbnz(Size::W, pte, is_small);
    logic(a, Logic::And, Size::W, phys, pde, 0xffc0_0000);
    a.ubfx(Size::W, t6, far, 12, 10);
    a.add_shift(Size::W, phys, phys, t6, Shift::Lsl, 12);
    logic(a, Logic::Orr, Size::W, t6, pde, 0x20);
    a.logic_shift(Logic::Orr, false, Size::W, t6, t6, write, Shift::Lsl, 6);
    a.str(Width::W, t6, pde_va, 0);
    a.mov(Size::W, pde, t6);
    a.ubfx(Size::W, t7, pde, 8, 1);
    a.movz(Size::W, t6, fill::BIG, 0);
    a.b(bits_done);
    a.bind(is_small);
    logic(a, Logic::Orr, Size::W, pde, pde, 0x20);
    a.str(Width::W, pde, pde_va, 0);
    logic(a, Logic::Orr, Size::W, pte, pte, 0x20);
    a.logic_shift(Logic::Orr, false, Size::W, pte, pte, write, Shift::Lsl, 6);
    a.str(Width::W, pte, pte_va, 0);
    logic(a, Logic::And, Size::W, phys, pte, 0xffff_f000);
    a.ubfx(Size::W, t7, pte, 8, 1);
    a.mov(Size::W, t6, ZR);
    a.bind(bits_done);
    // t7: x86 G bit, t6: fill flags so far.
    a.ubfx(Size::W, eff, flags, walk::PGE.trailing_zeros(), 1);
    a.and(Size::W, t7, t7, eff);
    a.logic_shift(Logic::Orr, false, Size::W, t6, t6, t7, Shift::Lsl, 1);

    a.subs(Size::W, ZR, phys, ram);
    a.b_cond(Cond::Hs, fail);
    let not_hole = a.label();
    a.mov_imm(Size::W, t7, 0xa_0000);
    a.subs(Size::W, ZR, phys, t7);
    a.b_cond(Cond::Lo, not_hole);
    a.mov_imm(Size::W, t7, 0x10_0000);
    a.subs(Size::W, ZR, phys, t7);
    a.b_cond(Cond::Lo, fail);
    a.bind(not_hole);

    let read_only = a.label();
    let mapped = a.label();
    a.cbz(Size::X, write, read_only);
    a.mov_imm(Size::X, t7, layout::va(layout::CODE_BITMAP));
    a.lsr_imm(Size::W, eff, phys, 15);
    a.add(Size::X, t7, t7, eff);
    a.ldr(Width::B, t7, t7, 0);
    a.ubfx(Size::W, eff, phys, 12, 3);
    a.lsrv(Size::W, t7, t7, eff);
    a.tbnz(t7, 0, fail);
    logic(a, Logic::Orr, Size::W, t6, t6, fill::WRITABLE as u64);
    a.mov_imm(Size::X, t7, crate::shadow::LEAF_ATTRS);
    a.b(mapped);
    a.bind(read_only);
    a.mov_imm(Size::X, t7, crate::shadow::LEAF_ATTRS | crate::shadow::LEAF_RO);
    a.bind(mapped);
    a.orr(Size::X, t7, t7, phys);
    a.ubfx(Size::X, eff, t6, fill::GLOBAL.trailing_zeros(), 1);
    a.logic_shift(Logic::Orr, false, Size::X, t7, t7, eff, Shift::Lsl, crate::shadow::LEAF_GLOBAL.trailing_zeros());
    a.str(Width::X, t7, leaf, 0);

    a.mov_imm(Size::X, t7, layout::va(layout::FILL_LOG));
    a.add_shift(Size::X, t7, t7, count, Shift::Lsl, 5);
    a.mov_imm(Size::X, eff, layout::RUNTIME_VA);
    a.sub(Size::X, eff, leaf, eff);
    a.str(Width::W, eff, t7, 0);
    a.str(Width::W, far, t7, 4);
    a.orr(Size::W, eff, phys, t6);
    a.ldr(Width::W, t6, STATE, state::USER as i32);
    a.logic_shift(Logic::Orr, false, Size::W, eff, eff, t6, Shift::Lsl, 3);
    a.str(Width::W, eff, t7, 8);
    a.str(Width::W, pde, t7, 12);
    a.str(Width::W, pte, t7, 16);
    a.add_imm(Size::W, count, count, 1);
    a.str(Width::W, count, STATE, state::FILL_COUNT as i32);

    // Entries that fault are never in the TLB, so a new leaf needs only the
    // barrier; the ERET after it synchronises the context.
    let (stale, done) = (a.label(), a.label());
    a.cbnz(Size::W, was_valid, stale);
    a.dsb(0xa);
    a.b(done);
    a.bind(stale);
    a.dsb(0xb);
    a.lsr_imm(Size::X, t7, far, 12);
    a.tlbi_vaae1(t7);
    a.dsb(0xb);
    a.isb();
    a.bind(done);
}

/// CPL 0 TPR accesses on `State::apic_va` use `State::tpr`; the rest go to
/// `other` with x0..x17 still saved.
fn tpr_access(a: &mut Asm, other: Label) {
    let (esr, far, t2, t3, srt, val, t6, t7) = (r(0), r(1), r(2), r(3), r(4), r(5), r(6), r(7));
    let logic = |a: &mut Asm, op: Logic, sz: Size, rd: Reg, rn: Reg, imm: u64| {
        let ok = a.logic_imm(op, sz, rd, rn, imm);
        assert!(ok);
    };
    a.ldr(Width::X, t2, STATE, state::APIC_VA as i32);
    a.cbz(Size::X, t2, other);
    a.mrs(far, sysreg::FAR_EL1);
    logic(a, Logic::And, Size::X, t3, far, !0xfff);
    logic(a, Logic::Orr, Size::X, t3, t3, 1);
    a.cmp(Size::X, t3, t2);
    a.b_cond(Cond::Ne, other);
    a.ubfx(Size::X, t3, far, 0, 12);
    a.cmp_imm(Size::W, t3, 0x80);
    a.b_cond(Cond::Ne, other);
    a.ldr(Width::W, t3, STATE, state::USER as i32);
    a.cbnz(Size::W, t3, other);
    a.mrs(esr, sysreg::ESR_EL1);
    a.ubfx(Size::X, t3, esr, 26, 6);
    a.cmp_imm(Size::W, t3, 0x25);
    a.b_cond(Cond::Ne, other);
    // ESR_EL1.ISV is 0 for these stage 1 faults, so read the instruction:
    // only the 32-bit LDR/STR (unsigned offset) that translated code emits.
    a.mrs(t6, sysreg::ELR_EL1);
    a.ldr(Width::W, esr, t6, 0);
    logic(a, Logic::And, Size::W, t3, esr, 0xff80_0000);
    a.mov_imm(Size::W, t7, 0xb900_0000);
    a.cmp(Size::W, t3, t7);
    a.b_cond(Cond::Ne, other);
    a.ubfx(Size::X, srt, esr, 0, 5);

    let (write, done, got, high) = (a.label(), a.label(), a.label(), a.label());
    a.tbz(esr, 22, write);
    a.ldr(Width::W, val, STATE, state::TPR as i32);
    a.cmp_imm(Size::W, srt, 31);
    a.b_cond(Cond::Eq, done);
    a.cmp_imm(Size::W, srt, 18);
    a.b_cond(Cond::Hs, high);
    a.add_shift(Size::X, t6, STATE, srt, Shift::Lsl, 3);
    a.str(Width::X, val, t6, state::SCRATCH as i32);
    a.b(done);
    let table = a.label();
    a.bind(high);
    a.adr(t6, table);
    a.sub_imm(Size::W, t7, srt, 18);
    a.add_shift(Size::X, t6, t6, t7, Shift::Lsl, 3);
    a.br(t6);
    a.bind(table);
    for n in 18..31 {
        a.mov(Size::X, Reg(n), val);
        a.b(done);
    }

    let (write_high, write_table) = (a.label(), a.label());
    a.bind(write);
    a.mov(Size::W, val, ZR);
    a.cmp_imm(Size::W, srt, 31);
    a.b_cond(Cond::Eq, got);
    a.cmp_imm(Size::W, srt, 18);
    a.b_cond(Cond::Hs, write_high);
    a.add_shift(Size::X, t6, STATE, srt, Shift::Lsl, 3);
    a.ldr(Width::X, val, t6, state::SCRATCH as i32);
    a.b(got);
    a.bind(write_high);
    a.adr(t6, write_table);
    a.sub_imm(Size::W, t7, srt, 18);
    a.add_shift(Size::X, t6, t6, t7, Shift::Lsl, 3);
    a.br(t6);
    a.bind(write_table);
    for n in 18..31 {
        a.mov(Size::X, val, Reg(n));
        a.b(got);
    }
    a.bind(got);
    logic(a, Logic::And, Size::W, val, val, 0xff);
    a.str(Width::W, val, STATE, state::TPR as i32);
    a.ldr(Width::W, t6, STATE, state::TPR_THRESHOLD as i32);
    logic(a, Logic::And, Size::W, t7, val, 0xf0);
    a.cmp(Size::W, t7, t6);
    a.b_cond(Cond::Hs, done);
    a.movz(Size::W, t6, 1, 0);
    a.str(Width::W, t6, STATE, state::PENDING as i32);

    a.bind(done);
    a.ldr(Width::X, t6, STATE, state::TPR_FAST as i32);
    a.add_imm(Size::X, t6, t6, 1);
    a.str(Width::X, t6, STATE, state::TPR_FAST as i32);
    a.mrs(t6, sysreg::ELR_EL1);
    a.add_imm(Size::X, t6, t6, 4);
    a.msr(sysreg::ELR_EL1, t6);
    restore_scratch(a);
    a.eret();
}

/// Builds the vectors and runtime code. The returned words are placed at
/// `layout::VECTORS`; `Entries` offsets are relative to the runtime region.
pub fn build() -> (Vec<u32>, Entries) {
    let mut a = Asm::new();
    let fault = a.label();
    let unexpected = a.label();
    for slot in 0..16 {
        assert_eq!(a.pos() * 4, slot * 0x80);
        a.b(if slot == 4 { fault } else { unexpected });
        while a.pos() * 4 < (slot + 1) * 0x80 {
            a.brk(slot as u16);
        }
    }
    assert_eq!(a.pos() * 4, layout::RUNTIME_CODE);

    let resume = a.label();
    let dispatch = a.label();
    let exit = a.label();
    let miss = a.label();
    let pending = a.label();

    a.bind(resume);
    let resume_pos = a.pos();
    ctl_maintenance(&mut a);
    load_guest(&mut a);
    a.ldr(Width::W, r(0), STATE, state::EIP as i32);

    a.bind(dispatch);
    let dispatch_pos = a.pos();
    a.ldr(Width::W, r(1), STATE, state::PENDING as i32);
    a.cbnz(Size::W, r(1), pending);
    let no_irq = a.label();
    a.ldr(Width::W, r(1), STATE, state::IRQ as i32);
    a.cbz(Size::W, r(1), no_irq);
    a.ldr(Width::W, r(1), STATE, state::IF_FLAG as i32);
    a.cbnz(Size::W, r(1), pending);
    a.bind(no_irq);
    let unlimited = a.label();
    a.ldr(Width::W, r(1), STATE, state::BUDGET as i32);
    a.cbz(Size::W, r(1), unlimited);
    a.sub_imm(Size::W, r(1), r(1), 1);
    a.str(Width::W, r(1), STATE, state::BUDGET as i32);
    a.cbz(Size::W, r(1), pending);
    a.bind(unlimited);
    // Tags are EIP | user << 32 | global << 33 | epoch << 34, with `GEPOCH`
    // for code on x86-global pages and `VEPOCH` for the rest.
    let (base, entry, tag_v) = (r(6), r(7), r(3));
    let (translate, not_vepoch) = (a.label(), a.label());
    a.ldr(Width::W, base, STATE, state::USER as i32);
    a.logic_shift(Logic::Orr, false, Size::X, base, r(0), base, Shift::Lsl, 32);
    a.ldr(Width::W, entry, STATE, state::VEPOCH as i32);
    a.logic_shift(Logic::Orr, false, Size::X, tag_v, base, entry, Shift::Lsl, 34);
    a.logic_shift(Logic::Eor, false, Size::W, entry, r(0), r(0), Shift::Lsr, 16);
    let ok = a.logic_imm(Logic::And, Size::W, entry, entry, layout::VLOOKUP_ENTRIES as u64 - 1);
    assert!(ok);
    a.ldr(Width::X, r(2), STATE, state::VLOOKUP as i32);
    a.add_shift(Size::X, entry, r(2), entry, Shift::Lsl, 4);
    a.ldp(Size::X, r(4), r(5), entry, 0);
    a.eor(Size::X, r(2), r(4), tag_v);
    a.cbnz(Size::X, r(2), not_vepoch);
    a.br(r(5));
    a.bind(not_vepoch);
    global_tag(&mut a, r(1), base);
    a.eor(Size::X, r(1), r(4), r(1));
    a.cbnz(Size::X, r(1), translate);
    a.br(r(5));
    a.bind(translate);
    let (probe, translated, nonglobal, tagged) = (a.label(), a.label(), a.label(), a.label());
    shadow_leaf(&mut a, probe);
    a.b(translated);
    // The code page is not in the shadow tree: a load from it lets the fault
    // handler fill it, or resume at `miss` when the x86 page tables lack it.
    a.bind(probe);
    a.ldr(Width::B, ZR, r(0), 0);
    shadow_leaf(&mut a, miss);
    a.bind(translated);
    a.tbz(r(1), crate::shadow::LEAF_GLOBAL.trailing_zeros(), nonglobal);
    global_tag(&mut a, base, base);
    a.b(tagged);
    a.bind(nonglobal);
    a.mov(Size::X, base, tag_v);
    a.bind(tagged);
    let ok = a.logic_imm(Logic::And, Size::X, r(1), r(1), 0xffff_f000);
    assert!(ok);
    a.bfxil(Size::X, r(1), r(0), 0, 12);
    a.ldr(Width::W, r(2), STATE, state::USER as i32);
    a.logic_shift(Logic::Orr, false, Size::X, r(1), r(1), r(2), Shift::Lsl, 32);
    a.ubfx(Size::X, r(2), r(0), 12, 20);
    a.logic_shift(Logic::Orr, false, Size::X, r(1), r(1), r(2), Shift::Lsl, 33);
    a.ldr(Width::X, r(2), STATE, state::LOOKUP as i32);
    a.mov_imm(Size::X, r(3), HASH_MUL);
    a.mul(Size::X, r(3), r(1), r(3));
    a.lsr_imm(Size::X, r(3), r(3), 64 - (layout::LOOKUP_ENTRIES.trailing_zeros() - 1));
    a.add_shift(Size::X, r(2), r(2), r(3), Shift::Lsl, 5);
    let second = a.label();
    a.ldp(Size::X, r(4), r(5), r(2), 0);
    a.eor(Size::X, r(4), r(4), r(1));
    a.cbnz(Size::X, r(4), second);
    a.stp(Size::X, base, r(5), entry, 0);
    a.br(r(5));
    a.bind(second);
    a.ldp(Size::X, r(4), r(5), r(2), 16);
    a.eor(Size::X, r(4), r(4), r(1));
    a.cbnz(Size::X, r(4), miss);
    a.stp(Size::X, base, r(5), entry, 0);
    a.br(r(5));

    a.bind(miss);
    a.movz(Size::W, r(1), reason::MISS, 0);
    a.b(exit);
    a.bind(pending);
    a.str(Width::W, ZR, STATE, state::PENDING as i32);
    a.movz(Size::W, r(1), reason::PENDING, 0);

    a.bind(exit);
    let exit_pos = a.pos();
    a.str(Width::W, r(0), STATE, state::EIP as i32);
    a.str(Width::W, r(1), STATE, state::REASON as i32);
    store_guest(&mut a, None);
    a.hvc(0);
    a.b(resume);

    for (label, why) in [(fault, reason::FAULT), (unexpected, reason::UNEXPECTED)] {
        a.bind(label);
        save_scratch(&mut a);
        if why == reason::FAULT {
            let (slow, report, fill) = (a.label(), a.label(), a.label());
            tpr_access(&mut a, fill);
            a.bind(fill);
            try_fill(&mut a, slow);
            restore_scratch(&mut a);
            a.eret();
            a.bind(slow);
            a.mrs(r(2), sysreg::ELR_EL1);
            a.adr(r(3), probe);
            a.cmp(Size::X, r(2), r(3));
            a.b_cond(Cond::Ne, report);
            a.adr(r(2), miss);
            a.msr(sysreg::ELR_EL1, r(2));
            restore_scratch(&mut a);
            a.eret();
            a.bind(report);
        }
        a.mrs(r(0), sysreg::ESR_EL1);
        a.mrs(r(1), sysreg::FAR_EL1);
        a.mrs(r(2), sysreg::ELR_EL1);
        a.mrs(r(3), sysreg::SPSR_EL1);
        a.stp(Size::X, r(0), r(1), STATE, state::INFO as i32);
        a.stp(Size::X, r(2), r(3), STATE, (state::INFO + 16) as i32);
        store_guest(&mut a, Some(r(3)));
        a.movz(Size::W, r(1), why, 0);
        a.str(Width::W, r(1), STATE, state::REASON as i32);
        a.hvc(1);
        ctl_maintenance(&mut a);
        let redispatch = a.label();
        a.ldr(Width::W, r(0), STATE, state::RESUME as i32);
        a.cbnz(Size::W, r(0), redispatch);
        restore_scratch(&mut a);
        a.eret();
        a.bind(redispatch);
        a.adr(r(0), resume);
        a.msr(sysreg::ELR_EL1, r(0));
        a.movz(Size::W, r(0), 0x3c5, 0);
        a.msr(sysreg::SPSR_EL1, r(0));
        a.eret();
    }

    let code = a.finish();
    (code, Entries { resume: resume_pos * 4, dispatch: dispatch_pos * 4, exit: exit_pos * 4 })
}

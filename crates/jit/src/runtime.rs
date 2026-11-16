//! Code that runs at EL1 inside the VM: exception vectors, the dispatcher and
//! the paths that hand control back to the host.

use a64::{Asm, Extend, Logic, Reg, Shift, Size, Width, ZR, r, sysreg};

use crate::layout;
use crate::state::{self, reason};

/// x86 EAX..EDI live in w19..w26 while translated code runs.
pub const fn guest_reg(n: usize) -> Reg {
    Reg(19 + n as u8)
}

pub const PF_RESULT: Reg = Reg(27);
pub const AF_AUX: Reg = Reg(18);
pub const STATE: Reg = Reg(28);
pub const ICOUNT: Reg = Reg(17);

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
    a.tlbi_vae1(r(2));
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
            debug_assert!(ok);
        }
        None => a.mrs(r(9), sysreg::NZCV),
    }
    a.str(Width::W, r(9), STATE, state::NZCV as i32);
    a.str(Width::W, PF_RESULT, STATE, state::PF_RESULT as i32);
    a.str(Width::W, AF_AUX, STATE, state::AF_AUX as i32);
    a.str(Width::X, ICOUNT, STATE, state::ICOUNT as i32);
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
    a.at_s1e1r(r(0));
    a.isb();
    a.mrs(r(1), sysreg::PAR_EL1);
    a.tbnz(r(1), 0, miss);
    a.ubfx(Size::X, r(1), r(1), 12, 36);
    a.lsl_imm(Size::X, r(1), r(1), 12);
    a.bfxil(Size::X, r(1), r(0), 0, 12);
    a.ldr(Width::X, r(2), STATE, state::LOOKUP as i32);
    a.logic_shift(Logic::Eor, false, Size::X, r(3), r(1), r(1), Shift::Lsr, 13);
    a.ubfx(Size::X, r(3), r(3), 0, layout::LOOKUP_ENTRIES.trailing_zeros());
    a.addsub_ext(Size::X, false, false, r(2), r(2), r(3), Extend::Uxtx, 4);
    a.ldp(Size::X, r(4), r(5), r(2), 0);
    a.eor(Size::X, r(4), r(4), r(1));
    a.cbnz(Size::X, r(4), miss);
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

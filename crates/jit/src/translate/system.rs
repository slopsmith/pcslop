//! Kernel entry and exit without leaving the VM: SYSENTER and SYSEXIT switch
//! CPL and the shadow tree themselves (the host catches up in `sync_mode`),
//! FS and GS loads read their descriptor from the GDT, and reading a control
//! register comes from the runtime state. Anything unusual goes to the
//! interpreter, which raises the right fault.

use a64::{Label, Logic, Reg, Shift, Size, Width, ZR, sysreg};
use x86::decode::Insn;

use super::{Emit, Step, T_A, T_ADDR, T_B, T_C, T_D, T_E};
use crate::runtime::{STATE, guest_reg};
use crate::layout;
use crate::state;

const ES: usize = 0;
const DS: usize = 3;
const FS: usize = 4;
const GS: usize = 5;

impl Emit {
    /// SYSENTER: CPL 0 at SYSENTER_EIP with the flat segments SYSENTER_CS implies.
    pub(super) fn sysenter(&mut self, eip: u32) -> Step {
        let slow = self.a.label();
        self.a.ldr(Width::W, T_A, STATE, state::SYSENTER_CS as i32);
        self.and_imm32(T_A, T_A, 0xfffc);
        self.a.cbz(Size::W, T_A, slow);
        self.a.str(Width::W, ZR, STATE, state::IF_FLAG as i32);
        self.a.str(Width::W, T_A, STATE, state::CS_SEL as i32);
        self.a.add_imm(Size::W, T_B, T_A, 8);
        self.a.str(Width::W, T_B, STATE, state::SS_SEL as i32);
        self.a.ldr(Width::W, guest_reg(4), STATE, state::SYSENTER_ESP as i32);
        self.a.ldr(Width::W, T_C, STATE, state::SYSENTER_EIP as i32);
        self.a.ldr(Width::X, T_B, STATE, state::SYSCALLS as i32);
        self.a.add_imm(Size::X, T_B, T_B, 1);
        self.a.str(Width::X, T_B, STATE, state::SYSCALLS as i32);
        self.switch_tree(false);
        self.goto_reg(T_C);
        self.a.bind(slow);
        self.exit_interp(eip);
        Step::End
    }

    /// SYSEXIT from CPL 0: CPL 3 at EDX with ESP = ECX.
    pub(super) fn sysexit(&mut self, eip: u32) -> Step {
        if self.user {
            return Step::Unsupported;
        }
        let slow = self.a.label();
        self.a.ldr(Width::W, T_A, STATE, state::SYSENTER_CS as i32);
        self.and_imm32(T_A, T_A, 0xfffc);
        self.a.cbz(Size::W, T_A, slow);
        self.a.add_imm(Size::W, T_A, T_A, 16 | 3);
        self.a.str(Width::W, T_A, STATE, state::CS_SEL as i32);
        self.a.add_imm(Size::W, T_B, T_A, 8);
        self.a.str(Width::W, T_B, STATE, state::SS_SEL as i32);
        self.a.mov(Size::W, guest_reg(4), guest_reg(1));
        self.a.mov(Size::W, T_C, guest_reg(2));
        self.switch_tree(true);
        self.goto_reg(T_C);
        self.a.bind(slow);
        self.exit_interp(eip);
        Step::End
    }

    fn switch_tree(&mut self, user: bool) {
        let r = user as i32;
        self.a.movz(Size::W, T_B, r as u32, 0);
        self.a.str(Width::W, T_B, STATE, state::USER as i32);
        self.a.ldr(Width::X, T_B, STATE, state::TTBR0 as i32 + 8 * r);
        self.a.msr(sysreg::TTBR0_EL1, T_B);
        self.a.isb();
        self.a.ldr(Width::X, T_B, STATE, state::ROOTS as i32 + 8 * r);
        self.a.str(Width::X, T_B, STATE, state::SHADOW_ROOT as i32);
    }

    /// POP to a segment register (07, 1F, 0F A1, 0F A9).
    pub(super) fn pop_seg(&mut self, insn: &Insn, eip: u32, seg: usize) -> bool {
        if !insn.op32 {
            return false;
        }
        let (slow, done) = (self.a.label(), self.a.label());
        let esp = guest_reg(4);
        self.a.mov(Size::W, T_ADDR, esp);
        self.load(Width::H, T_A);
        self.load_seg(seg, slow);
        self.a.add_imm(Size::W, esp, esp, 4);
        self.a.b(done);
        self.a.bind(slow);
        self.exit_interp(eip);
        self.a.bind(done);
        true
    }

    /// MOV Sreg, r/m16 (8E) for ES, DS, FS and GS.
    pub(super) fn mov_to_seg(&mut self, insn: &Insn, eip: u32) -> bool {
        let seg = insn.reg();
        if !matches!(seg, ES | DS | FS | GS) {
            return false;
        }
        match insn.mem {
            Some(m) => {
                if !self.ea(insn, &m) {
                    return false;
                }
                self.load(Width::H, T_A);
            }
            None => self.a.uxth(T_A, guest_reg(insn.rm())),
        }
        let (slow, done) = (self.a.label(), self.a.label());
        self.load_seg(seg, slow);
        self.a.b(done);
        self.a.bind(slow);
        self.exit_interp(eip);
        self.a.bind(done);
        true
    }

    /// Loads `seg` with the selector in T_A or branches to `slow`: ES and DS keep
    /// their selector; FS and GS take a present, accessed GDT data descriptor.
    fn load_seg(&mut self, seg: usize, slow: Label) {
        if matches!(seg, ES | DS) {
            let at = if seg == ES { state::ES_SEL } else { state::DS_SEL };
            self.a.ldr(Width::W, T_B, STATE, at as i32);
            self.a.eor(Size::W, T_B, T_B, T_A);
            self.a.cbnz(Size::W, T_B, slow);
            return;
        }
        let (base_at, sel_at) = if seg == FS { (state::FS_BASE, state::FS_SEL) } else { (state::GS_BASE, state::GS_SEL) };
        let (null, store) = (self.a.label(), self.a.label());
        self.and_imm32(T_B, T_A, 0xfffc);
        self.a.cbz(Size::W, T_B, null);
        if self.user {
            self.a.b(slow);
        } else {
            self.a.tbnz(T_A, 2, slow);
            self.and_imm32(T_C, T_A, 0xfff8);
            self.a.add_imm(Size::W, T_E, T_C, 7);
            self.a.ldr(Width::W, T_D, STATE, state::GDT_LIMIT as i32);
            self.a.sub(Size::W, T_D, T_D, T_E);
            self.a.tbnz(T_D, 31, slow);
            self.a.ldr(Width::W, T_D, STATE, state::GDT_BASE as i32);
            self.a.add(Size::W, T_ADDR, T_D, T_C);
            self.a.ldr(Width::X, T_D, T_ADDR, 0);
            self.a.ubfx(Size::X, T_E, T_D, 40, 8);
            self.a.mov_imm(Size::W, T_B, 0x98);
            self.a.and(Size::W, T_E, T_E, T_B);
            self.a.mov_imm(Size::W, T_B, 0x90);
            self.a.eor(Size::W, T_E, T_E, T_B);
            self.a.cbnz(Size::W, T_E, slow);
            // Loading a descriptor sets its Accessed bit, as on x86.
            let accessed = self.a.label();
            self.a.tbnz(T_D, 40, accessed);
            let ok = self.a.logic_imm(Logic::Orr, Size::X, T_D, T_D, 1 << 40);
            assert!(ok);
            self.a.str(Width::X, T_D, T_ADDR, 0);
            self.a.bind(accessed);
            self.a.ubfx(Size::X, T_E, T_D, 45, 2);
            self.and_imm32(T_B, T_A, 3);
            self.a.sub(Size::W, T_E, T_E, T_B);
            self.a.tbnz(T_E, 31, slow);
            self.a.ubfx(Size::X, T_B, T_D, 16, 24);
            self.a.lsr_imm(Size::X, T_E, T_D, 56);
            self.a.logic_shift(Logic::Orr, false, Size::W, T_B, T_B, T_E, Shift::Lsl, 24);
            self.a.b(store);
        }
        self.a.bind(null);
        self.a.movz(Size::W, T_B, 0, 0);
        self.a.bind(store);
        self.a.str(Width::W, T_B, STATE, base_at as i32);
        self.a.str(Width::W, T_A, STATE, sel_at as i32);
    }

    /// MOV CR0, r32 (`src`) changing only TS, MP and EM, or CLTS, at CPL 0: XP
    /// and Linux switch lazy FPU state with them on thread switches.
    pub(super) fn write_cr0_ts(&mut self, src: Option<Reg>, eip: u32) -> bool {
        use x86::cpu::{cr0, cr4};
        if self.user {
            return false;
        }
        self.leave_x87_fp();
        let (slow, done) = (self.a.label(), self.a.label());
        self.a.ldr(Width::W, T_A, STATE, state::CR0 as i32);
        match src {
            Some(r) => {
                let ok = self.a.logic_imm(Logic::Orr, Size::W, T_B, r, cr0::ET as u64);
                assert!(ok);
            }
            None => self.and_imm32(T_B, T_A, !cr0::TS as u64),
        }
        self.a.eor(Size::W, T_C, T_A, T_B);
        self.and_imm32(T_C, T_C, !(cr0::TS | cr0::MP | cr0::EM) as u64);
        self.a.cbnz(Size::W, T_C, slow);
        self.a.str(Width::W, T_B, STATE, state::CR0 as i32);
        // The "may run here" flags, as `Jit::load_state` derives them.
        let is_zero = |e: &mut Self, r: Reg| {
            e.a.sub_imm(Size::W, r, r, 1);
            e.a.lsr_imm(Size::W, r, r, 31);
        };
        self.and_imm32(T_E, T_B, (cr0::TS | cr0::EM) as u64);
        is_zero(self, T_E);
        self.a.ldr(Width::W, T_C, STATE, state::FSW as i32);
        self.a.ubfx(Size::W, T_C, T_C, x86::fpu::sw::ES.trailing_zeros(), 1);
        self.a.bic(Size::W, T_D, T_E, T_C);
        self.a.str(Width::W, T_D, STATE, state::X87_BASE as i32);
        self.a.str(Width::W, T_D, STATE, state::MMX_OK as i32);
        self.a.ldr(Width::W, T_C, STATE, state::FCW as i32);
        self.and_imm32(T_C, T_C, 0x3f);
        let ok = self.a.logic_imm(Logic::Eor, Size::W, T_C, T_C, 0x3f);
        assert!(ok);
        is_zero(self, T_C);
        self.a.and(Size::W, T_C, T_C, T_D);
        self.a.str(Width::W, T_C, STATE, state::X87_OK as i32);
        self.a.ldr(Width::W, T_C, STATE, (state::CR0 + 12) as i32);
        self.a.ubfx(Size::W, T_C, T_C, cr4::OSFXSR.trailing_zeros(), 1);
        self.a.and(Size::W, T_C, T_C, T_E);
        self.a.str(Width::W, T_C, STATE, state::SSE_OK as i32);
        self.a.b(done);
        self.a.bind(slow);
        self.exit_interp(eip);
        self.a.bind(done);
        true
    }

    /// MOV CR3, r32 at CPL 0 with CR4.PGE set: clears the non-global shadow
    /// leaves recorded in `layout::NONGLOBAL` instead of exiting.
    pub(super) fn write_cr3(&mut self, src: Reg, eip: u32) -> bool {
        use x86::cpu::cr4;
        if self.user {
            return false;
        }
        let (slow, done, apic_ok) = (self.a.label(), self.a.label(), self.a.label());
        self.a.ldr(Width::W, T_A, STATE, (state::CR0 + 12) as i32);
        self.a.tbz(T_A, cr4::PGE.trailing_zeros(), slow);
        self.a.mov_imm(Size::W, T_D, layout::NONGLOBAL_ENTRIES as u64);
        for r in 0..2 {
            self.a.ldr(Width::W, T_B, STATE, (state::NG_COUNT + 4 * r) as i32);
            self.a.sub(Size::W, T_E, T_D, T_B);
            self.a.tbnz(T_E, 31, slow);
        }
        self.a.ldr(Width::X, T_A, STATE, state::APIC_VA as i32);
        self.a.cbz(Size::X, T_A, apic_ok);
        self.a.ldr(Width::W, T_A, STATE, state::APIC_GLOBAL as i32);
        self.a.cbz(Size::W, T_A, slow);
        self.a.bind(apic_ok);
        self.a.str(Width::W, src, STATE, (state::CR0 + 8) as i32);
        self.and_imm32(T_A, src, 0xffff_f000);
        self.a.str(Width::W, T_A, STATE, state::WALK_CR3 as i32);
        self.a.mov_imm(Size::X, T_D, layout::RUNTIME_VA);
        for r in 0..2 {
            let (top, empty) = (self.a.label(), self.a.label());
            self.a.ldr(Width::W, T_B, STATE, (state::NG_COUNT + 4 * r) as i32);
            self.a.mov_imm(Size::X, T_C, layout::va(layout::NONGLOBAL + r * layout::NONGLOBAL_ENTRIES * 4));
            self.a.bind(top);
            self.a.cbz(Size::W, T_B, empty);
            self.a.sub_imm(Size::W, T_B, T_B, 1);
            self.a.ldst_reg(true, Width::W, false, false, T_A, T_C, T_B, a64::Extend::Uxtw, true);
            self.a.ldst_reg(false, Width::X, false, false, ZR, T_D, T_A, a64::Extend::Uxtw, false);
            self.a.b(top);
            self.a.bind(empty);
            self.a.str(Width::W, ZR, STATE, (state::NG_COUNT + 4 * r) as i32);
        }
        self.a.dsb(0xb);
        self.a.tlbi_vmalle1();
        self.a.dsb(0xb);
        self.a.isb();
        self.a.ldr(Width::W, T_A, STATE, state::VEPOCH as i32);
        self.a.add_imm(Size::W, T_A, T_A, 1);
        self.a.str(Width::W, T_A, STATE, state::VEPOCH as i32);
        self.a.ldr(Width::W, T_A, STATE, state::CR3_SWITCHED as i32);
        self.a.add_imm(Size::W, T_A, T_A, 1);
        self.a.str(Width::W, T_A, STATE, state::CR3_SWITCHED as i32);
        self.a.b(done);
        self.a.bind(slow);
        self.exit_interp(eip);
        self.a.bind(done);
        true
    }

    /// INVLPG at CPL 0: drops the page's leaf from both shadow trees, as the
    /// host's TLB flush would, unless it maps the APIC.
    pub(super) fn invlpg(&mut self, insn: &Insn, eip: u32) -> bool {
        let Some(m) = insn.mem else { return false };
        if self.user || !self.ea(insn, &m) {
            return false;
        }
        let (slow, done, apic_ok) = (self.a.label(), self.a.label(), self.a.label());
        self.a.ldr(Width::X, T_B, STATE, state::APIC_VA as i32);
        self.a.cbz(Size::X, T_B, apic_ok);
        self.and_imm32(T_C, T_ADDR, 0xffff_f000);
        let ok = self.a.logic_imm(Logic::Orr, Size::W, T_C, T_C, 1);
        assert!(ok);
        self.a.eor(Size::X, T_C, T_C, T_B);
        self.a.cbz(Size::X, T_C, slow);
        self.a.bind(apic_ok);
        self.a.mov_imm(Size::X, T_E, layout::RUNTIME_VA.wrapping_sub(layout::RUNTIME_IPA));
        for r in 0..2 {
            let absent = self.a.label();
            self.a.ldr(Width::X, T_B, STATE, (state::ROOTS + 8 * r) as i32);
            for (shift, bits) in [(30, 2), (21, 9)] {
                self.a.ubfx(Size::X, T_C, T_ADDR, shift, bits);
                self.a.add_shift(Size::X, T_B, T_B, T_C, Shift::Lsl, 3);
                self.a.ldr(Width::X, T_C, T_B, 0);
                self.a.tbz(T_C, 0, absent);
                let ok = self.a.logic_imm(Logic::And, Size::X, T_C, T_C, 0x0000_ffff_ffff_f000);
                assert!(ok);
                self.a.add(Size::X, T_B, T_C, T_E);
            }
            self.a.ubfx(Size::X, T_C, T_ADDR, 12, 9);
            self.a.add_shift(Size::X, T_B, T_B, T_C, Shift::Lsl, 3);
            self.a.str(Width::X, ZR, T_B, 0);
            self.a.bind(absent);
        }
        self.a.dsb(0xb);
        self.a.lsr_imm(Size::W, T_C, T_ADDR, 12);
        self.a.tlbi_vaae1(T_C);
        self.a.dsb(0xb);
        self.a.isb();
        for at in [state::VEPOCH, state::GEPOCH, state::INVLPGS] {
            self.a.ldr(Width::W, T_C, STATE, at as i32);
            self.a.add_imm(Size::W, T_C, T_C, 1);
            self.a.str(Width::W, T_C, STATE, at as i32);
        }
        self.a.b(done);
        self.a.bind(slow);
        self.exit_interp(eip);
        self.a.bind(done);
        true
    }

    /// MOV r32, CR0/CR2/CR3/CR4 (0F 20) at CPL 0.
    pub(super) fn mov_from_cr(&mut self, insn: &Insn) -> bool {
        let cr = insn.reg();
        if self.user || !matches!(cr, 0 | 2 | 3 | 4) {
            return false;
        }
        let at = state::CR0 + 4 * if cr == 0 { 0 } else { cr - 1 };
        self.a.ldr(Width::W, guest_reg(insn.rm()), STATE, at as i32);
        true
    }

    /// 32-bit IRET at CPL 0, to the same CS or to XP's flat CPL 3 segments.
    pub(super) fn iret(&mut self, insn: &Insn, eip: u32) -> Step {
        use x86::cpu::flags;
        if self.user || !insn.op32 {
            return Step::Unsupported;
        }
        let (slow, outer) = (self.a.label(), self.a.label());
        let esp = guest_reg(4);
        let (new_esp, ss, base) = (Reg(2), Reg(3), Reg(4));
        self.a.ldr(Width::W, T_B, STATE, state::EFLAGS_REST as i32);
        self.a.tbnz(T_B, flags::NT.trailing_zeros(), slow);
        self.a.mov(Size::W, T_ADDR, esp);
        self.load(Width::W, T_E);
        self.a.add_imm(Size::W, T_ADDR, esp, 4);
        self.load(Width::H, T_D);
        self.a.add_imm(Size::W, T_ADDR, esp, 8);
        self.load(Width::W, T_A);
        self.a.tbnz(T_A, flags::VM.trailing_zeros(), slow);
        self.and_imm32(T_C, T_D, 3);
        self.a.cbnz(Size::W, T_C, outer);
        self.a.ldr(Width::W, T_C, STATE, state::CS_SEL as i32);
        self.a.eor(Size::W, T_C, T_C, T_D);
        self.a.cbnz(Size::W, T_C, slow);
        self.load_eflags(slow, true);
        self.a.add_imm(Size::W, esp, esp, 12);
        self.goto_reg(T_E);

        self.a.bind(outer);
        let ok = self.a.logic_imm(Logic::Eor, Size::W, T_C, T_C, 3);
        assert!(ok);
        self.a.cbnz(Size::W, T_C, slow);
        self.a.ldr(Width::W, base, STATE, state::SYSENTER_CS as i32);
        self.and_imm32(base, base, 0xfffc);
        self.a.cbz(Size::W, base, slow);
        self.a.add_imm(Size::W, T_C, base, 16 | 3);
        self.a.eor(Size::W, T_C, T_C, T_D);
        self.a.cbnz(Size::W, T_C, slow);
        self.a.add_imm(Size::W, T_ADDR, esp, 12);
        self.load(Width::W, new_esp);
        self.a.add_imm(Size::W, T_ADDR, esp, 16);
        self.load(Width::H, ss);
        self.a.add_imm(Size::W, base, base, 24 | 3);
        for at in [None, Some(state::DS_SEL), Some(state::ES_SEL)] {
            match at {
                None => self.a.eor(Size::W, T_C, base, ss),
                Some(at) => {
                    self.a.ldr(Width::W, T_C, STATE, at as i32);
                    self.a.eor(Size::W, T_C, T_C, base);
                }
            }
            self.a.cbnz(Size::W, T_C, slow);
        }
        for at in [state::FS_SEL, state::GS_SEL] {
            let fine = self.a.label();
            self.a.ldr(Width::W, T_B, STATE, at as i32);
            self.and_imm32(T_C, T_B, 3);
            let ok = self.a.logic_imm(Logic::Eor, Size::W, T_C, T_C, 3);
            assert!(ok);
            self.a.cbz(Size::W, T_C, fine);
            self.and_imm32(T_C, T_B, 0xfffc);
            self.a.cbnz(Size::W, T_C, slow);
            self.a.bind(fine);
        }
        self.load_eflags(slow, true);
        self.a.mov(Size::W, esp, new_esp);
        self.a.str(Width::W, T_D, STATE, state::CS_SEL as i32);
        self.a.str(Width::W, base, STATE, state::SS_SEL as i32);
        self.switch_tree(true);
        self.goto_reg(T_E);
        self.a.bind(slow);
        self.exit_interp(eip);
        Step::End
    }

    /// MOV to or from DR0..DR3, DR6, DR7 at CPL 0; they are plain storage here.
    pub(super) fn mov_dr(&mut self, insn: &Insn, to_dr: bool) -> bool {
        let n = insn.reg();
        if self.user || matches!(n, 4 | 5) {
            return false;
        }
        let at = (state::DR + 4 * n) as i32;
        if to_dr {
            self.a.str(Width::W, guest_reg(insn.rm()), STATE, at);
        } else {
            self.a.ldr(Width::W, guest_reg(insn.rm()), STATE, at);
        }
        true
    }

    /// LLDT with a null selector at CPL 0, which is all XP does on a thread switch.
    pub(super) fn lldt(&mut self, insn: &Insn, eip: u32) -> bool {
        if self.user {
            return false;
        }
        match insn.mem {
            Some(m) => {
                if !self.ea(insn, &m) {
                    return false;
                }
                self.load(Width::H, T_A);
            }
            None => self.a.uxth(T_A, guest_reg(insn.rm())),
        }
        let (slow, done) = (self.a.label(), self.a.label());
        self.and_imm32(T_B, T_A, 0xfffc);
        self.a.cbnz(Size::W, T_B, slow);
        self.a.str(Width::W, T_A, STATE, state::LDTR_SEL as i32);
        self.a.movz(Size::W, T_B, 1, 0);
        self.a.str(Width::W, T_B, STATE, state::LDTR_NULLED as i32);
        self.a.b(done);
        self.a.bind(slow);
        self.exit_interp(eip);
        self.a.bind(done);
        true
    }

    fn and_imm32(&mut self, rd: Reg, rn: Reg, imm: u64) {
        let ok = self.a.logic_imm(Logic::And, Size::W, rd, rn, imm);
        assert!(ok);
    }
}

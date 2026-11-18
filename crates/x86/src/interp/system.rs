//! Protected-mode machinery: descriptors, segment loads, interrupt delivery
//! and far control transfers.

use super::{Ctx, Exception, Res, Source, Trap, gp, vector};
use crate::bus::Bus;
use crate::cpu::{CS, DS, ES, FS, GS, SS, Segment, attr, cr0, cr4, flags};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Descriptor {
    pub lo: u32,
    pub hi: u32,
}

impl Descriptor {
    pub fn attr(&self) -> u16 {
        ((self.hi >> 8) & 0xf0ff) as u16
    }

    pub fn base(&self) -> u32 {
        (self.lo >> 16) | (self.hi & 0xff) << 16 | (self.hi & 0xff00_0000)
    }

    pub fn limit(&self) -> u32 {
        let raw = (self.lo & 0xffff) | (self.hi & 0x000f_0000);
        if self.hi & (1 << 23) != 0 { raw << 12 | 0xfff } else { raw }
    }

    pub fn present(&self) -> bool {
        self.hi & (1 << 15) != 0
    }

    pub fn dpl(&self) -> u8 {
        ((self.hi >> 13) & 3) as u8
    }

    pub fn system(&self) -> bool {
        self.hi & (1 << 12) == 0
    }

    pub fn typ(&self) -> u8 {
        ((self.hi >> 8) & 0xf) as u8
    }

    pub fn is_code(&self) -> bool {
        !self.system() && self.typ() & 8 != 0
    }

    pub fn conforming(&self) -> bool {
        self.is_code() && self.typ() & 4 != 0
    }

    pub fn readable(&self) -> bool {
        !self.system() && (self.typ() & 8 == 0 || self.typ() & 2 != 0)
    }

    pub fn writable_data(&self) -> bool {
        !self.system() && self.typ() & 0xa == 2
    }

    pub fn segment(&self, selector: u16) -> Segment {
        Segment { selector, attr: self.attr(), base: self.base(), limit: self.limit() }
    }

    /// Gate target offset (bits 0..=15 and 48..=63).
    pub fn gate_offset(&self) -> u32 {
        (self.lo & 0xffff) | (self.hi & 0xffff_0000)
    }

    pub fn gate_selector(&self) -> u16 {
        (self.lo >> 16) as u16
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskSwitch {
    /// Through a task gate in the IDT, saving this EIP in the old task.
    Gate(u32),
    /// IRET with NT set, back to the task in the TSS back link.
    Iret,
}

fn ts<T>(sel: u16) -> Res<T> {
    Err(Exception::with_error(vector::TS, sel_error(sel)).into())
}

fn sel_error(sel: u16) -> u32 {
    (sel & !3) as u32
}

impl<B: Bus> Ctx<'_, B> {
    /// Reads the descriptor for `sel`; faults with `#GP(sel)` when it lies outside the table.
    pub fn descriptor(&mut self, sel: u16) -> Res<(Descriptor, u32)> {
        let (base, limit) = if sel & 4 != 0 {
            if !self.cpu.ldtr.present() {
                return gp(sel_error(sel));
            }
            (self.cpu.ldtr.base, self.cpu.ldtr.limit)
        } else {
            (self.cpu.gdtr.base, self.cpu.gdtr.limit as u32)
        };
        let off = (sel & !7) as u32;
        if off + 7 > limit {
            return gp(sel_error(sel));
        }
        let addr = base.wrapping_add(off);
        let lo = self.read_sys(addr, 4)?;
        let hi = self.read_sys(addr.wrapping_add(4), 4)?;
        Ok((Descriptor { lo, hi }, addr))
    }

    fn set_accessed(&mut self, d: &mut Descriptor, addr: u32) -> Res<()> {
        if d.hi & (1 << 8) == 0 {
            d.hi |= 1 << 8;
            self.write_sys(addr.wrapping_add(4), 4, d.hi)?;
        }
        Ok(())
    }

    /// MOV/POP/LxS to a data or stack segment register.
    pub fn load_seg(&mut self, seg: usize, sel: u16) -> Res<()> {
        if !self.cpu.protected_mode() || self.cpu.v86_mode() {
            let v86 = self.cpu.v86_mode();
            let s = &mut self.cpu.segs[seg];
            s.selector = sel;
            s.base = (sel as u32) << 4;
            if v86 {
                *s = Segment::real(sel);
                s.attr |= 3 << 5;
                if seg == CS {
                    s.attr |= attr::CODE;
                }
            }
            if seg == SS {
                self.cpu.interrupt_shadow = true;
            }
            return Ok(());
        }
        debug_assert!(seg != CS);
        let cpl = self.cpu.cpl();
        if sel & !3 == 0 {
            if seg == SS {
                return gp(0);
            }
            self.cpu.segs[seg] = Segment { selector: sel, ..Default::default() };
            return Ok(());
        }
        let (mut d, addr) = self.descriptor(sel)?;
        let rpl = (sel & 3) as u8;
        if seg == SS {
            if rpl != cpl || d.dpl() != cpl || !d.writable_data() {
                return gp(sel_error(sel));
            }
            if !d.present() {
                return Err(Exception::with_error(vector::SS, sel_error(sel)).into());
            }
            self.cpu.interrupt_shadow = true;
        } else {
            if !d.readable() {
                return gp(sel_error(sel));
            }
            if !d.conforming() && (rpl > d.dpl() || cpl > d.dpl()) {
                return gp(sel_error(sel));
            }
            if !d.present() {
                return Err(Exception::with_error(vector::NP, sel_error(sel)).into());
            }
        }
        self.set_accessed(&mut d, addr)?;
        self.cpu.segs[seg] = d.segment(sel);
        Ok(())
    }

    fn load_cs_checked(&mut self, sel: u16, d: &mut Descriptor, addr: u32, new_cpl: u8) -> Res<()> {
        self.set_accessed(d, addr)?;
        let mut s = d.segment((sel & !3) | new_cpl as u16);
        s.attr |= attr::ACCESSED;
        self.cpu.segs[CS] = s;
        Ok(())
    }

    /// Validates a code-segment target for JMP/CALL far within the same privilege level.
    fn code_target(&mut self, sel: u16) -> Res<(Descriptor, u32)> {
        if sel & !3 == 0 {
            return gp(0);
        }
        let (d, addr) = self.descriptor(sel)?;
        let cpl = self.cpu.cpl();
        if !d.is_code() {
            if d.system() {
                return Err(Trap::Unimplemented(format!("far transfer through system descriptor type {}", d.typ())));
            }
            return gp(sel_error(sel));
        }
        let rpl = (sel & 3) as u8;
        let ok = if d.conforming() { d.dpl() <= cpl } else { rpl <= cpl && d.dpl() == cpl };
        if !ok {
            return gp(sel_error(sel));
        }
        if !d.present() {
            return Err(Exception::with_error(vector::NP, sel_error(sel)).into());
        }
        Ok((d, addr))
    }

    pub fn jmp_far(&mut self, sel: u16, eip: u32) -> Res<()> {
        if !self.cpu.protected_mode() || self.cpu.v86_mode() {
            self.load_cs_real(sel);
            self.cpu.eip = eip;
            return Ok(());
        }
        let (mut d, addr) = self.code_target(sel)?;
        if eip > d.limit() {
            return gp(0);
        }
        let cpl = self.cpu.cpl();
        self.load_cs_checked(sel, &mut d, addr, cpl)?;
        self.cpu.eip = eip;
        Ok(())
    }

    pub fn call_far(&mut self, sel: u16, eip: u32, sz: u32) -> Res<()> {
        let old_cs = self.cpu.segs[CS].selector as u32;
        let ret = self.cpu.eip;
        if !self.cpu.protected_mode() || self.cpu.v86_mode() {
            self.push(old_cs, sz)?;
            self.push(ret, sz)?;
            self.load_cs_real(sel);
            self.cpu.eip = eip;
            return Ok(());
        }
        let (mut d, addr) = self.code_target(sel)?;
        if eip > d.limit() {
            return gp(0);
        }
        let saved_sp = self.cpu.regs[4];
        if let Err(e) = self.push(old_cs, sz).and_then(|_| self.push(ret, sz)) {
            self.cpu.regs[4] = saved_sp;
            return Err(e);
        }
        let cpl = self.cpu.cpl();
        self.load_cs_checked(sel, &mut d, addr, cpl)?;
        self.cpu.eip = eip;
        Ok(())
    }

    pub fn load_cs_real(&mut self, sel: u16) {
        let s = &mut self.cpu.segs[CS];
        s.selector = sel;
        s.base = (sel as u32) << 4;
        if self.cpu.eflags & flags::VM != 0 {
            *s = Segment::real(sel);
            s.attr |= attr::CODE | 3 << 5;
        }
    }

    /// RETF with an optional extra stack release of `imm` bytes.
    pub fn ret_far(&mut self, sz: u32, imm: u32) -> Res<()> {
        let eip = self.peek(0, sz)?;
        let sel = self.peek(sz, sz)? as u16;
        if !self.cpu.protected_mode() || self.cpu.v86_mode() {
            if eip > self.cpu.segs[CS].limit {
                return gp(0);
            }
            self.set_sp(self.sp().wrapping_add(2 * sz + imm));
            self.load_cs_real(sel);
            self.cpu.eip = if sz == 2 { eip & 0xffff } else { eip };
            return Ok(());
        }
        let cpl = self.cpu.cpl();
        let rpl = (sel & 3) as u8;
        if sel & !3 == 0 || rpl < cpl {
            return gp(sel_error(sel));
        }
        let (mut d, addr) = self.descriptor(sel)?;
        if !d.is_code() || (d.conforming() && d.dpl() > rpl) || (!d.conforming() && d.dpl() != rpl) {
            return gp(sel_error(sel));
        }
        if !d.present() {
            return Err(Exception::with_error(vector::NP, sel_error(sel)).into());
        }
        if eip > d.limit() {
            return gp(0);
        }
        if rpl == cpl {
            self.set_sp(self.sp().wrapping_add(2 * sz + imm));
            self.load_cs_checked(sel, &mut d, addr, cpl)?;
            self.cpu.eip = eip;
            return Ok(());
        }
        let esp = self.peek(2 * sz + imm, sz)?;
        let ss = self.peek(3 * sz + imm, sz)? as u16;
        self.load_cs_checked(sel, &mut d, addr, rpl)?;
        self.cpu.eip = eip;
        self.load_ss_for_cpl(ss, rpl)?;
        self.set_sp(esp.wrapping_add(imm));
        self.invalidate_data_segs(rpl);
        Ok(())
    }

    fn load_ss_for_cpl(&mut self, sel: u16, cpl: u8) -> Res<()> {
        if sel & !3 == 0 {
            return gp(0);
        }
        let (mut d, addr) = self.descriptor(sel)?;
        if (sel & 3) as u8 != cpl || d.dpl() != cpl || !d.writable_data() {
            return gp(sel_error(sel));
        }
        if !d.present() {
            return Err(Exception::with_error(vector::SS, sel_error(sel)).into());
        }
        self.set_accessed(&mut d, addr)?;
        self.cpu.segs[SS] = d.segment(sel);
        Ok(())
    }

    fn invalidate_data_segs(&mut self, cpl: u8) {
        for seg in [ES, DS, FS, GS] {
            let s = self.cpu.segs[seg];
            let data_or_nonconforming = s.attr & attr::CODE == 0 || s.attr & attr::DC == 0;
            if data_or_nonconforming && s.dpl() < cpl {
                self.cpu.segs[seg] = Segment { selector: 0, ..Default::default() };
            }
        }
    }

    pub fn iret(&mut self, sz: u32) -> Res<()> {
        if !self.cpu.protected_mode() {
            let eip = self.peek(0, sz)?;
            let cs = self.peek(sz, sz)? as u16;
            let fl = self.peek(2 * sz, sz)?;
            if eip > self.cpu.segs[CS].limit {
                return gp(0);
            }
            self.set_sp(self.sp().wrapping_add(3 * sz));
            self.load_cs_real(cs);
            self.cpu.eip = eip;
            let writable = if sz == 2 { flags::WRITABLE & 0xffff } else { flags::WRITABLE & !(flags::VM | flags::VIP | flags::VIF) };
            self.cpu.eflags = (self.cpu.eflags & !writable) | (fl & writable) | flags::FIXED1;
            return Ok(());
        }
        if self.cpu.v86_mode() {
            if self.cpu.iopl() < 3 {
                return gp(0);
            }
            let eip = self.peek(0, sz)?;
            let cs = self.peek(sz, sz)? as u16;
            let fl = self.peek(2 * sz, sz)?;
            self.set_sp(self.sp().wrapping_add(3 * sz));
            self.load_cs_real(cs);
            self.cpu.eip = eip;
            let writable = if sz == 2 { 0xffff } else { 0xffff_ffff } & flags::WRITABLE & !(flags::IOPL | flags::VM | flags::VIP | flags::VIF);
            self.cpu.eflags = (self.cpu.eflags & !writable) | (fl & writable) | flags::FIXED1;
            return Ok(());
        }
        if self.cpu.flag(flags::NT) {
            let back = self.read_sys(self.cpu.tr.base, 2)? as u16;
            return self.task_switch(back, TaskSwitch::Iret, None);
        }
        let eip = self.peek(0, sz)?;
        let sel = self.peek(sz, sz)? as u16;
        let fl = self.peek(2 * sz, sz)?;
        let cpl = self.cpu.cpl();
        if sz == 4 && fl & flags::VM != 0 && cpl == 0 {
            return self.iret_to_v86(eip, sel, fl);
        }
        let rpl = (sel & 3) as u8;
        if sel & !3 == 0 || rpl < cpl {
            return gp(sel_error(sel));
        }
        let (mut d, addr) = self.descriptor(sel)?;
        if !d.is_code() || (d.conforming() && d.dpl() > rpl) || (!d.conforming() && d.dpl() != rpl) {
            return gp(sel_error(sel));
        }
        if !d.present() {
            return Err(Exception::with_error(vector::NP, sel_error(sel)).into());
        }
        if eip > d.limit() {
            return gp(0);
        }
        let (esp, ss) = if rpl > cpl {
            (self.peek(3 * sz, sz)?, Some(self.peek(4 * sz, sz)? as u16))
        } else {
            (0, None)
        };
        self.load_cs_checked(sel, &mut d, addr, rpl)?;
        self.cpu.eip = eip;
        self.write_eflags(fl, sz, cpl);
        match ss {
            Some(ss) => {
                self.load_ss_for_cpl(ss, rpl)?;
                self.set_sp(esp);
                self.invalidate_data_segs(rpl);
            }
            None => self.set_sp(self.sp().wrapping_add(3 * sz)),
        }
        Ok(())
    }

    fn iret_to_v86(&mut self, eip: u32, cs: u16, fl: u32) -> Res<()> {
        let esp = self.peek(12, 4)?;
        let ss = self.peek(16, 4)? as u16;
        let es = self.peek(20, 4)? as u16;
        let ds = self.peek(24, 4)? as u16;
        let fs = self.peek(28, 4)? as u16;
        let gs = self.peek(32, 4)? as u16;
        self.cpu.eflags = (fl & flags::WRITABLE) | flags::FIXED1;
        for (seg, sel) in [(CS, cs), (SS, ss), (ES, es), (DS, ds), (FS, fs), (GS, gs)] {
            let mut s = Segment::real(sel);
            s.attr |= 3 << 5;
            if seg == CS {
                s.attr |= attr::CODE;
            }
            self.cpu.segs[seg] = s;
        }
        self.cpu.regs[4] = esp;
        self.cpu.eip = eip & 0xffff;
        Ok(())
    }

    /// POPF/IRET flag update rules for protected mode.
    pub fn write_eflags(&mut self, v: u32, sz: u32, cpl: u8) {
        let mut writable = flags::WRITABLE & !(flags::VM | flags::VIP | flags::VIF);
        if cpl > 0 {
            writable &= !flags::IOPL;
        }
        if cpl > self.cpu.iopl() {
            writable &= !flags::IF;
        }
        if sz == 2 {
            writable &= 0xffff;
        }
        self.cpu.eflags = (self.cpu.eflags & !writable) | (v & writable) | flags::FIXED1;
    }

    /// Delivers an exception or interrupt through the IVT or IDT.
    pub(super) fn raise(&mut self, e: Exception, source: Source) -> Res<()> {
        let ret_eip = if source == Source::Fault { self.start_eip } else { self.cpu.eip };
        let vec = e.vector as u32;
        if !self.cpu.protected_mode() {
            if vec * 4 + 3 > self.cpu.idtr.limit as u32 {
                return gp(0);
            }
            let entry = self.read_lin(self.cpu.idtr.base.wrapping_add(vec * 4), 4, false, super::mem::Access::Read)?;
            let fl = self.cpu.eflags;
            self.push(fl, 2)?;
            self.push(self.cpu.segs[CS].selector as u32, 2)?;
            self.push(ret_eip, 2)?;
            self.cpu.eflags &= !(flags::IF | flags::TF | flags::AC);
            self.load_cs_real((entry >> 16) as u16);
            self.cpu.eip = entry & 0xffff;
            return Ok(());
        }

        let ext = (source != Source::Software) as u32;
        let idt_err = vec << 3 | 2 | ext;
        if vec * 8 + 7 > self.cpu.idtr.limit as u32 {
            return gp(idt_err);
        }
        let base = self.cpu.idtr.base.wrapping_add(vec * 8);
        let gate = Descriptor { lo: self.read_sys(base, 4)?, hi: self.read_sys(base.wrapping_add(4), 4)? };
        let cpl = self.cpu.cpl();
        if source == Source::Software && self.cpu.v86_mode() && self.cpu.iopl() < 3 {
            return gp(0);
        }
        if source == Source::Software && gate.dpl() < cpl {
            return gp(idt_err);
        }
        if !gate.present() {
            return Err(Exception::with_error(vector::NP, idt_err).into());
        }
        let (gate32, trap) = match gate.typ() {
            5 => return self.task_switch(gate.gate_selector(), TaskSwitch::Gate(ret_eip), e.error),
            6 => (false, false),
            7 => (false, true),
            14 => (true, false),
            15 => (true, true),
            _ => return gp(idt_err),
        };
        let sel = gate.gate_selector();
        if sel & !3 == 0 {
            return gp(ext);
        }
        let (mut d, addr) = self.descriptor(sel)?;
        if !d.is_code() || d.dpl() > cpl {
            return gp(sel_error(sel) | ext);
        }
        if !d.present() {
            return Err(Exception::with_error(vector::NP, sel_error(sel) | ext).into());
        }
        let sz = if gate32 { 4 } else { 2 };
        let offset = if gate32 { gate.gate_offset() } else { gate.gate_offset() & 0xffff };
        let old_flags = self.cpu.eflags;
        let old_cs = self.cpu.segs[CS].selector as u32;
        let from_v86 = self.cpu.v86_mode();

        if !d.conforming() && d.dpl() < cpl {
            let new_cpl = d.dpl();
            if from_v86 && new_cpl != 0 {
                return gp(sel_error(sel));
            }
            let (new_ss, new_esp) = self.tss_stack(new_cpl)?;
            let old_ss = self.cpu.segs[SS].selector as u32;
            let old_esp = self.cpu.regs[4];
            let saved = (self.cpu.segs, self.cpu.regs[4], self.cpu.eflags);
            let result = (|| -> Res<()> {
                self.cpu.eflags &= !flags::VM;
                self.load_cs_checked(sel, &mut d, addr, new_cpl)?;
                self.load_ss_for_cpl(new_ss, new_cpl)?;
                self.cpu.regs[4] = new_esp;
                if from_v86 {
                    for seg in [GS, FS, DS, ES] {
                        let s = saved.0[seg].selector as u32;
                        self.push(s, sz)?;
                    }
                }
                self.push(old_ss, sz)?;
                self.push(old_esp, sz)?;
                self.push(old_flags, sz)?;
                self.push(old_cs, sz)?;
                self.push(ret_eip, sz)?;
                if let Some(code) = e.error {
                    self.push(code, sz)?;
                }
                Ok(())
            })();
            if let Err(err) = result {
                self.cpu.segs = saved.0;
                self.cpu.regs[4] = saved.1;
                self.cpu.eflags = saved.2;
                return Err(err);
            }
            if from_v86 {
                for seg in [ES, DS, FS, GS] {
                    self.cpu.segs[seg] = Segment::default();
                }
            }
        } else {
            if from_v86 {
                return gp(sel_error(sel));
            }
            let saved_sp = self.cpu.regs[4];
            let result = (|| -> Res<()> {
                self.push(old_flags, sz)?;
                self.push(old_cs, sz)?;
                self.push(ret_eip, sz)?;
                if let Some(code) = e.error {
                    self.push(code, sz)?;
                }
                Ok(())
            })();
            if let Err(err) = result {
                self.cpu.regs[4] = saved_sp;
                return Err(err);
            }
            self.load_cs_checked(sel, &mut d, addr, cpl)?;
        }
        if offset > self.cpu.segs[CS].limit {
            return gp(0);
        }
        self.cpu.eip = offset;
        self.cpu.eflags &= !(flags::TF | flags::NT | flags::VM | flags::RF);
        if !trap {
            self.cpu.eflags &= !flags::IF;
        }
        Ok(())
    }

    /// Switches to the task whose 32-bit TSS `sel` names, as an interrupt
    /// gate or IRET does (Intel SDM vol. 3A, 7.3), pushing `error` on the new stack.
    fn task_switch(&mut self, sel: u16, how: TaskSwitch, error: Option<u32>) -> Res<()> {
        if sel & 4 != 0 {
            return ts(sel);
        }
        let (mut d, addr) = self.descriptor(sel)?;
        let want = if how == TaskSwitch::Iret { 11 } else { 9 };
        if !d.system() {
            return gp(sel_error(sel));
        }
        if d.typ() & !2 == 1 {
            return Err(Trap::Unimplemented("16-bit task switch".into()));
        }
        if d.typ() != want {
            return if how == TaskSwitch::Iret { ts(sel) } else { gp(sel_error(sel)) };
        }
        if !d.present() {
            return Err(Exception::with_error(vector::NP, sel_error(sel)).into());
        }
        if d.limit() < 0x67 {
            return ts(sel);
        }

        let new = d.base();
        let mut regs = [0u32; 8];
        for (i, r) in regs.iter_mut().enumerate() {
            *r = self.read_sys(new.wrapping_add(0x28 + 4 * i as u32), 4)?;
        }
        let new_cr3 = self.read_sys(new.wrapping_add(0x1c), 4)?;
        let new_eip = self.read_sys(new.wrapping_add(0x20), 4)?;
        let mut new_flags = self.read_sys(new.wrapping_add(0x24), 4)?;
        let mut sels = [0u16; 6];
        for (i, s) in sels.iter_mut().enumerate() {
            *s = self.read_sys(new.wrapping_add(0x48 + 4 * i as u32), 2)? as u16;
        }
        let new_ldt = self.read_sys(new.wrapping_add(0x60), 2)? as u16;

        let old = self.cpu.tr;
        let eip = match how {
            TaskSwitch::Gate(eip) => eip,
            TaskSwitch::Iret => self.cpu.eip,
        };
        let mut old_flags = self.cpu.eflags;
        if how == TaskSwitch::Iret {
            old_flags &= !flags::NT;
        }
        self.write_sys(old.base.wrapping_add(0x20), 4, eip)?;
        self.write_sys(old.base.wrapping_add(0x24), 4, old_flags)?;
        for i in 0..8 {
            self.write_sys(old.base.wrapping_add(0x28 + 4 * i as u32), 4, self.cpu.regs[i])?;
        }
        for i in 0..6 {
            self.write_sys(old.base.wrapping_add(0x48 + 4 * i as u32), 2, self.cpu.segs[i].selector as u32)?;
        }

        if how == TaskSwitch::Iret {
            let (mut od, oaddr) = self.descriptor(old.selector)?;
            od.hi &= !(2 << 8);
            self.write_sys(oaddr.wrapping_add(4), 4, od.hi)?;
        } else {
            self.write_sys(new, 2, old.selector as u32)?;
            new_flags |= flags::NT;
            d.hi |= 2 << 8;
            self.write_sys(addr.wrapping_add(4), 4, d.hi)?;
        }
        self.cpu.tr = d.segment(sel);
        self.cpu.cr0 |= cr0::TS;

        if self.cpu.cr0 & cr0::PG != 0 && new_cr3 != self.cpu.cr3 {
            self.cpu.cr3 = new_cr3;
            if self.cpu.cr4 & cr4::PGE != 0 {
                self.cpu.tlb.flush_nonglobal();
            } else {
                self.cpu.tlb.flush();
            }
        }
        self.cpu.regs = regs;
        self.cpu.eip = new_eip;
        self.cpu.eflags = new_flags | flags::FIXED1;
        if new_flags & flags::VM != 0 {
            for (i, &s) in sels.iter().enumerate() {
                let mut seg = Segment::real(s);
                seg.attr |= 3 << 5;
                if i == CS {
                    seg.attr |= attr::CODE;
                }
                self.cpu.segs[i] = seg;
            }
            return Ok(());
        }

        // Errors while loading the new task's segments surface as #TS in its context.
        if new_ldt & 4 != 0 {
            return ts(new_ldt);
        }
        self.lldt(new_ldt).or_else(|_| ts(new_ldt))?;
        let cpl = (sels[CS] & 3) as u8;
        let (cs_d, _) = self.descriptor(sels[CS]).or_else(|_| ts(sels[CS]))?;
        if sels[CS] & !3 == 0 || !cs_d.is_code() {
            return ts(sels[CS]);
        }
        if !cs_d.present() {
            return Err(Exception::with_error(vector::NP, sel_error(sels[CS])).into());
        }
        self.cpu.segs[CS] = cs_d.segment(sels[CS]);
        let (ss_d, _) = self.descriptor(sels[SS]).or_else(|_| ts(sels[SS]))?;
        if sels[SS] & !3 == 0 || (sels[SS] & 3) as u8 != cpl || ss_d.dpl() != cpl || !ss_d.writable_data() {
            return ts(sels[SS]);
        }
        if !ss_d.present() {
            return Err(Exception::with_error(vector::SS, sel_error(sels[SS])).into());
        }
        self.cpu.segs[SS] = ss_d.segment(sels[SS]);
        for seg in [DS, ES, FS, GS] {
            let s = sels[seg];
            if s & !3 == 0 {
                self.cpu.segs[seg] = Segment { selector: s, ..Default::default() };
                continue;
            }
            let (sd, _) = self.descriptor(s).or_else(|_| ts(s))?;
            if !sd.readable() {
                return ts(s);
            }
            if !sd.present() {
                return Err(Exception::with_error(vector::NP, sel_error(s)).into());
            }
            self.cpu.segs[seg] = sd.segment(s);
        }
        if let Some(code) = error {
            self.push(code, 4)?;
        }
        Ok(())
    }

    fn tss_stack(&mut self, cpl: u8) -> Res<(u16, u32)> {
        let tr = self.cpu.tr;
        match tr.attr & 0xf {
            9 | 11 => {
                let off = 4 + cpl as u32 * 8;
                if off + 5 > tr.limit {
                    return Err(Exception::with_error(vector::TS, sel_error(tr.selector)).into());
                }
                let esp = self.read_sys(tr.base.wrapping_add(off), 4)?;
                let ss = self.read_sys(tr.base.wrapping_add(off + 4), 2)? as u16;
                Ok((ss, esp))
            }
            _ => Err(Trap::Unimplemented("16-bit TSS stack switch".into())),
        }
    }

    pub fn lldt(&mut self, sel: u16) -> Res<()> {
        if sel & !3 == 0 {
            self.cpu.ldtr = Segment { selector: sel, ..Default::default() };
            return Ok(());
        }
        if sel & 4 != 0 {
            return gp(sel_error(sel));
        }
        let (d, _) = self.descriptor(sel)?;
        if !d.system() || d.typ() != 2 {
            return gp(sel_error(sel));
        }
        if !d.present() {
            return Err(Exception::with_error(vector::NP, sel_error(sel)).into());
        }
        self.cpu.ldtr = d.segment(sel);
        Ok(())
    }

    pub fn ltr(&mut self, sel: u16) -> Res<()> {
        if sel & !3 == 0 || sel & 4 != 0 {
            return gp(sel_error(sel));
        }
        let (mut d, addr) = self.descriptor(sel)?;
        if !d.system() || !matches!(d.typ(), 1 | 9) {
            return gp(sel_error(sel));
        }
        if !d.present() {
            return Err(Exception::with_error(vector::NP, sel_error(sel)).into());
        }
        d.hi |= 2 << 8;
        self.write_sys(addr.wrapping_add(4), 4, d.hi)?;
        self.cpu.tr = d.segment(sel);
        Ok(())
    }

    /// SYSENTER (Intel SDM vol. 2B).
    pub fn sysenter(&mut self) -> Res<()> {
        let cs = self.cpu.sysenter_cs as u16;
        if !self.cpu.protected_mode() || cs & !3 == 0 {
            return gp(0);
        }
        self.cpu.eflags &= !(flags::VM | flags::IF | flags::RF);
        self.cpu.segs[CS] = Segment {
            selector: cs & !3,
            attr: attr::P | attr::S | attr::CODE | attr::RW | attr::ACCESSED | attr::DB | attr::G,
            base: 0,
            limit: 0xffff_ffff,
        };
        self.cpu.segs[SS] = Segment {
            selector: (cs & !3) + 8,
            attr: attr::P | attr::S | attr::RW | attr::ACCESSED | attr::DB | attr::G,
            base: 0,
            limit: 0xffff_ffff,
        };
        self.cpu.regs[4] = self.cpu.sysenter_esp;
        self.cpu.eip = self.cpu.sysenter_eip;
        Ok(())
    }

    pub fn sysexit(&mut self) -> Res<()> {
        let cs = self.cpu.sysenter_cs as u16;
        if !self.cpu.protected_mode() || cs & !3 == 0 || self.cpu.cpl() != 0 {
            return gp(0);
        }
        self.cpu.segs[CS] = Segment {
            selector: (cs + 16) | 3,
            attr: attr::P | attr::S | attr::CODE | attr::RW | attr::ACCESSED | attr::DB | attr::G | 3 << 5,
            base: 0,
            limit: 0xffff_ffff,
        };
        self.cpu.segs[SS] = Segment {
            selector: (cs + 24) | 3,
            attr: attr::P | attr::S | attr::RW | attr::ACCESSED | attr::DB | attr::G | 3 << 5,
            base: 0,
            limit: 0xffff_ffff,
        };
        self.cpu.regs[4] = self.cpu.regs[1];
        self.cpu.eip = self.cpu.regs[2];
        Ok(())
    }
}

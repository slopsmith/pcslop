use super::mem::{Loc, mask, sext, sign};
use super::string::StrOp;
use super::{Ctx, Exception, Res, Source, Trap, alu, gp, ud, vector};
use crate::bus::Bus;
use crate::cpu::{CS, DS, EAX, EBP, EBX, ECX, EDX, ES, FS, GS, SS, cr0, cr4, flags};
use crate::decode::{DecodeError, decode};

fn lockable(op: u16, reg: usize) -> bool {
    match op {
        0x00..=0x37 => op & 7 < 2,
        0x80..=0x83 => reg != 7,
        0x86 | 0x87 => true,
        0xf6 | 0xf7 => reg == 2 || reg == 3,
        0xfe | 0xff => reg < 2,
        0x1ab | 0x1b3 | 0x1bb | 0x1b0 | 0x1b1 | 0x1c0 | 0x1c1 => true,
        0x1ba => reg >= 5,
        0x1c7 => reg == 1,
        _ => false,
    }
}

impl<B: Bus> Ctx<'_, B> {
    pub fn set_arith(&mut self, f: u32) {
        self.cpu.eflags = (self.cpu.eflags & !flags::ARITH) | (f & flags::ARITH);
    }

    fn opsz(&self) -> u32 {
        if self.insn.op32 { 4 } else { 2 }
    }

    /// Byte size for even opcodes, operand size for odd ones.
    fn bsz(&self) -> u32 {
        if self.insn.op & 1 == 0 { 1 } else { self.opsz() }
    }

    pub(super) fn cond(&self, cc: u16) -> bool {
        let f = |m| self.cpu.flag(m);
        let r = match cc >> 1 {
            0 => f(flags::OF),
            1 => f(flags::CF),
            2 => f(flags::ZF),
            3 => f(flags::CF) || f(flags::ZF),
            4 => f(flags::SF),
            5 => f(flags::PF),
            6 => f(flags::SF) != f(flags::OF),
            _ => f(flags::ZF) || f(flags::SF) != f(flags::OF),
        };
        r ^ (cc & 1 != 0)
    }

    fn jump_to(&mut self, target: u32) -> Res<()> {
        let target = if self.insn.op32 { target } else { target & 0xffff };
        if target > self.cpu.segs[CS].limit {
            return gp(0);
        }
        self.cpu.eip = target;
        Ok(())
    }

    fn jump_rel(&mut self, rel: u32) -> Res<()> {
        self.jump_to(self.cpu.eip.wrapping_add(rel))
    }

    /// Offset of a later part of a multi-part memory operand; wraps at 64 KiB
    /// with 16-bit addressing.
    pub(super) fn off_add(&self, off: u32, n: u32) -> u32 {
        let v = off.wrapping_add(n);
        if self.insn.addr32 { v } else { v & 0xffff }
    }

    pub(super) fn require_mem(&self) -> Res<(usize, u32)> {
        match self.rm_loc() {
            Loc::Mem(seg, off) => Ok((seg, off)),
            Loc::Reg(_) => ud(),
        }
    }

    fn count_reg(&self) -> u32 {
        if self.insn.addr32 { self.cpu.regs[ECX] } else { self.cpu.regs[ECX] & 0xffff }
    }

    fn set_count_reg(&mut self, v: u32) {
        if self.insn.addr32 {
            self.cpu.regs[ECX] = v;
        } else {
            self.cpu.set_reg16(ECX, v as u16);
        }
    }

    pub(super) fn pm(&self) -> bool {
        self.cpu.protected_mode() && !self.cpu.v86_mode()
    }

    fn require_cpl0(&self) -> Res<()> {
        if self.cpu.protected_mode() && self.cpu.cpl() != 0 { gp(0) } else { Ok(()) }
    }

    pub(super) fn unimplemented<T>(&self) -> Res<T> {
        let i = &self.insn;
        let what = if i.has_modrm {
            format!("opcode {:#05x} /{} (modrm {:#04x})", i.op, i.reg(), i.modrm)
        } else {
            format!("opcode {:#05x}", i.op)
        };
        Err(Trap::Unimplemented(what))
    }

    fn software_int(&mut self, vec: u8) -> Res<()> {
        self.raise(Exception::new(vec), Source::Software)
    }

    fn decode_slow(&mut self, eip: u32, code32: bool) -> Result<crate::decode::Insn, DecodeError<Trap>> {
        let this = &mut *self;
        decode(code32, |i| this.fetch8(eip.wrapping_add(i)))
    }

    fn code_window(&mut self, eip: u32) -> Option<(u32, u32)> {
        let cs = &self.cpu.segs[CS];
        if eip > cs.limit {
            return None;
        }
        let lin = self.seg_check(CS, eip, 1, super::mem::Access::Fetch).ok()?;
        let in_limit = cs.limit - eip;
        let in_page = 0xfff - (lin & 0xfff);
        let avail = in_limit.min(in_page).saturating_add(1).min(crate::decode::MAX_LEN as u32);
        let phys = self.translate(lin, super::mem::Access::Fetch, self.cpu.cpl() == 3).ok()?;
        Some((phys, avail))
    }

    pub(super) fn exec(&mut self) -> Res<()> {
        let eip = self.cpu.eip;
        let code32 = self.cpu.segs[CS].big();
        let insn = match self.code_window(eip) {
            Some((phys, avail)) => {
                let bus = &mut *self.bus;
                let fast = decode(code32, |i| if i < avail { Ok(bus.read8(phys + i)) } else { Err(()) });
                match fast {
                    Ok(i) => Ok(i),
                    Err(DecodeError::TooLong) => Err(DecodeError::TooLong),
                    Err(DecodeError::Fetch(())) => self.decode_slow(eip, code32),
                }
            }
            None => self.decode_slow(eip, code32),
        };
        let insn = match insn {
            Ok(i) => i,
            Err(DecodeError::TooLong) => return gp(0),
            Err(DecodeError::Fetch(t)) => return Err(t),
        };
        self.insn = insn;
        let next = eip.wrapping_add(insn.len as u32);
        self.cpu.eip = next;
        if insn.lock && !(lockable(insn.op, insn.reg()) && insn.mem.is_some()) {
            return ud();
        }
        if insn.op >= 0x100 { self.exec_0f() } else { self.exec_1() }
    }

    fn exec_1(&mut self) -> Res<()> {
        let insn = self.insn;
        let op = insn.op;
        match op {
            0x00..=0x3f if op & 7 < 6 => {
                let aop = (op >> 3) as usize & 7;
                let sz = self.bsz();
                let cf = self.cpu.flag(flags::CF);
                match op & 7 {
                    0 | 1 => {
                        let loc = self.rm_loc();
                        if aop != 7 {
                            self.probe_write(loc, sz)?;
                        }
                        let a = self.read(loc, sz)?;
                        let b = self.get_reg(insn.reg(), sz);
                        let (r, f) = alu::group1(aop, a, b, cf, sz);
                        if aop != 7 {
                            self.write(loc, sz, r)?;
                        }
                        self.set_arith(f);
                    }
                    2 | 3 => {
                        let b = self.read(self.rm_loc(), sz)?;
                        let a = self.get_reg(insn.reg(), sz);
                        let (r, f) = alu::group1(aop, a, b, cf, sz);
                        if aop != 7 {
                            self.set_reg(insn.reg(), sz, r);
                        }
                        self.set_arith(f);
                    }
                    _ => {
                        let a = self.get_reg(EAX, sz);
                        let (r, f) = alu::group1(aop, a, insn.imm, cf, sz);
                        if aop != 7 {
                            self.set_reg(EAX, sz, r);
                        }
                        self.set_arith(f);
                    }
                }
            }
            0x06 | 0x0e | 0x16 | 0x1e => {
                let seg = (op >> 3) as usize;
                self.push(self.cpu.segs[seg].selector as u32, self.opsz())?;
            }
            0x07 | 0x17 | 0x1f => {
                let seg = (op >> 3) as usize;
                let sel = self.peek(0, 2)? as u16;
                self.load_seg(seg, sel)?;
                self.set_sp(self.sp().wrapping_add(self.opsz()));
            }
            0x27 | 0x2f => self.daa_das(op == 0x2f),
            0x37 | 0x3f => self.aaa_aas(op == 0x3f),
            0x40..=0x4f => {
                let r = (op & 7) as usize;
                let sz = self.opsz();
                let a = self.get_reg(r, sz);
                let (v, f) = if op < 0x48 { alu::inc(a, sz) } else { alu::dec(a, sz) };
                self.set_reg(r, sz, v);
                self.set_arith((f & !flags::CF) | (self.cpu.eflags & flags::CF));
            }
            0x50..=0x57 => {
                let sz = self.opsz();
                self.push(self.get_reg((op & 7) as usize, sz), sz)?;
            }
            0x58..=0x5f => {
                let sz = self.opsz();
                let v = self.pop(sz)?;
                self.set_reg((op & 7) as usize, sz, v);
            }
            0x60 => {
                let sz = self.opsz();
                let sp = self.get_reg(4, sz);
                let saved = self.cpu.regs[4];
                for r in 0..8 {
                    let v = if r == 4 { sp } else { self.get_reg(r, sz) };
                    if let Err(e) = self.push(v, sz) {
                        self.cpu.regs[4] = saved;
                        return Err(e);
                    }
                }
            }
            0x61 => {
                let sz = self.opsz();
                let mut vals = [0; 8];
                for (i, v) in vals.iter_mut().enumerate() {
                    *v = self.peek(i as u32 * sz, sz)?;
                }
                for r in 0..8 {
                    if r != 4 {
                        self.set_reg(r, sz, vals[7 - r]);
                    }
                }
                self.set_sp(self.sp().wrapping_add(8 * sz));
            }
            0x62 => {
                let (seg, off) = self.require_mem()?;
                let sz = self.opsz();
                let lo = sext(self.read_seg(seg, off, sz)?, sz) as i32;
                let hi = sext(self.read_seg(seg, self.off_add(off, sz), sz)?, sz) as i32;
                let idx = sext(self.get_reg(insn.reg(), sz), sz) as i32;
                if idx < lo || idx > hi {
                    return Err(Exception::new(vector::BR).into());
                }
            }
            0x63 => {
                if !self.pm() {
                    return ud();
                }
                let loc = self.rm_loc();
                let dst = self.read(loc, 2)?;
                let src = self.get_reg(insn.reg(), 2);
                if dst & 3 < src & 3 {
                    self.write(loc, 2, (dst & !3) | (src & 3))?;
                    self.cpu.set_flag(flags::ZF, true);
                } else {
                    self.cpu.set_flag(flags::ZF, false);
                }
            }
            0x68 => self.push(insn.imm, self.opsz())?,
            0x6a => self.push(sext(insn.imm, 1), self.opsz())?,
            0x69 | 0x6b => {
                let sz = self.opsz();
                let a = self.read(self.rm_loc(), sz)?;
                let b = if op == 0x6b { sext(insn.imm, 1) } else { insn.imm };
                let r = self.imul2(a, b, sz);
                self.set_reg(insn.reg(), sz, r);
            }
            0x6c | 0x6d => self.string_op(StrOp::Ins, self.bsz())?,
            0x6e | 0x6f => self.string_op(StrOp::Outs, self.bsz())?,
            0x70..=0x7f => {
                if self.cond(op & 0xf) {
                    self.jump_rel(sext(insn.imm, 1))?;
                }
            }
            0x80..=0x83 => {
                let sz = self.bsz();
                let imm = if op == 0x81 { insn.imm } else { sext(insn.imm, 1) };
                let loc = self.rm_loc();
                let aop = insn.reg();
                if aop != 7 {
                    self.probe_write(loc, sz)?;
                }
                let a = self.read(loc, sz)?;
                let (r, f) = alu::group1(aop, a, imm & mask(sz), self.cpu.flag(flags::CF), sz);
                if aop != 7 {
                    self.write(loc, sz, r)?;
                }
                self.set_arith(f);
            }
            0x84 | 0x85 => {
                let sz = self.bsz();
                let a = self.read(self.rm_loc(), sz)?;
                let (_, f) = alu::logic(a & self.get_reg(insn.reg(), sz), sz);
                self.set_arith(f);
            }
            0x86 | 0x87 => {
                let sz = self.bsz();
                let loc = self.rm_loc();
                self.probe_write(loc, sz)?;
                let a = self.read(loc, sz)?;
                let b = self.get_reg(insn.reg(), sz);
                self.write(loc, sz, b)?;
                self.set_reg(insn.reg(), sz, a);
            }
            0x88 | 0x89 => {
                let sz = self.bsz();
                let v = self.get_reg(insn.reg(), sz);
                self.write(self.rm_loc(), sz, v)?;
            }
            0x8a | 0x8b => {
                let sz = self.bsz();
                let v = self.read(self.rm_loc(), sz)?;
                self.set_reg(insn.reg(), sz, v);
            }
            0x8c => {
                if insn.reg() > 5 {
                    return ud();
                }
                let sel = self.cpu.segs[insn.reg()].selector as u32;
                match self.rm_loc() {
                    Loc::Reg(r) => self.set_reg(r, self.opsz(), sel),
                    loc => self.write(loc, 2, sel)?,
                }
            }
            0x8d => {
                let m = insn.mem.ok_or(()).or_else(|_| ud())?;
                let ea = self.ea(&m);
                self.set_reg(insn.reg(), self.opsz(), ea);
            }
            0x8e => {
                let seg = insn.reg();
                if seg == CS || seg > 5 {
                    return ud();
                }
                let sel = self.read(self.rm_loc(), 2)? as u16;
                self.load_seg(seg, sel)?;
            }
            0x8f => {
                if insn.reg() != 0 {
                    return ud();
                }
                let sz = self.opsz();
                let saved = self.cpu.regs[4];
                let v = self.pop(sz)?;
                let loc = self.rm_loc();
                if let Err(e) = self.write(loc, sz, v) {
                    self.cpu.regs[4] = saved;
                    return Err(e);
                }
            }
            0x90 => {}
            0x91..=0x97 => {
                let sz = self.opsz();
                let r = (op & 7) as usize;
                let a = self.get_reg(EAX, sz);
                let b = self.get_reg(r, sz);
                self.set_reg(EAX, sz, b);
                self.set_reg(r, sz, a);
            }
            0x98 => {
                if insn.op32 {
                    self.cpu.regs[EAX] = sext(self.cpu.regs[EAX], 2);
                } else {
                    let v = sext(self.cpu.regs[EAX], 1);
                    self.cpu.set_reg16(EAX, v as u16);
                }
            }
            0x99 => {
                let sz = self.opsz();
                let neg = self.get_reg(EAX, sz) & sign(sz) != 0;
                self.set_reg(EDX, sz, if neg { 0xffff_ffff } else { 0 });
            }
            0x9a => self.call_far(insn.imm2 as u16, insn.imm, self.opsz())?,
            0x9b => self.fwait()?,
            0x9c => {
                if self.cpu.v86_mode() && self.cpu.iopl() < 3 {
                    return gp(0);
                }
                let v = self.cpu.eflags & !(flags::VM | flags::RF);
                self.push(v, self.opsz())?;
            }
            0x9d => {
                if self.cpu.v86_mode() && self.cpu.iopl() < 3 {
                    return gp(0);
                }
                let sz = self.opsz();
                let v = self.pop(sz)?;
                if !self.cpu.protected_mode() {
                    let w = if sz == 2 { flags::WRITABLE & 0xffff } else { flags::WRITABLE & !(flags::VM | flags::VIP | flags::VIF) };
                    self.cpu.eflags = (self.cpu.eflags & !w) | (v & w) | flags::FIXED1;
                } else if self.cpu.v86_mode() {
                    let w = flags::WRITABLE & !(flags::VM | flags::VIP | flags::VIF | flags::IOPL) & mask(sz);
                    self.cpu.eflags = (self.cpu.eflags & !w) | (v & w) | flags::FIXED1;
                } else {
                    let cpl = self.cpu.cpl();
                    self.write_eflags(v, sz, cpl);
                }
            }
            0x9e => {
                let m = flags::SF | flags::ZF | flags::AF | flags::PF | flags::CF;
                let ah = self.cpu.reg8(4) as u32;
                self.cpu.eflags = (self.cpu.eflags & !m) | (ah & m) | flags::FIXED1;
            }
            0x9f => {
                let v = (self.cpu.eflags & 0xd5) | flags::FIXED1;
                self.cpu.set_reg8(4, v as u8);
            }
            0xa0..=0xa3 => {
                let sz = self.bsz();
                let seg = insn.seg_override.map_or(DS, |s| s as usize);
                if op < 0xa2 {
                    let v = self.read_seg(seg, insn.imm, sz)?;
                    self.set_reg(EAX, sz, v);
                } else {
                    let v = self.get_reg(EAX, sz);
                    self.write_seg(seg, insn.imm, sz, v)?;
                }
            }
            0xa4 | 0xa5 => self.string_op(StrOp::Movs, self.bsz())?,
            0xa6 | 0xa7 => self.string_op(StrOp::Cmps, self.bsz())?,
            0xa8 | 0xa9 => {
                let sz = self.bsz();
                let (_, f) = alu::logic(self.get_reg(EAX, sz) & insn.imm, sz);
                self.set_arith(f);
            }
            0xaa | 0xab => self.string_op(StrOp::Stos, self.bsz())?,
            0xac | 0xad => self.string_op(StrOp::Lods, self.bsz())?,
            0xae | 0xaf => self.string_op(StrOp::Scas, self.bsz())?,
            0xb0..=0xb7 => self.cpu.set_reg8((op & 7) as usize, insn.imm as u8),
            0xb8..=0xbf => self.set_reg((op & 7) as usize, self.opsz(), insn.imm),
            0xc0 | 0xc1 | 0xd0..=0xd3 => {
                let sz = self.bsz();
                let count = match op {
                    0xc0 | 0xc1 => insn.imm,
                    0xd0 | 0xd1 => 1,
                    _ => self.cpu.reg8(ECX) as u32,
                };
                let loc = self.rm_loc();
                self.probe_write(loc, sz)?;
                let a = self.read(loc, sz)?;
                let cf = self.cpu.flag(flags::CF);
                if let Some((r, f)) = alu::shift(insn.reg(), a, count, cf, self.cpu.eflags, sz) {
                    self.write(loc, sz, r)?;
                    self.set_arith(f);
                }
            }
            0xc2 | 0xc3 => {
                let sz = self.opsz();
                let target = self.peek(0, sz)?;
                self.jump_to(target)?;
                let release = if op == 0xc2 { insn.imm } else { 0 };
                self.set_sp(self.sp().wrapping_add(sz + release));
            }
            0xc4 | 0xc5 => {
                let (seg, off) = self.require_mem()?;
                let sz = self.opsz();
                let v = self.read_seg(seg, off, sz)?;
                let sel = self.read_seg(seg, self.off_add(off, sz), 2)? as u16;
                self.load_seg(if op == 0xc4 { ES } else { DS }, sel)?;
                self.set_reg(insn.reg(), sz, v);
            }
            0xc6 | 0xc7 => {
                if insn.reg() != 0 {
                    return ud();
                }
                self.write(self.rm_loc(), self.bsz(), insn.imm)?;
            }
            0xc8 => self.enter(insn.imm & 0xffff, insn.imm2 & 0x1f)?,
            0xc9 => {
                let sz = self.opsz();
                let bp = if self.stack32() { self.cpu.regs[EBP] } else { self.cpu.regs[EBP] & 0xffff };
                let v = self.read_seg(SS, bp, sz)?;
                self.set_sp(bp.wrapping_add(sz));
                self.set_reg(EBP, sz, v);
            }
            0xca | 0xcb => {
                let imm = if op == 0xca { insn.imm } else { 0 };
                self.ret_far(self.opsz(), imm)?;
            }
            0xcc => self.software_int(vector::BP)?,
            0xcd => self.software_int(insn.imm as u8)?,
            0xce => {
                if self.cpu.flag(flags::OF) {
                    self.software_int(vector::OF)?;
                }
            }
            0xcf => self.iret(self.opsz())?,
            0xd4 => {
                let base = insn.imm & 0xff;
                if base == 0 {
                    return Err(Exception::new(vector::DE).into());
                }
                let al = self.cpu.reg8(0) as u32;
                self.cpu.set_reg8(4, (al / base) as u8);
                self.cpu.set_reg8(0, (al % base) as u8);
                let (_, f) = alu::logic(al % base, 1);
                self.set_arith(f);
            }
            0xd5 => {
                let al = self.cpu.reg8(0) as u32;
                let ah = self.cpu.reg8(4) as u32;
                let (r, f) = alu::add(al, (ah * (insn.imm & 0xff)) & 0xff, false, 1);
                self.cpu.set_reg16(EAX, r as u16);
                self.set_arith(f);
            }
            0xd6 => {
                let v = if self.cpu.flag(flags::CF) { 0xff } else { 0 };
                self.cpu.set_reg8(0, v);
            }
            0xd7 => {
                let seg = insn.seg_override.map_or(DS, |s| s as usize);
                let mut off = self.cpu.regs[EBX].wrapping_add(self.cpu.reg8(0) as u32);
                if !insn.addr32 {
                    off &= 0xffff;
                }
                let v = self.read_seg(seg, off, 1)?;
                self.cpu.set_reg8(0, v as u8);
            }
            0xd8..=0xdf => self.x87()?,
            0xe0..=0xe2 => {
                let count = self.count_reg().wrapping_sub(1);
                self.set_count_reg(count);
                let count = if insn.addr32 { count } else { count & 0xffff };
                let zf = self.cpu.flag(flags::ZF);
                let take = count != 0
                    && match op {
                        0xe0 => !zf,
                        0xe1 => zf,
                        _ => true,
                    };
                if take {
                    self.jump_rel(sext(insn.imm, 1))?;
                }
            }
            0xe3 => {
                if self.count_reg() == 0 {
                    self.jump_rel(sext(insn.imm, 1))?;
                }
            }
            0xe4 | 0xe5 | 0xec | 0xed => {
                let sz = self.bsz();
                let port = if op < 0xec { insn.imm as u16 } else { self.cpu.reg16(EDX) };
                self.check_io(port, sz)?;
                let v = self.bus.io_read(port, sz as u8);
                self.set_reg(EAX, sz, v);
            }
            0xe6 | 0xe7 | 0xee | 0xef => {
                let sz = self.bsz();
                let port = if op < 0xee { insn.imm as u16 } else { self.cpu.reg16(EDX) };
                self.check_io(port, sz)?;
                let v = self.get_reg(EAX, sz);
                self.bus.io_write(port, sz as u8, v);
            }
            0xe8 => {
                let sz = self.opsz();
                let ret = self.cpu.eip;
                let target = ret.wrapping_add(insn.imm);
                let target = if insn.op32 { target } else { target & 0xffff };
                if target > self.cpu.segs[CS].limit {
                    return gp(0);
                }
                self.push(ret, sz)?;
                self.cpu.eip = target;
            }
            0xe9 => self.jump_rel(insn.imm)?,
            0xea => self.jmp_far(insn.imm2 as u16, insn.imm)?,
            0xeb => self.jump_rel(sext(insn.imm, 1))?,
            0xf1 => self.raise(Exception::new(vector::DB), Source::External)?,
            0xf4 => {
                self.require_cpl0()?;
                self.cpu.halted = true;
            }
            0xf5 => self.cpu.eflags ^= flags::CF,
            0xf6 | 0xf7 => self.group3()?,
            0xf8 => self.cpu.set_flag(flags::CF, false),
            0xf9 => self.cpu.set_flag(flags::CF, true),
            0xfa | 0xfb => {
                let allowed = if !self.cpu.protected_mode() {
                    true
                } else if self.cpu.v86_mode() {
                    self.cpu.iopl() == 3
                } else {
                    self.cpu.cpl() <= self.cpu.iopl()
                };
                if !allowed {
                    return gp(0);
                }
                if op == 0xfb && !self.cpu.flag(flags::IF) {
                    self.cpu.interrupt_shadow = true;
                }
                self.cpu.set_flag(flags::IF, op == 0xfb);
            }
            0xfc => self.cpu.set_flag(flags::DF, false),
            0xfd => self.cpu.set_flag(flags::DF, true),
            0xfe => {
                if insn.reg() > 1 {
                    return ud();
                }
                self.inc_dec_rm(1)?;
            }
            0xff => self.group5()?,
            _ => return ud(),
        }
        Ok(())
    }

    fn inc_dec_rm(&mut self, sz: u32) -> Res<()> {
        let loc = self.rm_loc();
        self.probe_write(loc, sz)?;
        let a = self.read(loc, sz)?;
        let (v, f) = if self.insn.reg() == 0 { alu::inc(a, sz) } else { alu::dec(a, sz) };
        self.write(loc, sz, v)?;
        self.set_arith((f & !flags::CF) | (self.cpu.eflags & flags::CF));
        Ok(())
    }

    fn group5(&mut self) -> Res<()> {
        let sz = self.opsz();
        match self.insn.reg() {
            0 | 1 => self.inc_dec_rm(sz),
            2 => {
                let target = self.read(self.rm_loc(), sz)?;
                let ret = self.cpu.eip;
                let target = if self.insn.op32 { target } else { target & 0xffff };
                if target > self.cpu.segs[CS].limit {
                    return gp(0);
                }
                self.push(ret, sz)?;
                self.cpu.eip = target;
                Ok(())
            }
            3 | 5 => {
                let (seg, off) = self.require_mem()?;
                let eip = self.read_seg(seg, off, sz)?;
                let sel = self.read_seg(seg, self.off_add(off, sz), 2)? as u16;
                if self.insn.reg() == 3 { self.call_far(sel, eip, sz) } else { self.jmp_far(sel, eip) }
            }
            4 => {
                let target = self.read(self.rm_loc(), sz)?;
                self.jump_to(target)
            }
            6 => {
                let v = self.read(self.rm_loc(), sz)?;
                self.push(v, sz)
            }
            _ => ud(),
        }
    }

    fn group3(&mut self) -> Res<()> {
        let sz = self.bsz();
        let loc = self.rm_loc();
        match self.insn.reg() {
            0 | 1 => {
                let a = self.read(loc, sz)?;
                let (_, f) = alu::logic(a & self.insn.imm, sz);
                self.set_arith(f);
            }
            2 => {
                self.probe_write(loc, sz)?;
                let a = self.read(loc, sz)?;
                self.write(loc, sz, !a & mask(sz))?;
            }
            3 => {
                self.probe_write(loc, sz)?;
                let a = self.read(loc, sz)?;
                let (r, f) = alu::sub(0, a, false, sz);
                self.write(loc, sz, r)?;
                self.set_arith(f);
            }
            4 | 5 => {
                let b = self.read(loc, sz)?;
                let a = self.get_reg(EAX, sz);
                let signed = self.insn.reg() == 5;
                let (lo, hi, overflow) = if signed {
                    let p = sext(a, sz) as i32 as i64 * sext(b, sz) as i32 as i64;
                    let lo = p as u32 & mask(sz);
                    let hi = (p >> (sz * 8)) as u32 & mask(sz);
                    (lo, hi, p != sext(lo, sz) as i32 as i64)
                } else {
                    let p = a as u64 * b as u64;
                    let lo = p as u32 & mask(sz);
                    let hi = (p >> (sz * 8)) as u32 & mask(sz);
                    (lo, hi, hi != 0)
                };
                if sz == 1 {
                    self.cpu.set_reg16(EAX, (hi << 8 | lo) as u16);
                } else {
                    self.set_reg(EAX, sz, lo);
                    self.set_reg(EDX, sz, hi);
                }
                let (_, f) = alu::logic(lo, sz);
                let f = (f & !(flags::CF | flags::OF)) | if overflow { flags::CF | flags::OF } else { 0 };
                self.set_arith(f);
            }
            _ => {
                let divisor = self.read(loc, sz)?;
                let signed = self.insn.reg() == 7;
                let dividend: u64 = if sz == 1 {
                    self.cpu.reg16(EAX) as u64
                } else {
                    (self.get_reg(EDX, sz) as u64) << (sz * 8) | self.get_reg(EAX, sz) as u64
                };
                let de = || Err(Exception::new(vector::DE).into());
                if divisor == 0 {
                    return de();
                }
                let (q, r) = if signed {
                    let bits = sz * 16;
                    let n = ((dividend << (64 - bits)) as i64) >> (64 - bits);
                    let d = sext(divisor, sz) as i32 as i64;
                    let q = n.wrapping_div(d);
                    let r = n.wrapping_rem(d);
                    let half = 1i64 << (sz * 8 - 1);
                    if q >= half || q < -half {
                        return de();
                    }
                    (q as u32 & mask(sz), r as u32 & mask(sz))
                } else {
                    let q = dividend / divisor as u64;
                    if q > mask(sz) as u64 {
                        return de();
                    }
                    (q as u32, (dividend % divisor as u64) as u32)
                };
                if sz == 1 {
                    self.cpu.set_reg16(EAX, (r << 8 | q) as u16);
                } else {
                    self.set_reg(EAX, sz, q);
                    self.set_reg(EDX, sz, r);
                }
            }
        }
        Ok(())
    }

    fn imul2(&mut self, a: u32, b: u32, sz: u32) -> u32 {
        let p = sext(a, sz) as i32 as i64 * sext(b, sz) as i32 as i64;
        let r = p as u32 & mask(sz);
        let overflow = p != sext(r, sz) as i32 as i64;
        let (_, f) = alu::logic(r, sz);
        let f = (f & !(flags::CF | flags::OF)) | if overflow { flags::CF | flags::OF } else { 0 };
        self.set_arith(f);
        r
    }

    fn daa_das(&mut self, sub: bool) {
        let old_al = self.cpu.reg8(0);
        let old_cf = self.cpu.flag(flags::CF);
        let mut al = old_al;
        let mut cf = false;
        let mut af = false;
        if al & 0xf > 9 || self.cpu.flag(flags::AF) {
            let (v, carry) = if sub { al.overflowing_sub(6) } else { al.overflowing_add(6) };
            al = v;
            cf = old_cf || carry;
            af = true;
        }
        if old_al > 0x99 || old_cf {
            al = if sub { al.wrapping_sub(0x60) } else { al.wrapping_add(0x60) };
            cf = true;
        }
        self.cpu.set_reg8(0, al);
        let (_, mut f) = alu::logic(al as u32, 1);
        if cf {
            f |= flags::CF;
        }
        if af {
            f |= flags::AF;
        }
        f |= self.cpu.eflags & flags::OF;
        self.set_arith(f);
    }

    fn aaa_aas(&mut self, sub: bool) {
        let al = self.cpu.reg8(0);
        let adjust = al & 0xf > 9 || self.cpu.flag(flags::AF);
        if adjust {
            let ax = self.cpu.reg16(EAX);
            let ax = if sub { ax.wrapping_sub(6).wrapping_sub(0x100) } else { ax.wrapping_add(0x106) };
            self.cpu.set_reg16(EAX, ax);
        }
        let al = self.cpu.reg8(0) & 0xf;
        self.cpu.set_reg8(0, al);
        let keep = self.cpu.eflags & (flags::ARITH & !(flags::CF | flags::AF));
        self.set_arith(keep | if adjust { flags::CF | flags::AF } else { 0 });
    }

    fn enter(&mut self, size: u32, level: u32) -> Res<()> {
        let sz = self.opsz();
        let saved_sp = self.cpu.regs[4];
        let saved_bp = self.cpu.regs[EBP];
        let result = (|| -> Res<()> {
            let bp = self.get_reg(EBP, sz);
            self.push(bp, sz)?;
            let frame = self.sp();
            if level > 0 {
                let mut bp = if self.stack32() { self.cpu.regs[EBP] } else { self.cpu.regs[EBP] & 0xffff };
                for _ in 1..level {
                    bp = bp.wrapping_sub(sz);
                    if !self.stack32() {
                        bp &= 0xffff;
                    }
                    let v = self.read_seg(SS, bp, sz)?;
                    self.push(v, sz)?;
                }
                self.push(frame, sz)?;
            }
            let new_sp = self.sp().wrapping_sub(size);
            let probe = if self.stack32() { new_sp } else { new_sp & 0xffff };
            if size > 0 {
                self.seg_check(SS, probe, 1, super::mem::Access::Write)?;
            }
            self.set_reg(EBP, sz, frame);
            self.set_sp(new_sp);
            Ok(())
        })();
        if result.is_err() {
            self.cpu.regs[4] = saved_sp;
            self.cpu.regs[EBP] = saved_bp;
        }
        result
    }

    fn bit_op(&mut self, kind: usize, bit_from_reg: bool) -> Res<()> {
        let sz = self.opsz();
        let bits = sz * 8;
        let offset = if bit_from_reg { self.get_reg(self.insn.reg(), sz) } else { self.insn.imm };
        let loc = match self.rm_loc() {
            Loc::Mem(seg, off) if bit_from_reg => {
                let signed = sext(offset, sz) as i32;
                let delta = (signed >> (bits.trailing_zeros())) * sz as i32;
                let off = off.wrapping_add(delta as u32);
                Loc::Mem(seg, if self.insn.addr32 { off } else { off & 0xffff })
            }
            loc => loc,
        };
        let bit = offset & (bits - 1);
        if kind != 0 {
            self.probe_write(loc, sz)?;
        }
        let v = self.read(loc, sz)?;
        let old = v >> bit & 1 != 0;
        let new = match kind {
            1 => v | 1 << bit,
            2 => v & !(1 << bit),
            3 => v ^ 1 << bit,
            _ => v,
        };
        if kind != 0 {
            self.write(loc, sz, new)?;
        }
        self.cpu.set_flag(flags::CF, old);
        Ok(())
    }

    fn exec_0f(&mut self) -> Res<()> {
        let insn = self.insn;
        let op = insn.op & 0xff;
        match op {
            0x00 => {
                if !self.pm() {
                    return ud();
                }
                match insn.reg() {
                    0 | 1 => {
                        let sel = if insn.reg() == 0 { self.cpu.ldtr.selector } else { self.cpu.tr.selector };
                        match self.rm_loc() {
                            Loc::Reg(r) => self.set_reg(r, self.opsz(), sel as u32),
                            loc => self.write(loc, 2, sel as u32)?,
                        }
                    }
                    2 | 3 => {
                        self.require_cpl0()?;
                        let sel = self.read(self.rm_loc(), 2)? as u16;
                        if insn.reg() == 2 { self.lldt(sel)? } else { self.ltr(sel)? }
                    }
                    4 | 5 => {
                        let sel = self.read(self.rm_loc(), 2)? as u16;
                        let ok = self.verify(sel, insn.reg() == 5)?;
                        self.cpu.set_flag(flags::ZF, ok);
                    }
                    _ => return ud(),
                }
            }
            0x01 => match insn.reg() {
                0 | 1 => {
                    let (seg, off) = self.require_mem()?;
                    let t = if insn.reg() == 0 { self.cpu.gdtr } else { self.cpu.idtr };
                    self.write_seg(seg, off, 2, t.limit as u32)?;
                    let base = if insn.op32 { t.base } else { t.base & 0x00ff_ffff };
                    self.write_seg(seg, self.off_add(off, 2), 4, base)?;
                }
                2 | 3 => {
                    self.require_cpl0()?;
                    let (seg, off) = self.require_mem()?;
                    let limit = self.read_seg(seg, off, 2)? as u16;
                    let mut base = self.read_seg(seg, self.off_add(off, 2), 4)?;
                    if !insn.op32 {
                        base &= 0x00ff_ffff;
                    }
                    let t = crate::cpu::DescriptorTable { base, limit };
                    if insn.reg() == 2 {
                        self.cpu.gdtr = t;
                    } else {
                        self.cpu.idtr = t;
                    }
                }
                4 => {
                    let v = self.cpu.cr0;
                    match self.rm_loc() {
                        Loc::Reg(r) => self.set_reg(r, self.opsz(), v),
                        loc => self.write(loc, 2, v)?,
                    }
                }
                6 => {
                    self.require_cpl0()?;
                    let v = self.read(self.rm_loc(), 2)?;
                    let keep_pe = self.cpu.cr0 & cr0::PE;
                    let old = self.cpu.cr0;
                    self.cpu.cr0 = (self.cpu.cr0 & !0xe) | (v & 0xf) | keep_pe;
                    if (old ^ self.cpu.cr0) & cr0::PE != 0 {
                        self.cpu.tlb.flush();
                    }
                }
                7 => {
                    self.require_cpl0()?;
                    let (seg, off) = self.require_mem()?;
                    let lin = self.cpu.segs[seg].base.wrapping_add(off);
                    self.cpu.tlb.flush_page(lin);
                }
                _ => return ud(),
            },
            0x02 | 0x03 => {
                if !self.pm() {
                    return ud();
                }
                let sel = self.read(self.rm_loc(), 2)? as u16;
                match self.lar_lsl(sel, op == 0x03)? {
                    Some(v) => {
                        self.set_reg(insn.reg(), self.opsz(), v);
                        self.cpu.set_flag(flags::ZF, true);
                    }
                    None => self.cpu.set_flag(flags::ZF, false),
                }
            }
            0x06 => {
                self.require_cpl0()?;
                self.cpu.cr0 &= !cr0::TS;
            }
            0x08 | 0x09 => self.require_cpl0()?,
            0x0b | 0xb9 | 0xff => return ud(),
            0x18..=0x1f => {}
            0x20 | 0x22 => {
                self.require_cpl0()?;
                let r = insn.rm();
                let n = insn.reg();
                if op == 0x20 {
                    let v = match n {
                        0 => self.cpu.cr0,
                        2 => self.cpu.cr2,
                        3 => self.cpu.cr3,
                        4 => self.cpu.cr4,
                        _ => return ud(),
                    };
                    self.cpu.regs[r] = v;
                } else {
                    let v = self.cpu.regs[r];
                    match n {
                        0 => {
                            if v & cr0::PG != 0 && v & cr0::PE == 0 {
                                return gp(0);
                            }
                            let old = self.cpu.cr0;
                            self.cpu.cr0 = v | cr0::ET;
                            if (old ^ self.cpu.cr0) & (cr0::PG | cr0::WP | cr0::PE) != 0 {
                                self.cpu.tlb.flush();
                            }
                        }
                        2 => self.cpu.cr2 = v,
                        3 => {
                            self.cpu.cr3 = v;
                            if self.cpu.cr4 & cr4::PGE != 0 {
                                self.cpu.tlb.flush_nonglobal();
                            } else {
                                self.cpu.tlb.flush();
                            }
                        }
                        4 => {
                            let supported = cr4::VME | cr4::PVI | cr4::TSD | cr4::DE | cr4::PSE | cr4::PGE
                                | cr4::PCE | cr4::OSFXSR | cr4::OSXMMEXCPT;
                            if v & !supported != 0 {
                                return gp(0);
                            }
                            let old = self.cpu.cr4;
                            self.cpu.cr4 = v;
                            if (old ^ v) & (cr4::PSE | cr4::PGE | cr4::PAE) != 0 {
                                self.cpu.tlb.flush();
                            }
                        }
                        _ => return ud(),
                    }
                }
            }
            0x21 | 0x23 => {
                self.require_cpl0()?;
                let r = insn.rm();
                let n = match insn.reg() {
                    4 | 5 if self.cpu.cr4 & cr4::DE != 0 => return ud(),
                    4 => 6,
                    5 => 7,
                    n => n,
                };
                if op == 0x21 {
                    self.cpu.regs[r] = self.cpu.dr[n];
                } else {
                    self.cpu.dr[n] = self.cpu.regs[r];
                }
            }
            0x30 => {
                self.require_cpl0()?;
                let v = (self.cpu.regs[EDX] as u64) << 32 | self.cpu.regs[EAX] as u64;
                self.wrmsr(self.cpu.regs[ECX], v)?;
            }
            0x31 => {
                if self.cpu.cr4 & cr4::TSD != 0 && self.cpu.protected_mode() && self.cpu.cpl() != 0 {
                    return gp(0);
                }
                let t = self.cpu.tsc;
                self.cpu.regs[EAX] = t as u32;
                self.cpu.regs[EDX] = (t >> 32) as u32;
            }
            0x32 => {
                self.require_cpl0()?;
                let v = self.rdmsr(self.cpu.regs[ECX])?;
                self.cpu.regs[EAX] = v as u32;
                self.cpu.regs[EDX] = (v >> 32) as u32;
            }
            0x34 => self.sysenter()?,
            0x35 => self.sysexit()?,
            0x40..=0x4f => {
                let sz = self.opsz();
                let v = self.read(self.rm_loc(), sz)?;
                if self.cond(op & 0xf) {
                    self.set_reg(insn.reg(), sz, v);
                }
            }
            0x80..=0x8f => {
                if self.cond(op & 0xf) {
                    self.jump_rel(insn.imm)?;
                }
            }
            0x90..=0x9f => {
                let v = self.cond(op & 0xf) as u32;
                self.write(self.rm_loc(), 1, v)?;
            }
            0xa0 | 0xa8 => {
                let seg = if op == 0xa0 { FS } else { GS };
                self.push(self.cpu.segs[seg].selector as u32, self.opsz())?;
            }
            0xa1 | 0xa9 => {
                let seg = if op == 0xa1 { FS } else { GS };
                let sel = self.peek(0, 2)? as u16;
                self.load_seg(seg, sel)?;
                self.set_sp(self.sp().wrapping_add(self.opsz()));
            }
            0xa2 => {
                let [a, b, c, d] = crate::cpuid::cpuid(self.cpu.regs[EAX], self.cpu.regs[ECX]);
                self.cpu.regs[EAX] = a;
                self.cpu.regs[EBX] = b;
                self.cpu.regs[ECX] = c;
                self.cpu.regs[EDX] = d;
            }
            0xa3 => self.bit_op(0, true)?,
            0xab => self.bit_op(1, true)?,
            0xb3 => self.bit_op(2, true)?,
            0xbb => self.bit_op(3, true)?,
            0xba => {
                if insn.reg() < 4 {
                    return ud();
                }
                self.bit_op(insn.reg() - 4, false)?;
            }
            0xa4 | 0xa5 | 0xac | 0xad => {
                let sz = self.opsz();
                let count = if op & 1 == 0 { insn.imm } else { self.cpu.reg8(ECX) as u32 };
                let loc = self.rm_loc();
                self.probe_write(loc, sz)?;
                let dst = self.read(loc, sz)?;
                let src = self.get_reg(insn.reg(), sz);
                if let Some((r, f)) = alu::shift_double(op < 0xa8, dst, src, count, self.cpu.eflags, sz) {
                    self.write(loc, sz, r)?;
                    self.set_arith(f);
                }
            }
            0xae => self.group15()?,
            0x10..=0x17 | 0x28..=0x2f | 0x50..=0x7f | 0xc2..=0xc6 | 0xd0..=0xfe => self.simd()?,
            0xaf => {
                let sz = self.opsz();
                let b = self.read(self.rm_loc(), sz)?;
                let a = self.get_reg(insn.reg(), sz);
                let r = self.imul2(a, b, sz);
                self.set_reg(insn.reg(), sz, r);
            }
            0xb0 | 0xb1 => {
                let sz = self.bsz();
                let loc = self.rm_loc();
                self.probe_write(loc, sz)?;
                let dst = self.read(loc, sz)?;
                let acc = self.get_reg(EAX, sz);
                let (_, f) = alu::sub(acc, dst, false, sz);
                self.set_arith(f);
                if acc == dst {
                    let src = self.get_reg(insn.reg(), sz);
                    self.write(loc, sz, src)?;
                } else {
                    self.write(loc, sz, dst)?;
                    self.set_reg(EAX, sz, dst);
                }
            }
            0xb2 | 0xb4 | 0xb5 => {
                let (seg, off) = self.require_mem()?;
                let sz = self.opsz();
                let v = self.read_seg(seg, off, sz)?;
                let sel = self.read_seg(seg, self.off_add(off, sz), 2)? as u16;
                let target = match op {
                    0xb2 => SS,
                    0xb4 => FS,
                    _ => GS,
                };
                self.load_seg(target, sel)?;
                self.set_reg(insn.reg(), sz, v);
            }
            0xb6 | 0xb7 | 0xbe | 0xbf => {
                let src_sz = if op & 1 == 0 { 1 } else { 2 };
                let v = self.read(self.rm_loc(), src_sz)?;
                let v = if op >= 0xbe { sext(v, src_sz) } else { v };
                self.set_reg(insn.reg(), self.opsz(), v);
            }
            0xbc | 0xbd => {
                let sz = self.opsz();
                let v = self.read(self.rm_loc(), sz)?;
                if v == 0 {
                    self.cpu.set_flag(flags::ZF, true);
                } else {
                    let idx = if op == 0xbc { v.trailing_zeros() } else { 31 - v.leading_zeros() };
                    self.set_reg(insn.reg(), sz, idx);
                    self.cpu.set_flag(flags::ZF, false);
                }
            }
            0xc0 | 0xc1 => {
                let sz = self.bsz();
                let loc = self.rm_loc();
                self.probe_write(loc, sz)?;
                let dst = self.read(loc, sz)?;
                let src = self.get_reg(insn.reg(), sz);
                let (r, f) = alu::add(dst, src, false, sz);
                self.set_reg(insn.reg(), sz, dst);
                self.write(loc, sz, r)?;
                self.set_arith(f);
            }
            0xc7 => {
                if insn.reg() != 1 {
                    return ud();
                }
                let (seg, off) = self.require_mem()?;
                let loc = Loc::Mem(seg, off);
                self.probe_write(loc, 4)?;
                self.probe_write(Loc::Mem(seg, self.off_add(off, 4)), 4)?;
                let lo = self.read_seg(seg, off, 4)?;
                let hi = self.read_seg(seg, self.off_add(off, 4), 4)?;
                if lo == self.cpu.regs[EAX] && hi == self.cpu.regs[EDX] {
                    let (nlo, nhi) = (self.cpu.regs[EBX], self.cpu.regs[ECX]);
                    self.write_seg(seg, off, 4, nlo)?;
                    self.write_seg(seg, self.off_add(off, 4), 4, nhi)?;
                    self.cpu.set_flag(flags::ZF, true);
                } else {
                    self.write_seg(seg, off, 4, lo)?;
                    self.write_seg(seg, self.off_add(off, 4), 4, hi)?;
                    self.cpu.regs[EAX] = lo;
                    self.cpu.regs[EDX] = hi;
                    self.cpu.set_flag(flags::ZF, false);
                }
            }
            0xc8..=0xcf => {
                let r = (op & 7) as usize;
                self.cpu.regs[r] = if insn.op32 { self.cpu.regs[r].swap_bytes() } else { self.cpu.regs[r] & 0xffff_0000 };
            }
            _ => return self.unimplemented(),
        }
        Ok(())
    }

    fn verify(&mut self, sel: u16, write: bool) -> Res<bool> {
        if sel & !3 == 0 {
            return Ok(false);
        }
        let Ok((d, _)) = self.descriptor(sel) else { return Ok(false) };
        if d.system() {
            return Ok(false);
        }
        let cpl = self.cpu.cpl();
        let rpl = (sel & 3) as u8;
        if !d.conforming() && (d.dpl() < cpl || d.dpl() < rpl) {
            return Ok(false);
        }
        Ok(if write { d.writable_data() } else { d.readable() })
    }

    fn lar_lsl(&mut self, sel: u16, lsl: bool) -> Res<Option<u32>> {
        if sel & !3 == 0 {
            return Ok(None);
        }
        let Ok((d, _)) = self.descriptor(sel) else { return Ok(None) };
        if d.system() {
            let ok = if lsl { matches!(d.typ(), 1 | 2 | 3 | 9 | 11) } else { matches!(d.typ(), 1..=5 | 9 | 11 | 12) };
            if !ok {
                return Ok(None);
            }
        }
        let cpl = self.cpu.cpl();
        let rpl = (sel & 3) as u8;
        if !d.conforming() && (d.dpl() < cpl || d.dpl() < rpl) {
            return Ok(None);
        }
        Ok(Some(if lsl { d.limit() } else { d.hi & 0x00f0_ff00 }))
    }

    fn rdmsr(&mut self, msr: u32) -> Res<u64> {
        Ok(match msr {
            0x10 => self.cpu.tsc,
            0x1b => self.cpu.apic_base,
            0x17 | 0x8b => 0,
            0x174 => self.cpu.sysenter_cs as u64,
            0x175 => self.cpu.sysenter_esp as u64,
            0x176 => self.cpu.sysenter_eip as u64,
            _ => return gp(0),
        })
    }

    fn wrmsr(&mut self, msr: u32, v: u64) -> Res<()> {
        match msr {
            0x10 => self.cpu.tsc = v,
            0x1b => self.cpu.apic_base = v,
            0x8b => {}
            0x174 => self.cpu.sysenter_cs = v as u32,
            0x175 => self.cpu.sysenter_esp = v as u32,
            0x176 => self.cpu.sysenter_eip = v as u32,
            _ => return gp(0),
        }
        Ok(())
    }
}

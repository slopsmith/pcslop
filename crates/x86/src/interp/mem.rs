//! Segmentation, paging and operand access.

use super::{Ctx, Exception, Res, gp, vector};
use crate::bus::Bus;
use crate::cpu::{SS, attr, cr0, cr4};
use crate::decode::{MemOperand, NO_REG};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loc {
    Reg(usize),
    Mem(usize, u32),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
    Fetch,
}

pub(crate) const fn mask(sz: u32) -> u32 {
    match sz {
        1 => 0xff,
        2 => 0xffff,
        _ => 0xffff_ffff,
    }
}

pub(crate) const fn sign(sz: u32) -> u32 {
    1 << (sz * 8 - 1)
}

pub(crate) fn sext(v: u32, sz: u32) -> u32 {
    match sz {
        1 => v as u8 as i8 as i32 as u32,
        2 => v as u16 as i16 as i32 as u32,
        _ => v,
    }
}

impl<B: Bus> Ctx<'_, B> {
    pub fn ea(&self, m: &MemOperand) -> u32 {
        let mut a = m.disp;
        if m.base != NO_REG {
            a = a.wrapping_add(self.cpu.regs[m.base as usize]);
        }
        if m.index != NO_REG {
            a = a.wrapping_add(self.cpu.regs[m.index as usize] << m.scale);
        }
        if self.insn.addr32 { a } else { a & 0xffff }
    }

    pub fn rm_loc(&self) -> Loc {
        match self.insn.mem {
            Some(m) => Loc::Mem(self.insn.seg(&m), self.ea(&m)),
            None => Loc::Reg(self.insn.rm()),
        }
    }

    pub fn get_reg(&self, r: usize, sz: u32) -> u32 {
        match sz {
            1 => self.cpu.reg8(r) as u32,
            2 => self.cpu.reg16(r) as u32,
            _ => self.cpu.regs[r],
        }
    }

    pub fn set_reg(&mut self, r: usize, sz: u32, v: u32) {
        match sz {
            1 => self.cpu.set_reg8(r, v as u8),
            2 => self.cpu.set_reg16(r, v as u16),
            _ => self.cpu.regs[r] = v,
        }
    }

    pub fn read(&mut self, loc: Loc, sz: u32) -> Res<u32> {
        match loc {
            Loc::Reg(r) => Ok(self.get_reg(r, sz)),
            Loc::Mem(seg, off) => self.read_seg(seg, off, sz),
        }
    }

    pub fn write(&mut self, loc: Loc, sz: u32, v: u32) -> Res<()> {
        match loc {
            Loc::Reg(r) => {
                self.set_reg(r, sz, v);
                Ok(())
            }
            Loc::Mem(seg, off) => self.write_seg(seg, off, sz, v),
        }
    }

    /// Checks that a write to `loc` would succeed, so that read-modify-write
    /// instructions fault before changing anything.
    pub fn probe_write(&mut self, loc: Loc, sz: u32) -> Res<()> {
        if let Loc::Mem(seg, off) = loc {
            let lin = self.seg_check(seg, off, sz, Access::Write)?;
            self.translate_range(lin, sz, Access::Write, self.cpu.cpl() == 3)?;
        }
        Ok(())
    }

    pub fn read_seg(&mut self, seg: usize, off: u32, sz: u32) -> Res<u32> {
        let lin = self.seg_check(seg, off, sz, Access::Read)?;
        self.read_lin(lin, sz, self.cpu.cpl() == 3, Access::Read)
    }

    pub fn write_seg(&mut self, seg: usize, off: u32, sz: u32, v: u32) -> Res<()> {
        let lin = self.seg_check(seg, off, sz, Access::Write)?;
        self.write_lin(lin, sz, v, self.cpu.cpl() == 3)
    }

    fn seg_fault(&self, seg: usize) -> Exception {
        if seg == SS {
            Exception::with_error(vector::SS, 0)
        } else {
            Exception::with_error(vector::GP, 0)
        }
    }

    /// Applies segment checks and returns the linear address.
    pub fn seg_check(&self, seg: usize, off: u32, sz: u32, acc: Access) -> Res<u32> {
        let s = &self.cpu.segs[seg];
        if self.cpu.protected_mode() && !self.cpu.v86_mode() {
            if !s.present() || s.attr & attr::S == 0 {
                return Err(self.seg_fault(seg).into());
            }
            let code = s.attr & attr::CODE != 0;
            let rw = s.attr & attr::RW != 0;
            let bad = match acc {
                Access::Write => code || !rw,
                Access::Read => code && !rw,
                Access::Fetch => !code,
            };
            if bad {
                return Err(self.seg_fault(seg).into());
            }
        }
        let last = off as u64 + sz as u64 - 1;
        let expand_down = s.attr & (attr::CODE | attr::DC) == attr::DC;
        let ok = if expand_down {
            let upper: u64 = if s.big() { 0xffff_ffff } else { 0xffff };
            off as u64 > s.limit as u64 && last <= upper
        } else {
            last <= s.limit as u64
        };
        if !ok {
            return Err(self.seg_fault(seg).into());
        }
        Ok(s.base.wrapping_add(off))
    }

    /// Translates a linear address through the x86 page tables, setting the
    /// Accessed and Dirty bits.
    pub fn translate(&mut self, lin: u32, acc: Access, user: bool) -> Res<u32> {
        if self.cpu.cr0 & cr0::PG == 0 {
            return Ok(lin);
        }
        let write = acc == Access::Write;
        let err = (write as u32) << 1 | (user as u32) << 2;
        let pf = |cpu: &mut crate::cpu::Cpu, present: bool| {
            cpu.cr2 = lin;
            Err(Exception::with_error(vector::PF, err | present as u32).into())
        };

        let pde_addr = (self.cpu.cr3 & !0xfff) | (lin >> 22) << 2;
        let mut pde = self.bus.read32(pde_addr);
        if pde & 1 == 0 {
            return pf(self.cpu, false);
        }
        let big = pde & 0x80 != 0 && self.cpu.cr4 & cr4::PSE != 0;
        let (phys, perms, entry_addr, mut entry) = if big {
            ((pde & 0xffc0_0000) | (lin & 0x3f_ffff), pde, pde_addr, pde)
        } else {
            let pte_addr = (pde & !0xfff) | ((lin >> 12) & 0x3ff) << 2;
            let pte = self.bus.read32(pte_addr);
            if pte & 1 == 0 {
                return pf(self.cpu, false);
            }
            ((pte & !0xfff) | (lin & 0xfff), pde & pte, pte_addr, pte)
        };

        let writable = perms & 2 != 0;
        let user_ok = perms & 4 != 0;
        if user && !user_ok {
            return pf(self.cpu, true);
        }
        if write && !writable && (user || self.cpu.cr0 & cr0::WP != 0) {
            return pf(self.cpu, true);
        }

        if !big && pde & 0x20 == 0 {
            pde |= 0x20;
            self.bus.write32(pde_addr, pde);
        }
        let wanted = entry | 0x20 | if write { 0x40 } else { 0 };
        if wanted != entry {
            entry = wanted;
            self.bus.write32(entry_addr, entry);
        }
        Ok(phys)
    }

    /// Translates the first and last byte of an access; returns both physical
    /// addresses (equal pages when the access does not cross a page).
    fn translate_range(&mut self, lin: u32, sz: u32, acc: Access, user: bool) -> Res<(u32, u32)> {
        let first = self.translate(lin, acc, user)?;
        let end = lin.wrapping_add(sz - 1);
        if (lin ^ end) & !0xfff == 0 {
            return Ok((first, first.wrapping_add(sz - 1)));
        }
        let last = self.translate(end, acc, user)?;
        Ok((first, last))
    }

    pub fn read_lin(&mut self, lin: u32, sz: u32, user: bool, acc: Access) -> Res<u32> {
        let (first, last) = self.translate_range(lin, sz, acc, user)?;
        if last.wrapping_sub(first) == sz - 1 {
            return Ok(match sz {
                1 => self.bus.read8(first) as u32,
                2 => self.bus.read16(first) as u32,
                _ => self.bus.read32(first),
            });
        }
        let mut v = 0;
        for i in 0..sz {
            let a = lin.wrapping_add(i);
            let p = if (a ^ lin) & !0xfff == 0 { first + i } else { (last & !0xfff) | (a & 0xfff) };
            v |= (self.bus.read8(p) as u32) << (8 * i);
        }
        Ok(v)
    }

    pub fn write_lin(&mut self, lin: u32, sz: u32, v: u32, user: bool) -> Res<()> {
        let (first, last) = self.translate_range(lin, sz, Access::Write, user)?;
        if last.wrapping_sub(first) == sz - 1 {
            match sz {
                1 => self.bus.write8(first, v as u8),
                2 => self.bus.write16(first, v as u16),
                _ => self.bus.write32(first, v),
            }
            return Ok(());
        }
        for i in 0..sz {
            let a = lin.wrapping_add(i);
            let p = if (a ^ lin) & !0xfff == 0 { first + i } else { (last & !0xfff) | (a & 0xfff) };
            self.bus.write8(p, (v >> (8 * i)) as u8);
        }
        Ok(())
    }

    /// Supervisor-mode linear accesses for descriptor tables and the TSS.
    pub fn read_sys(&mut self, lin: u32, sz: u32) -> Res<u32> {
        self.read_lin(lin, sz, false, Access::Read)
    }

    pub fn write_sys(&mut self, lin: u32, sz: u32, v: u32) -> Res<()> {
        self.write_lin(lin, sz, v, false)
    }

    pub fn fetch8(&mut self, off: u32) -> Res<u8> {
        let lin = self.seg_check(crate::cpu::CS, off, 1, Access::Fetch)?;
        Ok(self.read_lin(lin, 1, self.cpu.cpl() == 3, Access::Fetch)? as u8)
    }

    pub fn stack32(&self) -> bool {
        self.cpu.segs[SS].big()
    }

    pub fn sp(&self) -> u32 {
        if self.stack32() { self.cpu.regs[4] } else { self.cpu.regs[4] & 0xffff }
    }

    pub fn set_sp(&mut self, v: u32) {
        if self.stack32() {
            self.cpu.regs[4] = v;
        } else {
            self.cpu.set_reg16(4, v as u16);
        }
    }

    fn sp_offset(&self, delta: u32) -> u32 {
        let v = self.sp().wrapping_add(delta);
        if self.stack32() { v } else { v & 0xffff }
    }

    pub fn push(&mut self, v: u32, sz: u32) -> Res<()> {
        let new_sp = self.sp_offset(sz.wrapping_neg());
        self.write_seg(SS, new_sp, sz, v)?;
        self.set_sp(new_sp);
        Ok(())
    }

    pub fn pop(&mut self, sz: u32) -> Res<u32> {
        let v = self.read_seg(SS, self.sp(), sz)?;
        self.set_sp(self.sp_offset(sz));
        Ok(v)
    }

    /// Reads the stack at `sp + delta` without moving SP.
    pub fn peek(&mut self, delta: u32, sz: u32) -> Res<u32> {
        let off = self.sp_offset(delta);
        self.read_seg(SS, off, sz)
    }

    pub fn check_io(&mut self, port: u16, sz: u32) -> Res<()> {
        let cpu = &self.cpu;
        if !cpu.protected_mode() || (!cpu.v86_mode() && cpu.cpl() <= cpu.iopl()) {
            return Ok(());
        }
        let tr = self.cpu.tr;
        let tss32 = tr.attr & 0xf == 9 || tr.attr & 0xf == 11;
        if !tss32 || tr.limit < 0x67 {
            return gp(0);
        }
        let iomap = self.read_sys(tr.base.wrapping_add(0x66), 2)?;
        let byte = iomap + port as u32 / 8;
        if byte + 1 > tr.limit {
            return gp(0);
        }
        let bits = self.read_sys(tr.base.wrapping_add(byte), 2)?;
        let want = ((1u32 << sz) - 1) << (port % 8);
        if bits & want != 0 {
            return gp(0);
        }
        Ok(())
    }
}

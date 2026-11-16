use super::mem::Loc;
use super::{Ctx, Res, alu};
use crate::bus::Bus;
use crate::cpu::{DS, ECX, EDI, EDX, ES, ESI, flags};
use crate::decode::Rep;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrOp {
    Movs,
    Cmps,
    Stos,
    Lods,
    Scas,
    Ins,
    Outs,
}

impl<B: Bus> Ctx<'_, B> {
    fn index(&self, r: usize) -> u32 {
        if self.insn.addr32 { self.cpu.regs[r] } else { self.cpu.regs[r] & 0xffff }
    }

    fn advance(&mut self, r: usize, sz: u32) {
        let delta = if self.cpu.flag(flags::DF) { sz.wrapping_neg() } else { sz };
        if self.insn.addr32 {
            self.cpu.regs[r] = self.cpu.regs[r].wrapping_add(delta);
        } else {
            let v = (self.cpu.regs[r] as u16).wrapping_add(delta as u16);
            self.cpu.set_reg16(r, v);
        }
    }

    pub fn string_op(&mut self, op: StrOp, sz: u32) -> Res<()> {
        let rep = self.insn.rep;
        let src_seg = self.insn.seg_override.map_or(DS, |s| s as usize);
        if matches!(op, StrOp::Ins | StrOp::Outs) {
            self.check_io(self.cpu.reg16(EDX), sz)?;
        }
        loop {
            if rep != Rep::None && self.index(ECX) == 0 {
                return Ok(());
            }
            match op {
                StrOp::Movs => {
                    let v = self.read_seg(src_seg, self.index(ESI), sz)?;
                    self.write_seg(ES, self.index(EDI), sz, v)?;
                    self.advance(ESI, sz);
                    self.advance(EDI, sz);
                }
                StrOp::Cmps => {
                    let a = self.read_seg(src_seg, self.index(ESI), sz)?;
                    let b = self.read_seg(ES, self.index(EDI), sz)?;
                    let (_, f) = alu::sub(a, b, false, sz);
                    self.set_arith(f);
                    self.advance(ESI, sz);
                    self.advance(EDI, sz);
                }
                StrOp::Stos => {
                    let v = self.get_reg(0, sz);
                    self.write_seg(ES, self.index(EDI), sz, v)?;
                    self.advance(EDI, sz);
                }
                StrOp::Lods => {
                    let v = self.read_seg(src_seg, self.index(ESI), sz)?;
                    self.set_reg(0, sz, v);
                    self.advance(ESI, sz);
                }
                StrOp::Scas => {
                    let b = self.read_seg(ES, self.index(EDI), sz)?;
                    let (_, f) = alu::sub(self.get_reg(0, sz), b, false, sz);
                    self.set_arith(f);
                    self.advance(EDI, sz);
                }
                StrOp::Ins => {
                    self.probe_write(Loc::Mem(ES, self.index(EDI)), sz)?;
                    let v = self.bus.io_read(self.cpu.reg16(EDX), sz as u8);
                    self.write_seg(ES, self.index(EDI), sz, v)?;
                    self.advance(EDI, sz);
                }
                StrOp::Outs => {
                    let v = self.read_seg(src_seg, self.index(ESI), sz)?;
                    self.bus.io_write(self.cpu.reg16(EDX), sz as u8, v);
                    self.advance(ESI, sz);
                }
            }
            if rep == Rep::None {
                return Ok(());
            }
            let count = self.index(ECX).wrapping_sub(1);
            if self.insn.addr32 {
                self.cpu.regs[ECX] = count;
            } else {
                self.cpu.set_reg16(ECX, count as u16);
            }
            if matches!(op, StrOp::Cmps | StrOp::Scas) {
                let zf = self.cpu.flag(flags::ZF);
                if (rep == Rep::Rep && !zf) || (rep == Rep::Repne && zf) {
                    return Ok(());
                }
            }
        }
    }
}

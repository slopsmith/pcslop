//! Reference interpreter. It favours clarity over speed: it is the correctness
//! oracle for the JIT and the fallback for rare instructions and modes.

mod alu;
mod mem;
mod ops;
mod simd;
mod string;
mod system;
mod x87;

use crate::bus::Bus;
use crate::cpu::{Cpu, flags};
use crate::decode::Insn;

pub mod vector {
    pub const DE: u8 = 0;
    pub const DB: u8 = 1;
    pub const NMI: u8 = 2;
    pub const BP: u8 = 3;
    pub const OF: u8 = 4;
    pub const BR: u8 = 5;
    pub const UD: u8 = 6;
    pub const NM: u8 = 7;
    pub const DF: u8 = 8;
    pub const TS: u8 = 10;
    pub const NP: u8 = 11;
    pub const SS: u8 = 12;
    pub const GP: u8 = 13;
    pub const PF: u8 = 14;
    pub const MF: u8 = 16;
    pub const AC: u8 = 17;
    pub const XM: u8 = 19;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exception {
    pub vector: u8,
    pub error: Option<u32>,
}

impl Exception {
    pub const fn new(vector: u8) -> Self {
        Exception { vector, error: None }
    }

    pub const fn with_error(vector: u8, error: u32) -> Self {
        Exception { vector, error: Some(error) }
    }

    fn contributory(&self) -> bool {
        matches!(self.vector, vector::DE | vector::TS | vector::NP | vector::SS | vector::GP)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Trap {
    Fault(Exception),
    Unimplemented(String),
}

impl From<Exception> for Trap {
    fn from(e: Exception) -> Self {
        Trap::Fault(e)
    }
}

pub(crate) type Res<T> = Result<T, Trap>;

pub(crate) fn ud<T>() -> Res<T> {
    Err(Exception::new(vector::UD).into())
}

pub(crate) fn gp<T>(error: u32) -> Res<T> {
    Err(Exception::with_error(vector::GP, error).into())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// One instruction (or interrupt delivery) completed.
    Continue,
    /// The CPU is halted and no interrupt is pending.
    Halted,
    /// Triple fault.
    Shutdown,
    /// The interpreter hit something it does not implement yet.
    Unimplemented { eip: u32, what: String },
}

pub(crate) struct Ctx<'a, B: Bus> {
    pub cpu: &'a mut Cpu,
    pub bus: &'a mut B,
    pub insn: Insn,
    /// EIP of the instruction being executed.
    pub start_eip: u32,
}

/// Executes one instruction, or delivers one pending interrupt.
pub fn step<B: Bus>(cpu: &mut Cpu, bus: &mut B) -> Exit {
    let mut ctx = Ctx { cpu, bus, insn: Insn::default(), start_eip: 0 };
    ctx.step()
}

/// Translates `lin` for an access at the current privilege level, setting the
/// Accessed and Dirty bits as the CPU would. A failed translation sets CR2 and
/// returns the page fault to deliver.
pub fn translate<B: Bus>(cpu: &mut Cpu, bus: &mut B, lin: u32, write: bool) -> Result<u32, Exception> {
    translate_global(cpu, bus, lin, write).map(|(p, _)| p)
}

/// Like `translate`, also reporting whether the mapping is global.
pub fn translate_global<B: Bus>(cpu: &mut Cpu, bus: &mut B, lin: u32, write: bool) -> Result<(u32, bool), Exception> {
    let mut ctx = Ctx { cpu, bus, insn: Insn::default(), start_eip: 0 };
    let user = ctx.cpu.cpl() == 3;
    let acc = if write { mem::Access::Write } else { mem::Access::Read };
    match ctx.translate_full(lin, acc, user) {
        Ok((p, perms)) => Ok((p, perms & crate::cpu::tlb_perm::GLOBAL != 0)),
        Err(Trap::Fault(e)) => Err(e),
        Err(Trap::Unimplemented(_)) => unreachable!("page walks are fully implemented"),
    }
}

/// Loads data segment register `seg` with `sel` as MOV would, reading the descriptor.
pub fn load_segment<B: Bus>(cpu: &mut Cpu, bus: &mut B, seg: usize, sel: u16) -> bool {
    let mut ctx = Ctx { cpu, bus, insn: Insn::default(), start_eip: 0 };
    ctx.load_seg(seg, sel).is_ok()
}

/// Delivers `e` as a fault raised by the instruction at `cpu.eip`.
pub fn deliver_fault<B: Bus>(cpu: &mut Cpu, bus: &mut B, e: Exception) -> Exit {
    let mut ctx = Ctx { cpu, bus, insn: Insn::default(), start_eip: 0 };
    ctx.start_eip = ctx.cpu.eip;
    ctx.deliver(e, Source::Fault)
}

impl<B: Bus> Ctx<'_, B> {
    fn step(&mut self) -> Exit {
        if self.cpu.interrupt_shadow {
            self.cpu.interrupt_shadow = false;
        } else if self.cpu.flag(flags::IF) && self.bus.intr() {
            let vec = self.bus.inta();
            self.cpu.halted = false;
            self.start_eip = self.cpu.eip;
            return self.deliver(Exception::new(vec), Source::External);
        }
        if self.cpu.halted {
            return Exit::Halted;
        }

        self.start_eip = self.cpu.eip;
        let single_step = self.cpu.flag(flags::TF);
        match self.exec() {
            Ok(()) => {
                if single_step {
                    self.cpu.dr[6] |= 1 << 14;
                    return self.deliver(Exception::new(vector::DB), Source::External);
                }
                Exit::Continue
            }
            Err(Trap::Fault(e)) => {
                self.cpu.eip = self.start_eip;
                self.deliver(e, Source::Fault)
            }
            Err(Trap::Unimplemented(what)) => {
                self.cpu.eip = self.start_eip;
                Exit::Unimplemented { eip: self.start_eip, what }
            }
        }
    }

    fn deliver(&mut self, e: Exception, source: Source) -> Exit {
        let first = match self.raise(e, source) {
            Ok(()) => return Exit::Continue,
            Err(Trap::Unimplemented(what)) => return Exit::Unimplemented { eip: self.cpu.eip, what },
            Err(Trap::Fault(second)) => second,
        };
        let second_bad = first.contributory() || first.vector == vector::PF;
        let df = (e.contributory() && first.contributory()) || (e.vector == vector::PF && second_bad);
        if e.vector == vector::DF {
            return Exit::Shutdown;
        }
        let next = if df { Exception::with_error(vector::DF, 0) } else { first };
        match self.raise(next, Source::Fault) {
            Ok(()) => Exit::Continue,
            Err(Trap::Unimplemented(what)) => Exit::Unimplemented { eip: self.cpu.eip, what },
            Err(Trap::Fault(_)) => Exit::Shutdown,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// A fault: the saved EIP is the faulting instruction.
    Fault,
    /// An external interrupt, or a trap that already advanced EIP.
    External,
    /// `INT n`, `INT3` or `INTO`: subject to gate DPL and V86 IOPL checks.
    Software,
}

#[cfg(test)]
mod tests;

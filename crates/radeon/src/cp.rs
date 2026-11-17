//! The command processor: the ring buffer, indirect buffers, scratch
//! registers and PM4 packets. We interpret the packets ourselves and ignore
//! the microcode that drivers upload. Packet formats follow AMD's R5xx
//! Acceleration guide (v1.5, chapters 5 and 6).

use crate::mem::BusMemory;
use crate::regs::*;
use crate::{Radeon, Source};

/// The largest ring we follow, in dwords: CP_RB_CNTL allows 2^27 bytes.
const MAX_RING_DWORDS: u32 = 1 << 22;
/// Indirect buffers nest once, for IB2.
const MAX_IB_DEPTH: u8 = 2;
const MAX_IB_DWORDS: u32 = 1 << 20;

pub(crate) mod op {
    pub const NOP: u8 = 0x10;
    pub const NEXTCHAR: u8 = 0x19;
    pub const SET_SCISSORS: u8 = 0x1e;
    pub const PAINT: u8 = 0x91;
    pub const BITBLT: u8 = 0x92;
    pub const HOSTDATA_BLT: u8 = 0x94;
    pub const POLYLINE: u8 = 0x95;
    pub const POLYSCANLINES: u8 = 0x98;
    pub const PAINT_MULTI: u8 = 0x9a;
    pub const BITBLT_MULTI: u8 = 0x9b;
    pub const TRANS_BITBLT: u8 = 0x9c;
}

#[derive(Default)]
pub(crate) struct Cp {
    pub rptr: u32,
    /// Indirect buffers being executed, outermost first.
    pub ib_depth: u8,
    pub packets: u64,
    /// Type-3 opcodes we skipped, so each is traced once.
    pub skipped: [u64; 4],
}

impl Radeon {
    fn ring_dwords(&self) -> u32 {
        (2u32 << (self.reg(CP_RB_CNTL) & 0x3f).min(26)).min(MAX_RING_DWORDS)
    }

    /// The primary stream is fetched by bus mastering: CP_CSQ_MODE, when it
    /// enables the primary stream, overrides CP_CSQ_CNTL's mode.
    fn primary_bus_master(&self) -> bool {
        let mode = self.reg(CP_CSQ_MODE);
        if mode & CSQ_PRIMARY_ENABLE != 0 {
            return mode & CSQ_PRIMARY_BM != 0;
        }
        matches!(self.reg(CP_CSQ_CNTL) >> 28, 2 | 4 | 6 | 8)
    }

    pub(crate) fn cp_rptr(&self) -> u32 {
        self.cp.rptr
    }

    pub(crate) fn cp_write_wptr(&mut self, mem: &mut dyn BusMemory) {
        if self.reg(CP_RB_CNTL) & RB_RPTR_WR_ENA != 0 {
            self.cp.rptr = self.reg(CP_RB_RPTR_WR) & (self.ring_dwords() - 1);
        }
        self.run_ring(mem);
    }

    pub(crate) fn cp_soft_reset(&mut self) {
        self.cp.rptr = 0;
        self.engine.host = None;
    }

    /// Executes the ring from CP_RB_RPTR up to CP_RB_WPTR.
    pub(crate) fn run_ring(&mut self, mem: &mut dyn BusMemory) {
        if !self.primary_bus_master() || self.cp.ib_depth > 0 {
            return;
        }
        let size = self.ring_dwords();
        let wptr = self.reg(CP_RB_WPTR) & (size - 1);
        let rptr = self.cp.rptr % size;
        if rptr == wptr {
            return;
        }
        let base = self.reg(CP_RB_BASE) & !3;
        let count = (wptr + size - rptr) % size;
        let mut dwords = Vec::with_capacity(count as usize);
        let first = count.min(size - rptr);
        dwords.extend(self.mc_read_dwords(mem, base.wrapping_add(rptr * 4), first as usize));
        dwords.extend(self.mc_read_dwords(mem, base, (count - first) as usize));
        let done = self.execute(mem, &dwords);
        self.cp.rptr = (rptr + done as u32) % size;
        if self.reg(CP_RB_CNTL) & RB_NO_UPDATE == 0 {
            let addr = self.reg(CP_RB_RPTR_ADDR) & !3;
            let rptr = self.cp.rptr;
            self.mc_write_dword(mem, addr, rptr);
        }
    }

    /// Runs the indirect buffer at CP_IB_BASE, CP_IB_BUFSZ dwords long.
    fn run_indirect(&mut self, mem: &mut dyn BusMemory) {
        if self.cp.ib_depth >= MAX_IB_DEPTH {
            return;
        }
        let base = self.reg(CP_IB_BASE) & !3;
        let size = (self.reg(CP_IB_BUFSZ) & 0x7f_ffff).min(MAX_IB_DWORDS);
        let dwords = self.mc_read_dwords(mem, base, size as usize);
        self.cp.ib_depth += 1;
        self.execute(mem, &dwords);
        self.cp.ib_depth -= 1;
    }

    /// Executes whole packets from `d` and returns how many dwords they took.
    pub(crate) fn execute(&mut self, mem: &mut dyn BusMemory, d: &[u32]) -> usize {
        let mut i = 0;
        while i < d.len() {
            let h = d[i];
            let count = ((h >> 16) & 0x3fff) as usize + 1;
            let len = match h >> 30 {
                0 | 3 => 1 + count,
                1 => 3,
                _ => 1,
            };
            if i + len > d.len() {
                break;
            }
            let body = &d[i + 1..i + len];
            match h >> 30 {
                0 => {
                    let reg = (h & 0x1fff) << 2;
                    let one = h & 0x8000 != 0;
                    for (k, &v) in body.iter().enumerate() {
                        let r = if one { reg } else { reg + 4 * k as u32 };
                        self.cp_reg_write(mem, r, v);
                    }
                }
                1 => {
                    self.cp_reg_write(mem, (h & 0x7ff) << 2, body[0]);
                    self.cp_reg_write(mem, ((h >> 11) & 0x7ff) << 2, body[1]);
                }
                2 => {}
                _ => self.packet3(mem, (h >> 8) as u8, body),
            }
            self.cp.packets += 1;
            i += len;
        }
        i
    }

    fn cp_reg_write(&mut self, mem: &mut dyn BusMemory, reg: u32, v: u32) {
        if reg >= crate::MMIO_SIZE {
            return;
        }
        self.trace_reg(Source::Cp, 'W', 4, reg, v);
        self.write_reg(reg, v, u32::MAX, mem);
    }

    fn packet3(&mut self, mem: &mut dyn BusMemory, opcode: u8, body: &[u32]) {
        match opcode {
            op::NOP => {}
            op::NEXTCHAR => self.next_char(body),
            op::SET_SCISSORS => {
                for (reg, &v) in [SC_TOP_LEFT, SC_BOTTOM_RIGHT].into_iter().zip(body) {
                    self.cp_reg_write(mem, reg, v);
                }
            }
            op::PAINT
            | op::BITBLT
            | op::HOSTDATA_BLT
            | op::POLYLINE
            | op::POLYSCANLINES
            | op::PAINT_MULTI
            | op::BITBLT_MULTI
            | op::TRANS_BITBLT => self.packet_2d(mem, opcode, body),
            _ => {
                let (word, bit) = (opcode as usize / 64, 1u64 << (opcode % 64));
                if self.cp.skipped[word] & bit == 0 {
                    self.cp.skipped[word] |= bit;
                    self.note(&format!(
                        "cp: skipped type-3 opcode {opcode:#04x} ({} dwords)",
                        body.len()
                    ));
                }
            }
        }
    }

    fn scratch_written(&mut self, n: u32, mem: &mut dyn BusMemory) {
        if self.reg(SCRATCH_UMSK) & 1 << n != 0 {
            let addr = (self.reg(SCRATCH_ADDR) & !3).wrapping_add(4 * n);
            let v = self.reg(SCRATCH_REG0 + 4 * n);
            self.mc_write_dword(mem, addr, v);
        }
    }

    /// Side effects of writing a CP register; false if `reg` has none.
    pub(crate) fn cp_register_written(&mut self, reg: u32, mem: &mut dyn BusMemory) -> bool {
        match reg {
            CP_RB_WPTR => self.cp_write_wptr(mem),
            CP_CSQ_CNTL | CP_CSQ_MODE => self.run_ring(mem),
            CP_IB_BUFSZ => self.run_indirect(mem),
            SCRATCH_REG0..=SCRATCH_REG7 => self.scratch_written((reg - SCRATCH_REG0) / 4, mem),
            CP_RESYNC_DATA => {
                let n = self.reg(CP_RESYNC_ADDR) & 7;
                self.regs[(SCRATCH_REG0 / 4 + n) as usize] = self.reg(CP_RESYNC_DATA);
                self.scratch_written(n, mem);
            }
            RBBM_SOFT_RESET => {
                if self.reg(reg) & (SOFT_RESET_CP | SOFT_RESET_E2) != 0 {
                    self.cp_soft_reset();
                }
            }
            _ => return false,
        }
        true
    }
}

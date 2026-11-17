//! The hardware I2C engine behind I2C_CNTL_0, I2C_CNTL_1 and I2C_DATA. It runs
//! whole transfers to the monitor's EDID at once. The register layout and the
//! way drivers use it follow xf86-video-ati's radeon_reg.h and radeon_mm_i2c.c
//! (MIT license; ATI Technologies Inc. and others).

use std::collections::VecDeque;

use crate::ddc::EDID_ADDRESS;
use crate::regs::*;

#[derive(Default)]
pub(crate) struct I2cEngine {
    tx: VecDeque<u8>,
    rx: VecDeque<u8>,
    offset: u8,
    pub transfers: u64,
}

impl I2cEngine {
    pub fn push(&mut self, byte: u8) {
        self.tx.push_back(byte);
    }

    pub fn pop(&mut self) -> u8 {
        self.rx.pop_front().unwrap_or(0xff)
    }

    /// Applies a write of `cntl0` and returns the value I2C_CNTL_0 then holds.
    pub fn control(&mut self, cntl0: u32, cntl1: u32, edid: &[u8; 128]) -> u32 {
        let mut v = cntl0;
        if v & I2C_SOFT_RST != 0 {
            self.tx.clear();
            self.rx.clear();
            v &= !(I2C_SOFT_RST | I2C_DONE | I2C_NACK | I2C_HALT);
        }
        if v & I2C_GO != 0 {
            v &= !(I2C_DONE | I2C_NACK | I2C_HALT);
            if v & I2C_ABORT == 0 {
                v |= self.transfer(v, cntl1, edid);
            }
            v &= !(I2C_GO | I2C_ABORT);
        }
        v
    }

    fn transfer(&mut self, cntl0: u32, cntl1: u32, edid: &[u8; 128]) -> u32 {
        self.transfers += 1;
        let Some(addr) = self.tx.pop_front() else { return I2C_NACK };
        if addr >> 1 != EDID_ADDRESS {
            self.tx.clear();
            return I2C_NACK;
        }
        if addr & 1 != 0 || cntl0 & I2C_RECEIVE != 0 {
            for _ in 0..cntl1 & 0xf {
                self.rx.push_back(edid[self.offset as usize % edid.len()]);
                self.offset = self.offset.wrapping_add(1);
            }
        } else if let Some(&offset) = self.tx.front() {
            self.offset = offset;
        }
        self.tx.clear();
        I2C_DONE
    }
}

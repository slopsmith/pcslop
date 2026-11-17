//! A floppy controller (Intel 82077AA) with no drives attached. Without one,
//! Windows' floppy driver waits four seconds at boot for the interrupt that
//! follows a controller reset. Result bytes after a reset follow QEMU's
//! `hw/block/fdc.c` (the QEMU project authors).

use std::collections::VecDeque;

const MSR_RQM: u8 = 0x80;
const MSR_DIO: u8 = 0x40;
const MSR_BUSY: u8 = 0x10;
const DOR_RESET_OFF: u8 = 0x04;
const DOR_IRQ_DMA: u8 = 0x08;
/// SENSE INTERRUPT answers owed after a reset, one per drive.
const RESET_SENSES: u8 = 4;
/// ST0 for a command on a missing drive: abnormal termination, equipment check.
const ST0_NO_DRIVE: u8 = 0x40 | 0x10;

#[derive(Default)]
pub struct Fdc {
    dor: u8,
    tdr: u8,
    command: Vec<u8>,
    result: VecDeque<u8>,
    reset_senses: u8,
    /// ST0 of a finished RECALIBRATE or SEEK, for the next SENSE INTERRUPT.
    seek_st0: Option<u8>,
    irq: bool,
}

/// Bytes in a command, including the command byte, by its low five bits.
fn command_len(cmd: u8) -> usize {
    match cmd & 0x1f {
        0x02 | 0x05 | 0x06 | 0x09 | 0x0c | 0x11 | 0x16 | 0x19 | 0x1d => 9,
        0x0d => 6,
        0x13 => 4,
        0x03 | 0x0f => 3,
        0x04 | 0x07 | 0x0a | 0x12 => 2,
        _ => 1,
    }
}

impl Fdc {
    /// True while the controller requests IRQ 6.
    pub fn irq(&self) -> bool {
        self.irq && self.dor & DOR_IRQ_DMA != 0
    }

    pub fn read(&mut self, port: u16) -> u8 {
        match port & 7 {
            2 => self.dor,
            3 => self.tdr,
            4 => {
                if !self.result.is_empty() {
                    MSR_RQM | MSR_DIO | MSR_BUSY
                } else if !self.command.is_empty() {
                    MSR_RQM | MSR_BUSY
                } else {
                    MSR_RQM
                }
            }
            5 => self.result.pop_front().unwrap_or(0),
            _ => 0xff,
        }
    }

    pub fn write(&mut self, port: u16, v: u8) {
        match port & 7 {
            2 => {
                let leaving_reset = self.dor & DOR_RESET_OFF == 0 && v & DOR_RESET_OFF != 0;
                self.dor = v;
                if v & DOR_RESET_OFF == 0 {
                    self.command.clear();
                    self.result.clear();
                    self.irq = false;
                } else if leaving_reset {
                    self.reset();
                }
            }
            3 => self.tdr = v,
            4 if v & 0x80 != 0 => self.reset(),
            5 => {
                self.result.clear();
                self.command.push(v);
                if self.command.len() >= command_len(self.command[0]) {
                    let cmd = std::mem::take(&mut self.command);
                    self.execute(&cmd);
                }
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.command.clear();
        self.result.clear();
        self.seek_st0 = None;
        self.reset_senses = RESET_SENSES;
        self.irq = true;
    }

    fn execute(&mut self, cmd: &[u8]) {
        let drive = cmd.get(1).map_or(0, |b| b & 3);
        match cmd[0] & 0x1f {
            0x08 => {
                self.irq = false;
                if self.reset_senses > 0 {
                    let d = RESET_SENSES - self.reset_senses;
                    self.reset_senses -= 1;
                    self.result.extend([0xc0 | d, 0]);
                } else if let Some(st0) = self.seek_st0.take() {
                    self.result.extend([st0, 0]);
                } else {
                    self.result.push_back(0x80);
                }
            }
            0x07 | 0x0f => {
                self.seek_st0 = Some(ST0_NO_DRIVE | 0x20 | drive);
                self.irq = true;
            }
            0x04 => self.result.push_back(drive),
            0x10 => self.result.push_back(0x90),
            0x14 => self.result.push_back((cmd[0] >> 7) << 4),
            0x18 => self.result.push_back(0x41),
            0x0e => self.result.extend([0; 10]),
            0x03 | 0x12 | 0x13 => {}
            0x02 | 0x05 | 0x06 | 0x09 | 0x0a | 0x0c | 0x0d | 0x11 | 0x16 | 0x19 | 0x1d => {
                let chrn = if cmd.len() >= 6 { [cmd[2], cmd[3], cmd[4], cmd[5]] } else { [0; 4] };
                self.result.extend([ST0_NO_DRIVE | drive, 0x01, 0]);
                self.result.extend(chrn);
                self.irq = true;
            }
            _ => self.result.push_back(0x80),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sense(f: &mut Fdc) -> Vec<u8> {
        f.write(0x3f5, 0x08);
        let mut out = Vec::new();
        while f.read(0x3f4) & MSR_DIO != 0 {
            out.push(f.read(0x3f5));
        }
        out
    }

    #[test]
    fn reset_raises_the_interrupt_and_answers_four_senses() {
        let mut f = Fdc::default();
        f.write(0x3f2, 0x08);
        f.write(0x3f2, 0x0c);
        assert!(f.irq());
        for d in 0..4 {
            assert_eq!(sense(&mut f), [0xc0 | d, 0]);
        }
        assert!(!f.irq());
        assert_eq!(sense(&mut f), [0x80]);
    }

    #[test]
    fn recalibrate_reports_a_missing_drive() {
        let mut f = Fdc::default();
        f.write(0x3f2, 0x0c);
        for _ in 0..4 {
            sense(&mut f);
        }
        f.write(0x3f5, 0x07);
        assert_eq!(f.read(0x3f4) & MSR_BUSY, MSR_BUSY);
        f.write(0x3f5, 0x01);
        assert!(f.irq());
        assert_eq!(sense(&mut f), [0x71, 0]);
    }

    #[test]
    fn version_says_82077() {
        let mut f = Fdc::default();
        f.write(0x3f5, 0x10);
        assert_eq!(f.read(0x3f5), 0x90);
        assert_eq!(f.read(0x3f4), MSR_RQM);
    }
}

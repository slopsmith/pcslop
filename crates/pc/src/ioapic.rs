//! Intel 82093AA I/O APIC (datasheet 290566-001): 24 interrupt inputs, each
//! turned into a message for the local APICs by its redirection entry.

use crate::apic::Message;

pub const BASE: u32 = 0xfec0_0000;
pub const SIZE: u32 = 0x1000;
pub const PINS: usize = 24;

const IOREGSEL: u32 = 0x00;
const IOWIN: u32 = 0x10;
/// Version 0x11, highest redirection entry 23.
const VERSION: u32 = 0x0017_0011;

pub mod redir {
    pub const REMOTE_IRR: u64 = 1 << 14;
    pub const LEVEL: u64 = 1 << 15;
    pub const MASKED: u64 = 1 << 16;
    /// Delivery status and remote IRR are read-only.
    pub const READ_ONLY: u64 = 1 << 12 | REMOTE_IRR;
    pub const WRITABLE_HI: u64 = 0xff << 56;
}

#[derive(snapshot::Snap)]
pub struct IoApic {
    id: u8,
    select: u8,
    redir: [u64; PINS],
    /// Input levels, for edge detection and level-triggered requests.
    levels: u32,
    /// Edges seen on unmasked edge-triggered inputs, not yet delivered.
    edges: u32,
}

impl Default for IoApic {
    fn default() -> Self {
        IoApic { id: 0, select: 0, redir: [redir::MASKED; PINS], levels: 0, edges: 0 }
    }
}

impl IoApic {
    /// Drives input `pin`. Edge-triggered inputs latch a rising edge, unless
    /// masked: the datasheet says those edges are ignored.
    pub fn set_irq(&mut self, pin: u8, level: bool) {
        let bit = 1 << pin;
        let e = self.redir[pin as usize];
        if level && self.levels & bit == 0 && e & (redir::LEVEL | redir::MASKED) == 0 {
            self.edges |= bit;
        }
        if level {
            self.levels |= bit;
        } else {
            self.levels &= !bit;
        }
    }

    pub fn entry(&self, pin: usize) -> u64 {
        self.redir[pin]
    }

    /// Sends every request it can to `deliver`, which returns whether a local
    /// APIC took the message. A level-triggered request sets remote IRR and
    /// is not sent again until an EOI clears it.
    pub fn service(&mut self, mut deliver: impl FnMut(Message) -> bool) {
        for pin in 0..PINS {
            let bit = 1 << pin;
            let e = self.redir[pin];
            if e & redir::MASKED != 0 {
                continue;
            }
            let level = e & redir::LEVEL != 0;
            let ready = if level { self.levels & bit != 0 && e & redir::REMOTE_IRR == 0 } else { self.edges & bit != 0 };
            if !ready {
                continue;
            }
            let m = Message { vector: e as u8, mode: ((e >> 8) & 7) as u8, dest: (e >> 56) as u8, logical: e & 1 << 11 != 0, level };
            if deliver(m) {
                if level {
                    self.redir[pin] |= redir::REMOTE_IRR;
                } else {
                    self.edges &= !bit;
                }
            }
        }
    }

    /// A local APIC ended level-triggered `vector`.
    pub fn eoi(&mut self, vector: u8) {
        for e in &mut self.redir {
            if *e & redir::LEVEL != 0 && *e as u8 == vector {
                *e &= !redir::REMOTE_IRR;
            }
        }
    }

    pub fn read(&self, offset: u32) -> u32 {
        match offset & 0xff {
            IOREGSEL => self.select as u32,
            IOWIN => match self.select {
                0x00 | 0x02 => (self.id as u32) << 24,
                0x01 => VERSION,
                r @ 0x10..=0x3f => {
                    let e = self.redir[(r as usize - 0x10) / 2];
                    if r & 1 == 0 { e as u32 } else { (e >> 32) as u32 }
                }
                _ => 0,
            },
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u32, v: u32) {
        match offset & 0xff {
            IOREGSEL => self.select = v as u8,
            IOWIN => match self.select {
                0x00 => self.id = ((v >> 24) & 0xf) as u8,
                r @ 0x10..=0x3f => {
                    let pin = (r as usize - 0x10) / 2;
                    let e = &mut self.redir[pin];
                    if r & 1 == 0 {
                        *e = (*e & (redir::READ_ONLY | 0xffff_ffff << 32)) | (v as u64 & !redir::READ_ONLY);
                        if *e & redir::LEVEL == 0 {
                            *e &= !redir::REMOTE_IRR;
                        }
                        if *e & redir::MASKED != 0 {
                            self.edges &= !(1 << pin);
                        }
                    } else {
                        *e = (*e & 0xffff_ffff) | ((v as u64) << 32 & redir::WRITABLE_HI);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(io: &mut IoApic, pin: usize, lo: u32, hi: u32) {
        io.write(IOREGSEL, 0x10 + 2 * pin as u32 + 1);
        io.write(IOWIN, hi);
        io.write(IOREGSEL, 0x10 + 2 * pin as u32);
        io.write(IOWIN, lo);
    }

    fn collect(io: &mut IoApic) -> Vec<Message> {
        let mut out = Vec::new();
        io.service(|m| {
            out.push(m);
            true
        });
        out
    }

    #[test]
    fn reports_version_and_reset_masks() {
        let mut io = IoApic::default();
        io.write(IOREGSEL, 1);
        assert_eq!(io.read(IOWIN), 0x0017_0011);
        io.write(IOREGSEL, 0x10);
        assert_eq!(io.read(IOWIN), 1 << 16);
    }

    #[test]
    fn edge_input_sends_one_message_per_rising_edge() {
        let mut io = IoApic::default();
        program(&mut io, 2, 0x830, 0x0100_0000);
        io.set_irq(2, true);
        let m = collect(&mut io);
        assert_eq!(m, vec![Message { vector: 0x30, mode: 0, dest: 1, logical: true, level: false }]);
        io.set_irq(2, true);
        assert!(collect(&mut io).is_empty());
        io.set_irq(2, false);
        io.set_irq(2, true);
        assert_eq!(collect(&mut io).len(), 1);
    }

    #[test]
    fn masked_edge_input_is_ignored() {
        let mut io = IoApic::default();
        program(&mut io, 4, 0x1_0034, 0);
        io.set_irq(4, true);
        io.set_irq(4, false);
        program(&mut io, 4, 0x34, 0);
        assert!(collect(&mut io).is_empty());
    }

    #[test]
    fn level_input_waits_for_eoi_and_repeats_while_asserted() {
        let mut io = IoApic::default();
        program(&mut io, 11, 0x8000 | 0x41, 0);
        io.set_irq(11, true);
        assert_eq!(collect(&mut io).len(), 1);
        io.write(IOREGSEL, 0x10 + 22);
        assert_ne!(io.read(IOWIN) & 1 << 14, 0, "remote IRR set");
        assert!(collect(&mut io).is_empty());
        io.eoi(0x41);
        let m = collect(&mut io);
        assert_eq!(m.len(), 1);
        assert!(m[0].level);
        io.set_irq(11, false);
        io.eoi(0x41);
        assert!(collect(&mut io).is_empty());
        io.write(IOREGSEL, 0x10 + 22);
        assert_eq!(io.read(IOWIN) & 1 << 14, 0);
    }

    #[test]
    fn unmasking_an_asserted_level_input_delivers_it() {
        let mut io = IoApic::default();
        program(&mut io, 16, 0x1_8000 | 0x50, 0);
        io.set_irq(16, true);
        assert!(collect(&mut io).is_empty());
        program(&mut io, 16, 0x8000 | 0x50, 0);
        assert_eq!(collect(&mut io)[0].vector, 0x50);
    }

    #[test]
    fn rejected_message_stays_pending() {
        let mut io = IoApic::default();
        program(&mut io, 1, 0x39, 0);
        io.set_irq(1, true);
        io.service(|_| false);
        assert_eq!(collect(&mut io).len(), 1);
    }
}

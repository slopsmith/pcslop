//! The 93C46 serial EEPROM (64 x 16 bits) that holds an RTL8139's MAC address.
//!
//! Follows `prom9346_*` in QEMU's hw/net/rtl8139.c (Copyright (c) 2006 Igor
//! Kovalenko, EEPROM changes by Juergen Lock; MIT license), including its
//! return to command mode after each read, which FreeBSD's drivers need.

pub const WORDS: usize = 64;
const ADDR_MASK: u16 = WORDS as u16 - 1;

const OP_MASK: u16 = 0xc0;
const OP_READ: u16 = 0x80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Idle,
    AwaitStart,
    Command,
    Read,
}

#[derive(Clone, Debug)]
pub struct Eeprom93c46 {
    pub contents: [u16; WORDS],
    mode: Mode,
    tick: u32,
    address: u16,
    input: u16,
    output: u16,
    cs: bool,
    sk: bool,
    di: bool,
    dout: bool,
}

impl Eeprom93c46 {
    pub fn new(contents: [u16; WORDS]) -> Self {
        Eeprom93c46 {
            contents,
            mode: Mode::Idle,
            tick: 0,
            address: 0,
            input: 0,
            output: 0,
            cs: false,
            sk: false,
            di: false,
            dout: false,
        }
    }

    /// The data-out pin, which only drives the bus while chip select is high.
    pub fn data_out(&self) -> bool {
        self.cs && self.dout
    }

    /// Drives chip select, the shift clock and data in.
    pub fn set_wires(&mut self, cs: bool, sk: bool, di: bool) {
        let rising_cs = !self.cs && cs;
        let rising_sk = !self.sk && sk;
        (self.cs, self.sk, self.di) = (cs, sk, di);
        if rising_cs {
            self.tick = 0;
            self.input = 0;
            self.output = 0;
            self.mode = Mode::AwaitStart;
        }
        if cs && rising_sk {
            self.clock();
        }
    }

    fn clock(&mut self) {
        let bit = self.di as u16;
        self.tick += 1;
        match self.mode {
            Mode::AwaitStart if bit == 1 => {
                self.mode = Mode::Command;
                self.tick = 0;
                self.input = 0;
            }
            Mode::Command => {
                self.input = self.input << 1 | bit;
                if self.tick == 8 {
                    self.decode(self.input & 0xff);
                }
            }
            Mode::Read => {
                self.dout = self.output & 0x8000 != 0;
                self.output <<= 1;
                if self.tick == 16 {
                    self.mode = Mode::AwaitStart;
                    self.input = 0;
                    self.tick = 0;
                }
            }
            _ => {}
        }
    }

    fn decode(&mut self, command: u16) {
        self.address = command & ADDR_MASK;
        self.tick = 0;
        self.mode = match command & OP_MASK {
            OP_READ => {
                self.output = self.contents[self.address as usize];
                self.dout = false;
                Mode::Read
            }
            // Like QEMU's, the EEPROM is read-only: writes are decoded and dropped.
            _ => Mode::Idle,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads one word the way Linux's 8139too does: a select, the read
    /// opcode with leading zeros, then 16 clocks sampling data out.
    fn read_word(e: &mut Eeprom93c46, addr: u16) -> u16 {
        e.set_wires(false, false, false);
        e.set_wires(true, false, false);
        let cmd = addr | 6 << 6;
        for i in (0..=10).rev() {
            let di = cmd & 1 << i != 0;
            e.set_wires(true, false, di);
            e.set_wires(true, true, di);
        }
        e.set_wires(true, false, false);
        let mut v = 0;
        for _ in 0..16 {
            e.set_wires(true, true, false);
            v = v << 1 | e.data_out() as u16;
            e.set_wires(true, false, false);
        }
        e.set_wires(false, false, false);
        v
    }

    fn eeprom() -> Eeprom93c46 {
        let mut c = [0; WORDS];
        for (i, w) in c.iter_mut().enumerate() {
            *w = 0x1000 + i as u16 * 0x0101;
        }
        Eeprom93c46::new(c)
    }

    #[test]
    fn reads_words_at_any_address() {
        let mut e = eeprom();
        for addr in [0, 7, 8, 9, 63] {
            assert_eq!(read_word(&mut e, addr), e.contents[addr as usize]);
        }
    }

    #[test]
    fn a_second_read_works_without_dropping_chip_select() {
        let mut e = eeprom();
        e.set_wires(true, false, false);
        for addr in [1u16, 2] {
            let cmd = addr | 6 << 6;
            for i in (0..=8).rev() {
                let di = cmd & 1 << i != 0;
                e.set_wires(true, false, di);
                e.set_wires(true, true, di);
            }
            let mut v = 0;
            for _ in 0..16 {
                e.set_wires(true, false, false);
                e.set_wires(true, true, false);
                v = v << 1 | e.data_out() as u16;
            }
            e.set_wires(true, false, false);
            assert_eq!(v, e.contents[addr as usize]);
        }
    }

    #[test]
    fn data_out_floats_low_without_chip_select() {
        let mut e = Eeprom93c46::new([0xffff; WORDS]);
        read_word(&mut e, 0);
        assert!(!e.data_out());
    }

    #[test]
    fn writes_are_ignored() {
        let mut e = eeprom();
        let before = e.contents;
        e.set_wires(true, false, false);
        let cmd: u32 = (5 << 6 | 3) << 16 | 0xbeef;
        for i in (0..25).rev() {
            let di = cmd & 1 << i != 0;
            e.set_wires(true, false, di);
            e.set_wires(true, true, di);
        }
        e.set_wires(false, false, false);
        assert_eq!(e.contents, before);
    }
}

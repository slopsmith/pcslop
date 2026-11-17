//! Cascaded 8259A programmable interrupt controllers (Intel 8259A datasheet),
//! plus the PIIX ELCR registers that select edge or level triggering.

#[derive(Default, Clone)]
struct Chip {
    irr: u8,
    isr: u8,
    imr: u8,
    /// Last level seen on each input, for edge detection.
    lines: u8,
    elcr: u8,
    vector_base: u8,
    init_step: u8,
    needs_icw4: bool,
    single: bool,
    auto_eoi: bool,
    read_isr: bool,
    special_mask: bool,
    rotate_on_aeoi: bool,
    lowest_priority: u8,
    /// Master: inputs with a slave attached. Slave: its cascade identity.
    cascade: u8,
}

impl Chip {
    fn reset_init(&mut self) {
        self.imr = 0;
        self.isr = 0;
        self.irr &= self.elcr;
        self.lowest_priority = 7;
        self.special_mask = false;
        self.read_isr = false;
        self.auto_eoi = false;
    }

    fn set_line(&mut self, irq: u8, level: bool) {
        let bit = 1 << irq;
        if self.elcr & bit != 0 {
            if level {
                self.irr |= bit;
            } else {
                self.irr &= !bit;
            }
        } else if level && self.lines & bit == 0 {
            self.irr |= bit;
        }
        if level {
            self.lines |= bit;
        } else {
            self.lines &= !bit;
        }
    }

    /// Priority order starting after `lowest_priority`.
    fn order(&self) -> impl Iterator<Item = u8> + '_ {
        (1..=8).map(move |i| (self.lowest_priority + i) & 7)
    }

    /// Highest-priority requested, unmasked IRQ that beats everything in service.
    fn pending(&self) -> Option<u8> {
        let req = self.irr & !self.imr;
        if req == 0 {
            return None;
        }
        for irq in self.order() {
            let bit = 1 << irq;
            if self.isr & bit != 0 && !self.special_mask {
                return None;
            }
            if req & bit != 0 {
                return Some(irq);
            }
        }
        None
    }

    fn ack(&mut self, irq: u8) {
        let bit = 1 << irq;
        if self.elcr & bit == 0 {
            self.irr &= !bit;
        }
        if self.auto_eoi {
            if self.rotate_on_aeoi {
                self.lowest_priority = irq;
            }
        } else {
            self.isr |= bit;
        }
    }

    fn eoi_highest(&mut self) -> Option<u8> {
        let irq = self.order().find(|&i| self.isr & (1 << i) != 0)?;
        self.isr &= !(1 << irq);
        Some(irq)
    }

    fn write(&mut self, a0: bool, v: u8) {
        if !a0 {
            if v & 0x10 != 0 {
                self.reset_init();
                self.needs_icw4 = v & 1 != 0;
                self.single = v & 2 != 0;
                self.init_step = 2;
            } else if v & 0x08 != 0 {
                if v & 0x02 != 0 {
                    self.read_isr = v & 1 != 0;
                }
                if v & 0x40 != 0 {
                    self.special_mask = v & 0x20 != 0;
                }
            } else {
                let rotate = v & 0x80 != 0;
                match (v >> 5) & 3 {
                    1 => {
                        if let Some(irq) = self.eoi_highest()
                            && rotate
                        {
                            self.lowest_priority = irq;
                        }
                    }
                    3 => {
                        let irq = v & 7;
                        self.isr &= !(1 << irq);
                        if rotate {
                            self.lowest_priority = irq;
                        }
                    }
                    0 if v & 0x60 == 0 => self.rotate_on_aeoi = rotate,
                    2 if rotate => self.lowest_priority = v & 7,
                    _ => {}
                }
            }
            return;
        }
        match self.init_step {
            2 => {
                self.vector_base = v & 0xf8;
                self.init_step = if self.single {
                    if self.needs_icw4 { 4 } else { 0 }
                } else {
                    3
                };
            }
            3 => {
                self.cascade = v;
                self.init_step = if self.needs_icw4 { 4 } else { 0 };
            }
            4 => {
                self.auto_eoi = v & 2 != 0;
                self.init_step = 0;
            }
            _ => self.imr = v,
        }
    }

    fn read(&self, a0: bool) -> u8 {
        if a0 {
            self.imr
        } else if self.read_isr {
            self.isr
        } else {
            self.irr
        }
    }
}

pub struct Pic {
    chips: [Chip; 2],
}

impl Default for Pic {
    fn default() -> Self {
        let mut p = Pic { chips: [Chip::default(), Chip::default()] };
        for c in &mut p.chips {
            c.lowest_priority = 7;
        }
        p.chips[0].cascade = 1 << 2;
        p.chips[1].cascade = 2;
        p
    }
}

impl Pic {
    pub fn set_irq(&mut self, irq: u8, level: bool) {
        if irq < 8 {
            self.chips[0].set_line(irq, level);
        } else {
            self.chips[1].set_line(irq - 8, level);
            self.update_cascade();
        }
    }

    /// IRR, ISR and IMR of the master and slave, for debugging.
    pub fn registers(&self) -> [(u8, u8, u8); 2] {
        self.chips.each_ref().map(|c| (c.irr, c.isr, c.imr))
    }

    /// True while an edge on `irq` (master PIC) is latched but not yet acknowledged.
    pub fn requested(&self, irq: u8) -> bool {
        self.chips[0].irr & (1 << irq) != 0
    }

    pub fn pulse_irq(&mut self, irq: u8) {
        self.set_irq(irq, true);
        self.set_irq(irq, false);
    }

    fn update_cascade(&mut self) {
        let slave = self.chips[1].pending().is_some();
        self.chips[0].set_line(2, slave);
    }

    pub fn intr(&self) -> bool {
        self.chips[0].pending().is_some()
    }

    /// Interrupt acknowledge: returns the vector and updates ISR/IRR.
    pub fn inta(&mut self) -> u8 {
        let Some(irq) = self.chips[0].pending() else {
            return self.chips[0].vector_base | 7;
        };
        self.chips[0].ack(irq);
        let vector = if irq == 2 {
            let v = match self.chips[1].pending() {
                Some(s) => {
                    self.chips[1].ack(s);
                    self.chips[1].vector_base | s
                }
                None => self.chips[1].vector_base | 7,
            };
            self.update_cascade();
            v
        } else {
            self.chips[0].vector_base | irq
        };
        vector
    }

    pub fn io_read(&mut self, port: u16) -> u8 {
        match port {
            0x20 | 0x21 => self.chips[0].read(port & 1 != 0),
            0xa0 | 0xa1 => self.chips[1].read(port & 1 != 0),
            0x4d0 => self.chips[0].elcr,
            0x4d1 => self.chips[1].elcr,
            _ => 0xff,
        }
    }

    pub fn io_write(&mut self, port: u16, v: u8) {
        match port {
            0x20 | 0x21 => self.chips[0].write(port & 1 != 0, v),
            0xa0 | 0xa1 => {
                self.chips[1].write(port & 1 != 0, v);
                self.update_cascade();
            }
            0x4d0 => self.chips[0].elcr = v & 0xf8,
            0x4d1 => self.chips[1].elcr = v & 0xde,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(p: &mut Pic) {
        for (port, v) in [(0x20, 0x11), (0x21, 0x08), (0x21, 0x04), (0x21, 0x01), (0x21, 0x00)] {
            p.io_write(port, v);
        }
        for (port, v) in [(0xa0, 0x11), (0xa1, 0x70), (0xa1, 0x02), (0xa1, 0x01), (0xa1, 0x00)] {
            p.io_write(port, v);
        }
    }

    #[test]
    fn delivers_master_and_slave_vectors() {
        let mut p = Pic::default();
        init(&mut p);
        p.pulse_irq(0);
        assert!(p.intr());
        assert_eq!(p.inta(), 0x08);
        assert!(!p.intr());
        p.io_write(0x20, 0x20);
        p.pulse_irq(8);
        assert!(p.intr());
        assert_eq!(p.inta(), 0x70);
    }

    #[test]
    fn in_service_irq_blocks_lower_priority() {
        let mut p = Pic::default();
        init(&mut p);
        p.pulse_irq(1);
        assert_eq!(p.inta(), 0x09);
        p.pulse_irq(3);
        assert!(!p.intr());
        p.io_write(0x20, 0x20);
        assert_eq!(p.inta(), 0x0b);
    }

    #[test]
    fn masked_irq_is_not_delivered() {
        let mut p = Pic::default();
        init(&mut p);
        p.io_write(0x21, 0x01);
        p.pulse_irq(0);
        assert!(!p.intr());
    }
}

//! A 16550A UART (National Semiconductor PC16550D datasheet). Transmitted
//! bytes go straight to a host writer and the line never stalls, so the
//! baud rate does not matter. Received bytes come from the host.

use std::collections::VecDeque;
use std::io::Write;

const IER_RDI: u8 = 0x01;
const IER_THRI: u8 = 0x02;
const IIR_NONE: u8 = 0x01;
const IIR_THRI: u8 = 0x02;
const IIR_RDI: u8 = 0x04;
const IIR_FIFO: u8 = 0xc0;
const FCR_ENABLE: u8 = 0x01;
const FCR_CLEAR_RX: u8 = 0x02;
const LCR_DLAB: u8 = 0x80;
const LSR_DR: u8 = 0x01;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;
const MCR_LOOP: u8 = 0x10;
const MCR_OUT2: u8 = 0x08;
const RX_FIFO: usize = 16;

#[derive(snapshot::Snap)]
pub struct Uart {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    fcr: u8,
    divisor: u16,
    /// A transmitter-empty interrupt is waiting; reading IIR or writing THR clears it.
    thr_irq: bool,
    rx: VecDeque<u8>,
    /// Host bytes not yet in the receive FIFO.
    input: VecDeque<u8>,
    #[snap(skip)]
    out: Box<dyn Write + Send>,
}

impl Uart {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Uart {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            fcr: 0,
            divisor: 12,
            thr_irq: false,
            rx: VecDeque::new(),
            input: VecDeque::new(),
            out,
        }
    }

    /// Resets the registers and keeps the output and any queued host input.
    pub fn reset(&mut self) {
        let out = std::mem::replace(&mut self.out, Box::new(std::io::sink()));
        let input = std::mem::take(&mut self.input);
        *self = Uart::new(out);
        self.input = input;
    }

    pub fn set_output(&mut self, out: Box<dyn Write + Send>) {
        self.out = out;
    }

    /// Queues bytes for the guest to receive.
    pub fn send(&mut self, bytes: &[u8]) {
        self.input.extend(bytes);
        self.refill();
    }

    fn refill(&mut self) {
        let cap = if self.fcr & FCR_ENABLE != 0 { RX_FIFO } else { 1 };
        while self.rx.len() < cap
            && let Some(b) = self.input.pop_front()
        {
            self.rx.push_back(b);
        }
    }

    fn iir(&self) -> u8 {
        let fifo = if self.fcr & FCR_ENABLE != 0 { IIR_FIFO } else { 0 };
        let id = if self.ier & IER_RDI != 0 && !self.rx.is_empty() {
            IIR_RDI
        } else if self.ier & IER_THRI != 0 && self.thr_irq {
            IIR_THRI
        } else {
            IIR_NONE
        };
        fifo | id
    }

    /// Level of the interrupt line. PCs gate it with OUT2.
    pub fn irq(&self) -> bool {
        self.mcr & MCR_OUT2 != 0 && self.iir() & IIR_NONE == 0
    }

    fn msr(&self) -> u8 {
        if self.mcr & MCR_LOOP != 0 {
            // Loopback wires DTR to DSR, RTS to CTS, OUT1 to RI and OUT2 to DCD.
            let m = self.mcr;
            (m & 1) << 5 | (m & 2) << 3 | (m & 4) << 4 | (m & 8) << 4
        } else {
            0xb0
        }
    }

    /// Reads register `reg` (0..=7).
    pub fn read(&mut self, reg: u16) -> u8 {
        let dlab = self.lcr & LCR_DLAB != 0;
        match reg {
            0 if dlab => self.divisor as u8,
            1 if dlab => (self.divisor >> 8) as u8,
            0 => {
                let b = self.rx.pop_front().unwrap_or(0);
                self.refill();
                b
            }
            1 => self.ier,
            2 => {
                let v = self.iir();
                if v & 0x0f == IIR_THRI {
                    self.thr_irq = false;
                }
                v
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => LSR_THRE | LSR_TEMT | if self.rx.is_empty() { 0 } else { LSR_DR },
            6 => self.msr(),
            _ => self.scr,
        }
    }

    pub fn write(&mut self, reg: u16, v: u8) {
        let dlab = self.lcr & LCR_DLAB != 0;
        match reg {
            0 if dlab => self.divisor = self.divisor & 0xff00 | v as u16,
            1 if dlab => self.divisor = self.divisor & 0xff | (v as u16) << 8,
            0 => {
                if self.mcr & MCR_LOOP != 0 {
                    self.rx.push_back(v);
                } else {
                    let _ = self.out.write_all(&[v]);
                    if v == b'\n' {
                        let _ = self.out.flush();
                    }
                }
                self.thr_irq = true;
            }
            1 => {
                // Enabling the THR interrupt while THR is empty raises it at once.
                if v & IER_THRI != 0 && self.ier & IER_THRI == 0 {
                    self.thr_irq = true;
                }
                self.ier = v & 0x0f;
            }
            2 => {
                if v & FCR_CLEAR_RX != 0 {
                    self.rx.clear();
                }
                self.fcr = v & 0xc9;
                self.refill();
            }
            3 => self.lcr = v,
            4 => self.mcr = v & 0x1f,
            7 => self.scr = v,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn uart() -> (Uart, Sink) {
        let s = Sink::default();
        (Uart::new(Box::new(s.clone())), s)
    }

    #[test]
    fn transmitted_bytes_reach_the_host() {
        let (mut u, s) = uart();
        for b in b"hi\n" {
            u.write(0, *b);
        }
        assert_eq!(&*s.0.lock().unwrap(), b"hi\n");
        assert_eq!(u.read(5) & (LSR_THRE | LSR_TEMT), LSR_THRE | LSR_TEMT);
    }

    #[test]
    fn divisor_latch_hides_data_registers() {
        let (mut u, s) = uart();
        u.write(3, LCR_DLAB);
        u.write(0, 0x01);
        u.write(1, 0x00);
        assert_eq!((u.read(0), u.read(1)), (0x01, 0x00));
        u.write(3, 0x03);
        assert_eq!(u.read(1), 0);
        assert!(s.0.lock().unwrap().is_empty());
    }

    #[test]
    fn loopback_mirrors_modem_control_and_data() {
        let (mut u, s) = uart();
        u.write(4, MCR_LOOP | 0x0a);
        assert_eq!(u.read(6) & 0xf0, 0x90);
        u.write(0, 0x5a);
        assert_eq!(u.read(5) & LSR_DR, LSR_DR);
        assert_eq!(u.read(0), 0x5a);
        assert!(s.0.lock().unwrap().is_empty());
    }

    #[test]
    fn fifo_enable_shows_in_iir() {
        let (mut u, _) = uart();
        assert_eq!(u.read(2), IIR_NONE);
        u.write(2, 0xc7);
        assert_eq!(u.read(2), IIR_FIFO | IIR_NONE);
    }

    #[test]
    fn thr_interrupt_fires_on_enable_and_clears_on_iir_read() {
        let (mut u, _) = uart();
        u.write(4, MCR_OUT2);
        u.write(1, IER_THRI);
        assert!(u.irq());
        assert_eq!(u.read(2), IIR_THRI);
        assert!(!u.irq());
        u.write(0, b'x');
        assert!(u.irq());
    }

    #[test]
    fn interrupt_needs_out2() {
        let (mut u, _) = uart();
        u.write(1, IER_THRI);
        assert!(!u.irq());
    }

    #[test]
    fn host_input_raises_data_ready_interrupt_until_drained() {
        let (mut u, _) = uart();
        u.write(4, MCR_OUT2);
        u.write(2, FCR_ENABLE);
        u.write(1, IER_RDI);
        u.send(b"ab");
        assert_eq!(u.read(2) & 0x0f, IIR_RDI);
        assert_eq!((u.read(0), u.read(0)), (b'a', b'b'));
        assert!(!u.irq());
        assert_eq!(u.read(5) & LSR_DR, 0);
    }
}

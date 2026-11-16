//! USB 1.1 for the PIIX3 board: the UHCI host controller (PCI 8086:7020)
//! and the devices plugged into its root ports.

mod control;
mod tablet;
mod uhci;

use std::any::Any;

pub use tablet::{MAX_COORD, Tablet};
pub use uhci::Uhci;

pub const PCI_VENDOR: u16 = 0x8086;
pub const PCI_DEVICE: u16 = 0x7020;
/// Serial bus controller, USB, UHCI.
pub const PCI_CLASS_CODE: u32 = 0x0c_03_00;

/// Guest physical memory as seen by the host controller's DMA engine.
pub trait DmaMemory {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]);
    fn write_phys(&mut self, addr: u32, buf: &[u8]);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pid {
    Setup,
    In,
    Out,
}

/// A device's handshake for one transaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reply {
    /// The transaction succeeded. For IN, this many bytes of the buffer hold data.
    Ack(usize),
    Nak,
    Stall,
}

/// A full-speed device on a root port.
pub trait UsbDevice: Any {
    /// USB bus reset: back to the Default state at address 0.
    fn reset(&mut self);

    fn address(&self) -> u8;

    /// Runs one transaction on endpoint `ep`. For SETUP and OUT, `buf` holds
    /// the data the host sent; for IN, the device writes up to `buf.len()`
    /// bytes into it.
    fn transact(&mut self, pid: Pid, ep: u8, buf: &mut [u8]) -> Reply;

    /// Called at the start of every frame with the controller's frame count (1 ms each).
    fn start_frame(&mut self, _frame: u64) {}

    /// True once if the device wants to signal remote wakeup.
    fn take_wakeup(&mut self) -> bool {
        false
    }
}

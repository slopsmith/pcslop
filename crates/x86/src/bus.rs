/// The CPU's view of the outside world: physical memory, I/O ports and the
/// interrupt line.
pub trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    fn write8(&mut self, addr: u32, value: u8);

    fn read16(&mut self, addr: u32) -> u16 {
        u16::from_le_bytes([self.read8(addr), self.read8(addr.wrapping_add(1))])
    }

    fn read32(&mut self, addr: u32) -> u32 {
        u32::from_le_bytes([
            self.read8(addr),
            self.read8(addr.wrapping_add(1)),
            self.read8(addr.wrapping_add(2)),
            self.read8(addr.wrapping_add(3)),
        ])
    }

    fn write16(&mut self, addr: u32, value: u16) {
        let b = value.to_le_bytes();
        self.write8(addr, b[0]);
        self.write8(addr.wrapping_add(1), b[1]);
    }

    fn write32(&mut self, addr: u32, value: u32) {
        let b = value.to_le_bytes();
        for (i, byte) in b.into_iter().enumerate() {
            self.write8(addr.wrapping_add(i as u32), byte);
        }
    }

    /// `size` is 1, 2 or 4 bytes.
    fn io_read(&mut self, port: u16, size: u8) -> u32;
    fn io_write(&mut self, port: u16, size: u8, value: u32);

    /// True while the interrupt controller asserts INTR.
    fn intr(&mut self) -> bool {
        false
    }

    /// Runs an interrupt-acknowledge cycle and returns the vector.
    fn inta(&mut self) -> u8 {
        0
    }

    /// Reads an MSR that lives outside the CPU core, such as IA32_APIC_BASE.
    fn rdmsr(&mut self, _msr: u32) -> Option<u64> {
        None
    }

    /// Writes an MSR that lives outside the CPU core. False if there is none.
    fn wrmsr(&mut self, _msr: u32, _value: u64) -> bool {
        false
    }

    /// Whether CPUID should report a local APIC.
    fn apic_present(&self) -> bool {
        false
    }

    /// Asks the bus to report writes to the physical page containing `addr`,
    /// because translated code was made from it.
    fn watch_code_page(&mut self, _addr: u32) {}

    /// Returns (and forgets) the watched pages written since the last call.
    fn take_written_code_pages(&mut self) -> Vec<u32> {
        Vec::new()
    }
}

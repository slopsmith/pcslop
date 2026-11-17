//! The local APIC of a single Pentium 4 in xAPIC mode (Intel SDM vol. 3A,
//! chapter "Advanced Programmable Interrupt Controller (APIC)").

pub const DEFAULT_BASE: u32 = 0xfee0_0000;
pub const SIZE: u32 = 0x1000;
/// Nanoseconds per tick of the bus clock that drives the timer (100 MHz).
pub const NS_PER_TICK: u64 = 10;

pub mod msr {
    pub const APIC_BASE: u32 = 0x1b;
    pub const BSP: u64 = 1 << 8;
    pub const ENABLE: u64 = 1 << 11;
    pub const BASE_MASK: u64 = 0xf_ffff_f000;
}

pub mod reg {
    pub const ID: u32 = 0x20;
    pub const VERSION: u32 = 0x30;
    pub const TPR: u32 = 0x80;
    pub const APR: u32 = 0x90;
    pub const PPR: u32 = 0xa0;
    pub const EOI: u32 = 0xb0;
    pub const LDR: u32 = 0xd0;
    pub const DFR: u32 = 0xe0;
    pub const SVR: u32 = 0xf0;
    pub const ISR: u32 = 0x100;
    pub const TMR: u32 = 0x180;
    pub const IRR: u32 = 0x200;
    pub const ESR: u32 = 0x280;
    pub const ICR_LO: u32 = 0x300;
    pub const ICR_HI: u32 = 0x310;
    pub const LVT_TIMER: u32 = 0x320;
    pub const LVT_LINT0: u32 = 0x350;
    pub const LVT_LINT1: u32 = 0x360;
    pub const LVT_ERROR: u32 = 0x370;
    pub const TIMER_INITIAL: u32 = 0x380;
    pub const TIMER_CURRENT: u32 = 0x390;
    pub const TIMER_DIVIDE: u32 = 0x3e0;
}

/// Version 0x14 (an xAPIC), with six LVT entries: timer, thermal, performance
/// counters, LINT0, LINT1 and error.
const VERSION: u32 = 0x0005_0014;
const LVTS: usize = 6;
const TIMER: usize = 0;
const LINT0: usize = 3;
const ERROR: usize = 5;

pub mod lvt {
    pub const VECTOR: u32 = 0xff;
    pub const MODE: u32 = 7 << 8;
    pub const LEVEL: u32 = 1 << 15;
    pub const MASKED: u32 = 1 << 16;
    pub const PERIODIC: u32 = 1 << 17;
}

pub mod mode {
    pub const FIXED: u8 = 0;
    pub const LOWEST: u8 = 1;
    pub const NMI: u8 = 4;
    pub const INIT: u8 = 5;
    pub const STARTUP: u8 = 6;
    pub const EXTINT: u8 = 7;
}

const SVR_ENABLE: u32 = 1 << 8;
const ESR_SEND_ILLEGAL: u32 = 1 << 5;
const ESR_RECEIVE_ILLEGAL: u32 = 1 << 6;

/// An interrupt message on the system bus, from the I/O APIC or an IPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub vector: u8,
    pub mode: u8,
    pub dest: u8,
    pub logical: bool,
    pub level: bool,
}

/// MMIO accesses since the APIC was created.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub tpr_reads: u64,
    pub tpr_writes: u64,
    pub eois: u64,
}

/// A 256-bit vector bitmap, as in ISR, TMR and IRR.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, snapshot::Snap)]
struct Bits([u64; 4]);

impl Bits {
    fn get(&self, v: u8) -> bool {
        self.0[v as usize >> 6] & 1 << (v & 63) != 0
    }

    fn set(&mut self, v: u8, on: bool) {
        let w = &mut self.0[v as usize >> 6];
        if on {
            *w |= 1 << (v & 63);
        } else {
            *w &= !(1 << (v & 63));
        }
    }

    fn highest(&self) -> Option<u8> {
        (0..4).rev().find(|&i| self.0[i] != 0).map(|i| (i * 64 + 63 - self.0[i].leading_zeros() as usize) as u8)
    }

    /// Bits `32 * i .. 32 * i + 32`, as the register at `base + 0x10 * i` shows them.
    fn word(&self, i: usize) -> u32 {
        (self.0[i / 2] >> (32 * (i % 2))) as u32
    }
}

#[derive(snapshot::Snap)]
pub struct LocalApic {
    base_msr: u64,
    id: u8,
    tpr: u8,
    ldr: u32,
    dfr: u32,
    svr: u32,
    isr: Bits,
    irr: Bits,
    tmr: Bits,
    /// Errors since the last write to ESR, and what ESR shows.
    errors: u32,
    esr: u32,
    icr: u64,
    lvt: [u32; LVTS],
    timer_initial: u32,
    timer_divide: u32,
    /// When the current timer period started, while the timer counts.
    timer_start: Option<u64>,
    #[snap(skip)]
    pub stats: Stats,
}

impl Default for LocalApic {
    fn default() -> Self {
        let mut a = LocalApic {
            base_msr: DEFAULT_BASE as u64 | msr::ENABLE | msr::BSP,
            id: 0,
            tpr: 0,
            ldr: 0,
            dfr: 0,
            svr: 0,
            isr: Bits::default(),
            irr: Bits::default(),
            tmr: Bits::default(),
            errors: 0,
            esr: 0,
            icr: 0,
            lvt: [0; LVTS],
            timer_initial: 0,
            timer_divide: 0,
            timer_start: None,
            stats: Stats::default(),
        };
        a.reset_registers();
        a
    }
}

impl LocalApic {
    /// The register state after power-up, an INIT or a global disable (SDM 13.4.7.1).
    fn reset_registers(&mut self) {
        self.tpr = 0;
        self.ldr = 0;
        self.dfr = u32::MAX;
        self.svr = 0xff;
        self.isr = Bits::default();
        self.irr = Bits::default();
        self.tmr = Bits::default();
        self.errors = 0;
        self.esr = 0;
        self.icr = 0;
        self.lvt = [lvt::MASKED; LVTS];
        self.timer_initial = 0;
        self.timer_divide = 0;
        self.timer_start = None;
    }

    pub fn msr(&self) -> u64 {
        self.base_msr
    }

    pub fn set_msr(&mut self, v: u64) {
        let was_enabled = self.hw_enabled();
        self.base_msr = (v & (msr::BASE_MASK | msr::ENABLE)) | msr::BSP;
        if was_enabled && !self.hw_enabled() {
            self.reset_registers();
        }
    }

    /// The global enable flag in IA32_APIC_BASE. While it is clear, the CPU
    /// behaves as if it had no APIC.
    pub fn hw_enabled(&self) -> bool {
        self.base_msr & msr::ENABLE != 0
    }

    fn sw_enabled(&self) -> bool {
        self.svr & SVR_ENABLE != 0
    }

    pub fn base(&self) -> u32 {
        (self.base_msr & msr::BASE_MASK) as u32
    }

    /// Whether the MMIO window is decoded at `addr`.
    pub fn claims(&self, addr: u32) -> bool {
        self.hw_enabled() && addr.wrapping_sub(self.base()) < SIZE
    }

    /// Whether the 8259's INTR output reaches the CPU: through LINT0 set up
    /// as ExtINT, or directly while the APIC is globally disabled. This
    /// follows QEMU's apic_accept_pic_intr (hw/intc/apic.c, Fabrice Bellard).
    pub fn accepts_pic(&self) -> bool {
        let l = self.lvt[LINT0];
        !self.hw_enabled() || (l & lvt::MASKED == 0 && (l & lvt::MODE) >> 8 == mode::EXTINT as u32)
    }

    fn isrv(&self) -> u8 {
        self.isr.highest().unwrap_or(0)
    }

    pub fn ppr(&self) -> u8 {
        let isrv = self.isrv();
        if self.tpr >> 4 >= isrv >> 4 { self.tpr } else { isrv & 0xf0 }
    }

    pub fn tpr(&self) -> u8 {
        self.tpr
    }

    /// Sets the TPR without counting an access, for a TPR the JIT keeps in the VM.
    pub fn set_tpr(&mut self, v: u8) {
        self.tpr = v;
    }

    /// The priority class of the highest request that only the TPR holds back,
    /// or 0. Lowering the TPR class below it lets that request through.
    pub fn tpr_threshold(&self) -> u8 {
        match self.irr.highest() {
            Some(v) if self.pending().is_none() && v >> 4 > self.isrv() >> 4 && self.sw_enabled() => v & 0xf0,
            _ => 0,
        }
    }

    /// The vector the APIC would deliver now, if any beats the processor priority.
    pub fn pending(&self) -> Option<u8> {
        if !self.hw_enabled() || !self.sw_enabled() {
            return None;
        }
        let v = self.irr.highest()?;
        (v >> 4 > self.ppr() >> 4).then_some(v)
    }

    /// Whether `vector` waits in IRR.
    pub fn requested(&self, vector: u8) -> bool {
        self.irr.get(vector)
    }

    /// Interrupt acknowledge: moves the highest pending vector from IRR to
    /// ISR, or returns the spurious vector if none may be delivered any more.
    pub fn ack(&mut self) -> u8 {
        let Some(v) = self.pending() else { return self.svr as u8 };
        self.irr.set(v, false);
        self.isr.set(v, true);
        v
    }

    /// Ends the highest-priority interrupt in service. Returns its vector if
    /// it was level-triggered, so the I/O APIC can clear its remote IRR.
    fn eoi(&mut self) -> Option<u8> {
        self.stats.eois += 1;
        let v = self.isr.highest()?;
        self.isr.set(v, false);
        self.tmr.get(v).then_some(v)
    }

    pub fn matches(&self, dest: u8, logical: bool) -> bool {
        if dest == 0xff {
            return true;
        }
        if !logical {
            return dest == self.id;
        }
        let ldr = (self.ldr >> 24) as u8;
        if self.dfr >> 28 == 0xf {
            ldr & dest != 0
        } else {
            ldr >> 4 == dest >> 4 && ldr & dest & 0xf != 0
        }
    }

    /// Takes a message from the bus. Returns false if it is not for this APIC.
    pub fn accept(&mut self, m: Message) -> bool {
        if !self.hw_enabled() || !self.matches(m.dest, m.logical) {
            return false;
        }
        if matches!(m.mode, mode::FIXED | mode::LOWEST) {
            self.set_irr(m.vector, m.level);
        }
        true
    }

    fn set_irr(&mut self, vector: u8, level: bool) {
        if vector < 16 {
            self.error(ESR_RECEIVE_ILLEGAL);
            return;
        }
        self.irr.set(vector, true);
        self.tmr.set(vector, level);
    }

    fn error(&mut self, bits: u32) {
        self.errors |= bits;
        self.raise_lvt(ERROR);
    }

    fn raise_lvt(&mut self, i: usize) {
        let l = self.lvt[i];
        if l & lvt::MASKED == 0 && (l & lvt::MODE) >> 8 == mode::FIXED as u32 {
            self.set_irr(l as u8, false);
        }
    }

    fn send_ipi(&mut self) {
        let lo = self.icr as u32;
        let dest = (self.icr >> 56) as u8;
        let logical = lo & 1 << 11 != 0;
        let to_self = match (lo >> 18) & 3 {
            0 => self.matches(dest, logical),
            1 | 2 => true,
            _ => false,
        };
        let m = ((lo >> 8) & 7) as u8;
        if !matches!(m, mode::FIXED | mode::LOWEST) {
            return;
        }
        if (lo as u8) < 16 {
            self.error(ESR_SEND_ILLEGAL);
        }
        if to_self {
            self.set_irr(lo as u8, false);
        }
    }

    fn divisor(&self) -> u64 {
        let d = (self.timer_divide & 3) | (self.timer_divide >> 1) & 4;
        if d == 7 { 1 } else { 2 << d }
    }

    fn period_ns(&self) -> u64 {
        self.timer_initial as u64 * self.divisor() * NS_PER_TICK
    }

    fn timer_current(&self, now: u64) -> u32 {
        let Some(start) = self.timer_start else { return 0 };
        let elapsed = now.saturating_sub(start);
        let period = self.period_ns();
        if elapsed >= period && self.lvt[TIMER] & lvt::PERIODIC == 0 {
            return 0;
        }
        let ticks = (elapsed % period) / (self.divisor() * NS_PER_TICK);
        self.timer_initial - ticks as u32
    }

    /// Advances the timer to `now`, raising its interrupt if a count ran out.
    /// Periods missed in between merge into one interrupt, as they would in IRR.
    pub fn poll(&mut self, now: u64) {
        let Some(start) = self.timer_start else { return };
        let period = self.period_ns();
        let elapsed = now.saturating_sub(start);
        if elapsed < period {
            return;
        }
        if self.lvt[TIMER] & lvt::PERIODIC != 0 {
            self.timer_start = Some(start + elapsed / period * period);
        } else {
            self.timer_start = None;
        }
        self.raise_lvt(TIMER);
    }

    /// Nanoseconds from `now` until the timer next runs out.
    pub fn next_event_ns(&self, now: u64) -> Option<u64> {
        Some((self.timer_start? + self.period_ns()).saturating_sub(now))
    }

    pub fn read(&mut self, offset: u32, now: u64) -> u32 {
        self.stats.reads += 1;
        let i = ((offset >> 4) & 7) as usize;
        match offset & 0xff0 {
            reg::ID => (self.id as u32) << 24,
            reg::VERSION => VERSION,
            reg::TPR => {
                self.stats.tpr_reads += 1;
                self.tpr as u32
            }
            reg::PPR => self.ppr() as u32,
            reg::LDR => self.ldr,
            reg::DFR => self.dfr,
            reg::SVR => self.svr,
            0x100..=0x170 => self.isr.word(i),
            0x180..=0x1f0 => self.tmr.word(i),
            0x200..=0x270 => self.irr.word(i),
            reg::ESR => self.esr,
            reg::ICR_LO => self.icr as u32,
            reg::ICR_HI => (self.icr >> 32) as u32,
            0x320..=0x370 => self.lvt[(offset as usize >> 4) - 0x32],
            reg::TIMER_INITIAL => self.timer_initial,
            reg::TIMER_CURRENT => self.timer_current(now),
            reg::TIMER_DIVIDE => self.timer_divide,
            _ => 0,
        }
    }

    /// Writes a register. Returns the vector of a level-triggered interrupt
    /// that an EOI just ended, for the I/O APIC.
    pub fn write(&mut self, offset: u32, v: u32, now: u64) -> Option<u8> {
        self.stats.writes += 1;
        match offset & 0xff0 {
            reg::ID => self.id = (v >> 24) as u8,
            reg::TPR => {
                self.stats.tpr_writes += 1;
                self.tpr = v as u8;
            }
            reg::EOI => return self.eoi(),
            reg::LDR => self.ldr = v & 0xff00_0000,
            reg::DFR => self.dfr = v | 0x0fff_ffff,
            reg::SVR => {
                self.svr = v & 0x1ff;
                if !self.sw_enabled() {
                    for l in &mut self.lvt {
                        *l |= lvt::MASKED;
                    }
                }
            }
            reg::ESR => self.esr = std::mem::take(&mut self.errors),
            reg::ICR_LO => {
                self.icr = (self.icr & !0xffff_ffff) | (v & !(1 << 12)) as u64;
                self.send_ipi();
            }
            reg::ICR_HI => self.icr = (self.icr & 0xffff_ffff) | ((v & 0xff00_0000) as u64) << 32,
            0x320..=0x370 => {
                let i = (offset as usize >> 4) - 0x32;
                let writable = match i {
                    TIMER => lvt::VECTOR | lvt::MASKED | lvt::PERIODIC,
                    LINT0 | 4 => lvt::VECTOR | lvt::MODE | 1 << 13 | lvt::LEVEL | lvt::MASKED,
                    ERROR => lvt::VECTOR | lvt::MASKED,
                    _ => lvt::VECTOR | lvt::MODE | lvt::MASKED,
                };
                let masked = if self.sw_enabled() { 0 } else { lvt::MASKED };
                self.lvt[i] = (v & writable) | masked;
            }
            reg::TIMER_INITIAL => {
                self.timer_initial = v;
                self.timer_start = (v != 0).then_some(now);
            }
            reg::TIMER_DIVIDE => {
                let counted = self.timer_start.map(|_| self.timer_initial - self.timer_current(now));
                self.timer_divide = v & 0xb;
                if let Some(ticks) = counted {
                    self.timer_start = Some(now.saturating_sub(ticks as u64 * self.divisor() * NS_PER_TICK));
                }
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> LocalApic {
        let mut a = LocalApic::default();
        a.write(reg::SVR, 0x1ff, 0);
        a
    }

    fn fixed(vector: u8, level: bool) -> Message {
        Message { vector, mode: mode::FIXED, dest: 0, logical: false, level }
    }

    #[test]
    fn delivers_highest_vector_and_masks_lower_classes_until_eoi() {
        let mut a = enabled();
        a.accept(fixed(0x41, false));
        a.accept(fixed(0x52, false));
        assert_eq!(a.ack(), 0x52);
        assert_eq!(a.ppr(), 0x50);
        assert_eq!(a.pending(), None);
        a.accept(fixed(0x61, false));
        assert_eq!(a.ack(), 0x61);
        a.write(reg::EOI, 0, 0);
        assert_eq!(a.pending(), None, "0x41 is still below 0x52 in service");
        a.write(reg::EOI, 0, 0);
        assert_eq!(a.ack(), 0x41);
        a.write(reg::EOI, 0, 0);
        assert_eq!(a.pending(), None);
        assert_eq!(a.read(reg::ISR + 0x20, 0), 0);
    }

    #[test]
    fn tpr_holds_back_vectors_in_its_class_and_below() {
        let mut a = enabled();
        a.write(reg::TPR, 0x50, 0);
        a.accept(fixed(0x55, false));
        assert_eq!(a.pending(), None);
        a.accept(fixed(0x60, false));
        assert_eq!(a.pending(), Some(0x60));
        a.write(reg::TPR, 0, 0);
        a.ack();
        assert_eq!(a.pending(), None);
        a.write(reg::EOI, 0, 0);
        assert_eq!(a.pending(), Some(0x55));
    }

    #[test]
    fn tpr_threshold_names_the_class_the_tpr_holds_back() {
        let mut a = enabled();
        a.write(reg::TPR, 0x50, 0);
        assert_eq!(a.tpr_threshold(), 0);
        a.accept(fixed(0x45, false));
        assert_eq!(a.tpr_threshold(), 0x40);
        a.set_tpr(0x3f);
        assert_eq!(a.pending(), Some(0x45));
        assert_eq!(a.tpr_threshold(), 0);
    }

    #[test]
    fn tpr_threshold_ignores_requests_held_back_by_the_isr() {
        let mut a = enabled();
        a.accept(fixed(0x80, false));
        assert_eq!(a.ack(), 0x80);
        a.write(reg::TPR, 0x50, 0);
        a.accept(fixed(0x45, false));
        assert_eq!(a.tpr_threshold(), 0);
    }

    #[test]
    fn software_disabled_apic_delivers_nothing_and_masks_lvts() {
        let mut a = LocalApic::default();
        a.accept(fixed(0x40, false));
        assert_eq!(a.pending(), None);
        a.write(reg::LVT_LINT0, 0x700, 0);
        assert!(!a.accepts_pic());
        a.write(reg::SVR, 0x1ff, 0);
        assert_eq!(a.pending(), Some(0x40));
        a.write(reg::LVT_LINT0, 0x8700, 0);
        assert!(a.accepts_pic());
        a.write(reg::SVR, 0xff, 0);
        assert!(!a.accepts_pic());
    }

    #[test]
    fn eoi_of_level_interrupt_is_reported() {
        let mut a = enabled();
        a.accept(fixed(0x31, true));
        a.accept(fixed(0x40, false));
        assert_eq!(a.ack(), 0x40);
        assert_eq!(a.write(reg::EOI, 0, 0), None);
        assert_eq!(a.ack(), 0x31);
        assert_eq!(a.write(reg::EOI, 0, 0), Some(0x31));
    }

    #[test]
    fn one_shot_timer_counts_down_and_fires_once() {
        let mut a = enabled();
        a.write(reg::TIMER_DIVIDE, 0x3, 0);
        a.write(reg::LVT_TIMER, 0x30, 0);
        a.write(reg::TIMER_INITIAL, 1000, 0);
        let tick = 16 * NS_PER_TICK;
        assert_eq!(a.read(reg::TIMER_CURRENT, 250 * tick), 750);
        a.poll(999 * tick);
        assert_eq!(a.pending(), None);
        assert_eq!(a.next_event_ns(999 * tick), Some(tick));
        a.poll(1000 * tick);
        assert_eq!(a.ack(), 0x30);
        assert_eq!(a.read(reg::TIMER_CURRENT, 1500 * tick), 0);
        a.write(reg::EOI, 0, 0);
        a.poll(5000 * tick);
        assert_eq!(a.pending(), None);
        assert_eq!(a.next_event_ns(5000 * tick), None);
    }

    #[test]
    fn periodic_timer_reloads() {
        let mut a = enabled();
        a.write(reg::TIMER_DIVIDE, 0xb, 0);
        a.write(reg::LVT_TIMER, lvt::PERIODIC | 0x30, 0);
        a.write(reg::TIMER_INITIAL, 100, 0);
        let p = 100 * NS_PER_TICK;
        a.poll(p);
        assert_eq!(a.ack(), 0x30);
        a.write(reg::EOI, 0, 0);
        assert_eq!(a.read(reg::TIMER_CURRENT, p + 30 * NS_PER_TICK), 70);
        a.poll(3 * p + 5);
        assert_eq!(a.ack(), 0x30);
        assert_eq!(a.pending(), None);
        assert_eq!(a.next_event_ns(3 * p + 5), Some(p - 5));
    }

    #[test]
    fn masked_timer_counts_without_interrupting() {
        let mut a = enabled();
        a.write(reg::LVT_TIMER, lvt::MASKED | 0x30, 0);
        a.write(reg::TIMER_INITIAL, 10, 0);
        a.poll(1_000_000);
        assert_eq!(a.pending(), None);
    }

    #[test]
    fn self_ipi_and_illegal_vector() {
        let mut a = enabled();
        a.write(reg::ICR_LO, 1 << 18 | 0x45, 0);
        assert_eq!(a.pending(), Some(0x45));
        a.write(reg::LVT_ERROR, 0x33, 0);
        a.write(reg::ICR_LO, 1 << 18 | 0x05, 0);
        a.write(reg::ESR, 0, 0);
        assert_eq!(a.read(reg::ESR, 0), ESR_SEND_ILLEGAL | ESR_RECEIVE_ILLEGAL);
        assert_eq!(a.ack(), 0x45);
        a.write(reg::EOI, 0, 0);
        assert_eq!(a.ack(), 0x33);
        a.write(reg::ICR_LO, 3 << 18 | 0x46, 0);
        a.write(reg::EOI, 0, 0);
        assert_eq!(a.pending(), None, "all-excluding-self skips the only CPU");
    }

    #[test]
    fn logical_destinations() {
        let mut a = enabled();
        a.write(reg::LDR, 0x0100_0000, 0);
        assert!(a.matches(0x01, true));
        assert!(!a.matches(0x02, true));
        a.write(reg::DFR, 0x0fff_ffff, 0);
        a.write(reg::LDR, 0x2100_0000, 0);
        assert!(a.matches(0x23, true));
        assert!(!a.matches(0x11, true));
        assert!(a.matches(0, false));
        assert!(!a.matches(1, false));
    }

    #[test]
    fn global_disable_hides_the_apic_and_resets_it() {
        let mut a = enabled();
        a.write(reg::LVT_LINT0, 0x700, 0);
        a.set_msr(DEFAULT_BASE as u64);
        assert!(!a.claims(DEFAULT_BASE));
        assert!(a.accepts_pic());
        a.set_msr(0xfed0_0000 | msr::ENABLE);
        assert!(a.claims(0xfed0_0080));
        assert_eq!(a.read(reg::SVR, 0), 0xff);
        assert!(!a.accepts_pic());
    }
}

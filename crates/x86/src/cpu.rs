pub mod flags {
    pub const CF: u32 = 1 << 0;
    pub const FIXED1: u32 = 1 << 1;
    pub const PF: u32 = 1 << 2;
    pub const AF: u32 = 1 << 4;
    pub const ZF: u32 = 1 << 6;
    pub const SF: u32 = 1 << 7;
    pub const TF: u32 = 1 << 8;
    pub const IF: u32 = 1 << 9;
    pub const DF: u32 = 1 << 10;
    pub const OF: u32 = 1 << 11;
    pub const IOPL: u32 = 3 << 12;
    pub const NT: u32 = 1 << 14;
    pub const RF: u32 = 1 << 16;
    pub const VM: u32 = 1 << 17;
    pub const AC: u32 = 1 << 18;
    pub const VIF: u32 = 1 << 19;
    pub const VIP: u32 = 1 << 20;
    pub const ID: u32 = 1 << 21;

    pub const ARITH: u32 = CF | PF | AF | ZF | SF | OF;
    pub const WRITABLE: u32 = ARITH | TF | IF | DF | IOPL | NT | RF | VM | AC | VIF | VIP | ID;
}

pub mod cr0 {
    pub const PE: u32 = 1 << 0;
    pub const MP: u32 = 1 << 1;
    pub const EM: u32 = 1 << 2;
    pub const TS: u32 = 1 << 3;
    pub const ET: u32 = 1 << 4;
    pub const NE: u32 = 1 << 5;
    pub const WP: u32 = 1 << 16;
    pub const AM: u32 = 1 << 18;
    pub const NW: u32 = 1 << 29;
    pub const CD: u32 = 1 << 30;
    pub const PG: u32 = 1 << 31;
}

pub mod cr4 {
    pub const VME: u32 = 1 << 0;
    pub const PVI: u32 = 1 << 1;
    pub const TSD: u32 = 1 << 2;
    pub const DE: u32 = 1 << 3;
    pub const PSE: u32 = 1 << 4;
    pub const PAE: u32 = 1 << 5;
    pub const MCE: u32 = 1 << 6;
    pub const PGE: u32 = 1 << 7;
    pub const PCE: u32 = 1 << 8;
    pub const OSFXSR: u32 = 1 << 9;
    pub const OSXMMEXCPT: u32 = 1 << 10;
}

pub mod mxcsr {
    pub const DAZ: u32 = 1 << 6;
    pub const MASK_SHIFT: u32 = 7;
    pub const RC_SHIFT: u32 = 13;
    pub const FZ: u32 = 1 << 15;
    pub const RESET: u32 = 0x1f80;
    /// Writable bits (MXCSR_MASK as reported by FXSAVE).
    pub const WRITABLE: u32 = 0xffff;
}

pub const EAX: usize = 0;
pub const ECX: usize = 1;
pub const EDX: usize = 2;
pub const EBX: usize = 3;
pub const ESP: usize = 4;
pub const EBP: usize = 5;
pub const ESI: usize = 6;
pub const EDI: usize = 7;

pub const ES: usize = 0;
pub const CS: usize = 1;
pub const SS: usize = 2;
pub const DS: usize = 3;
pub const FS: usize = 4;
pub const GS: usize = 5;

/// Hidden descriptor-cache part of a segment register.
///
/// `attr` holds descriptor bits 40..=55: the access byte in bits 0..=7 and the
/// AVL, L, D/B and G bits in bits 12..=15.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Segment {
    pub selector: u16,
    pub attr: u16,
    pub base: u32,
    pub limit: u32,
}

pub mod attr {
    pub const ACCESSED: u16 = 1 << 0;
    pub const RW: u16 = 1 << 1;
    pub const DC: u16 = 1 << 2;
    pub const CODE: u16 = 1 << 3;
    pub const S: u16 = 1 << 4;
    pub const P: u16 = 1 << 7;
    pub const DB: u16 = 1 << 14;
    pub const G: u16 = 1 << 15;
}

impl Segment {
    pub const fn real(selector: u16) -> Self {
        Segment {
            selector,
            attr: attr::P | attr::S | attr::RW | attr::ACCESSED,
            base: (selector as u32) << 4,
            limit: 0xffff,
        }
    }

    pub fn dpl(&self) -> u8 {
        ((self.attr >> 5) & 3) as u8
    }

    pub fn present(&self) -> bool {
        self.attr & attr::P != 0
    }

    pub fn big(&self) -> bool {
        self.attr & attr::DB != 0
    }

    pub fn is_code(&self) -> bool {
        self.attr & (attr::S | attr::CODE) == (attr::S | attr::CODE)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct DescriptorTable {
    pub base: u32,
    pub limit: u16,
}

/// Cached x86 page translation. `tag` is the linear page number plus one, so 0 is empty.
#[derive(Clone, Copy, Debug, Default)]
pub struct TlbEntry {
    pub tag: u32,
    pub phys: u32,
    pub perms: u8,
}

pub mod tlb_perm {
    pub const USER_READ: u8 = 1;
    pub const USER_WRITE: u8 = 2;
    pub const SUPER_WRITE: u8 = 4;
    /// The mapping is global (PTE.G with CR4.PGE) and survives CR3 loads.
    pub const GLOBAL: u8 = 8;
}

/// What the most recent TLB flush covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushKind {
    All,
    /// A CR3 load with CR4.PGE set: global mappings stay.
    NonGlobal,
    Page(u32),
}

pub const TLB_SIZE: usize = 1024;

/// Software TLB for the interpreter. Like a hardware TLB it may hold stale
/// entries until CR3 is reloaded or INVLPG runs; write permission is only
/// cached once the Dirty bit is set.
#[derive(Clone, Debug)]
pub struct Tlb {
    pub entries: Box<[TlbEntry; TLB_SIZE]>,
    /// Incremented by every flush, so other translation caches can follow.
    pub generation: u64,
    pub last_flush: FlushKind,
}

impl Default for Tlb {
    fn default() -> Self {
        Tlb { entries: Box::new([TlbEntry::default(); TLB_SIZE]), generation: 0, last_flush: FlushKind::All }
    }
}

impl Tlb {
    pub fn flush(&mut self) {
        self.entries.fill(TlbEntry::default());
        self.generation += 1;
        self.last_flush = FlushKind::All;
    }

    pub fn flush_nonglobal(&mut self) {
        for e in self.entries.iter_mut() {
            if e.perms & tlb_perm::GLOBAL == 0 {
                *e = TlbEntry::default();
            }
        }
        self.generation += 1;
        self.last_flush = FlushKind::NonGlobal;
    }

    pub fn flush_page(&mut self, lin: u32) {
        self.generation += 1;
        self.last_flush = FlushKind::Page(lin & !0xfff);
        let e = &mut self.entries[(lin >> 12) as usize % TLB_SIZE];
        if e.tag == (lin >> 12) + 1 {
            *e = TlbEntry::default();
        }
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct Cpu {
    pub regs: [u32; 8],
    pub eip: u32,
    pub eflags: u32,
    pub segs: [Segment; 6],
    pub ldtr: Segment,
    pub tr: Segment,
    pub gdtr: DescriptorTable,
    pub idtr: DescriptorTable,
    pub cr0: u32,
    pub cr2: u32,
    pub cr3: u32,
    pub cr4: u32,
    pub dr: [u32; 8],
    pub tsc: u64,
    /// With `tsc_mul` set, the TSC counts host clock ticks since `tsc_origin`,
    /// times `tsc_mul`; `tsc` is then unused.
    pub tsc_origin: u64,
    pub tsc_mul: u64,
    pub sysenter_cs: u32,
    pub sysenter_esp: u32,
    pub sysenter_eip: u32,
    pub apic_base: u64,
    pub halted: bool,
    pub interrupt_shadow: bool,
    pub fpu: crate::fpu::Fpu,
    pub xmm: [u128; 8],
    pub mxcsr: u32,
    /// Level of the FERR# output. A waiting x87 instruction sets it, instead
    /// of raising #MF, when an unmasked x87 exception is pending and CR0.NE is
    /// clear. The board should assert IRQ13 while it is set (the PC/AT wiring
    /// of FERR#); FNCLEX, FNINIT, FLDENV, FRSTOR and FXRSTOR clear it once
    /// they leave no exception pending. The board may also clear it on a write
    /// to port 0xF0 (IGNNE#).
    pub ferr_pending: bool,
    pub tlb: Tlb,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        let mut cpu = Cpu {
            regs: [0; 8],
            eip: 0,
            eflags: 0,
            segs: [Segment::default(); 6],
            ldtr: Segment::default(),
            tr: Segment::default(),
            gdtr: DescriptorTable::default(),
            idtr: DescriptorTable::default(),
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            dr: [0; 8],
            tsc: 0,
            tsc_origin: 0,
            tsc_mul: 0,
            sysenter_cs: 0,
            sysenter_esp: 0,
            sysenter_eip: 0,
            apic_base: 0,
            halted: false,
            interrupt_shadow: false,
            fpu: crate::fpu::Fpu::default(),
            xmm: [0; 8],
            mxcsr: 0,
            ferr_pending: false,
            tlb: Tlb::default(),
        };
        cpu.reset();
        cpu
    }

    /// Puts the CPU in its power-on state (Intel SDM vol. 3, table 10-1).
    pub fn reset(&mut self) {
        self.regs = [0; 8];
        self.regs[EDX] = crate::cpuid::SIGNATURE;
        self.eip = 0xfff0;
        self.eflags = flags::FIXED1;
        for seg in &mut self.segs {
            *seg = Segment::real(0);
        }
        self.segs[CS] = Segment {
            selector: 0xf000,
            attr: attr::P | attr::S | attr::CODE | attr::RW | attr::ACCESSED,
            base: 0xffff_0000,
            limit: 0xffff,
        };
        self.ldtr = Segment { attr: attr::P | 2, limit: 0xffff, ..Default::default() };
        self.tr = Segment { attr: attr::P | 11, limit: 0xffff, ..Default::default() };
        self.gdtr = DescriptorTable { base: 0, limit: 0xffff };
        self.idtr = DescriptorTable { base: 0, limit: 0xffff };
        self.cr0 = cr0::ET | cr0::NW | cr0::CD;
        self.cr2 = 0;
        self.cr3 = 0;
        self.cr4 = 0;
        self.dr = [0, 0, 0, 0, 0, 0, 0xffff_0ff0, 0x400];
        self.apic_base = 0xfee0_0000 | (1 << 11) | (1 << 8);
        self.write_tsc(0);
        self.halted = false;
        self.interrupt_shadow = false;
        self.tlb.flush();
        self.fpu = crate::fpu::Fpu::default();
        self.xmm = [0; 8];
        self.mxcsr = mxcsr::RESET;
        self.ferr_pending = false;
    }

    /// Runs the TSC from the host clock at about 2.8 GHz.
    pub fn start_host_tsc(&mut self) {
        let ticks_per_sec = 1e9 * TIMEBASE.denom as f64 / TIMEBASE.numer as f64;
        self.tsc_mul = (2.8e9 / ticks_per_sec).round().max(1.0) as u64;
        self.write_tsc(0);
    }

    pub fn read_tsc(&self) -> u64 {
        if self.tsc_mul == 0 {
            return self.tsc;
        }
        host_ticks().wrapping_sub(self.tsc_origin).wrapping_mul(self.tsc_mul)
    }

    pub fn write_tsc(&mut self, v: u64) {
        self.tsc = v;
        if self.tsc_mul != 0 {
            self.tsc_origin = host_ticks().wrapping_sub(v / self.tsc_mul);
        }
    }

    pub fn protected_mode(&self) -> bool {
        self.cr0 & cr0::PE != 0
    }

    pub fn v86_mode(&self) -> bool {
        self.eflags & flags::VM != 0
    }

    pub fn cpl(&self) -> u8 {
        if !self.protected_mode() {
            0
        } else if self.v86_mode() {
            3
        } else {
            (self.segs[CS].selector & 3) as u8
        }
    }

    pub fn iopl(&self) -> u8 {
        ((self.eflags >> 12) & 3) as u8
    }

    pub fn flag(&self, f: u32) -> bool {
        self.eflags & f != 0
    }

    pub fn set_flag(&mut self, f: u32, on: bool) {
        if on {
            self.eflags |= f;
        } else {
            self.eflags &= !f;
        }
    }

    pub fn reg8(&self, r: usize) -> u8 {
        let v = self.regs[r & 3];
        if r < 4 { v as u8 } else { (v >> 8) as u8 }
    }

    pub fn set_reg8(&mut self, r: usize, v: u8) {
        let slot = &mut self.regs[r & 3];
        if r < 4 {
            *slot = (*slot & !0xff) | v as u32;
        } else {
            *slot = (*slot & !0xff00) | (v as u32) << 8;
        }
    }

    pub fn reg16(&self, r: usize) -> u16 {
        self.regs[r] as u16
    }

    pub fn set_reg16(&mut self, r: usize, v: u16) {
        self.regs[r] = (self.regs[r] & !0xffff) | v as u32;
    }
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

static TIMEBASE: std::sync::LazyLock<Timebase> = std::sync::LazyLock::new(|| {
    let mut t = Timebase { numer: 0, denom: 0 };
    unsafe { mach_timebase_info(&mut t) };
    t
});

/// The host's monotonic tick counter, which a Hypervisor.framework guest
/// reads as CNTVCT_EL0 when the vCPU's virtual timer offset is 0.
pub fn host_ticks() -> u64 {
    unsafe { mach_absolute_time() }
}

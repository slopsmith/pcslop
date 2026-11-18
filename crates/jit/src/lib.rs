//! Runs x86 code by translating it to ARM64 and executing it inside a
//! Hypervisor.framework VM. See docs/architecture.md, "VM runtime layout".

pub mod layout;
mod runtime;
pub mod shadow;
pub mod state;
mod translate;

use std::collections::BTreeMap;
use std::hash::{BuildHasherDefault, Hasher};

use hvf::{Exit, GuestMemory, Vcpu, VcpuKicker, Vm, mem, reg, sysreg};
use x86::bus::Bus;
use x86::cpu::{CS, Cpu, DS, ES, FS, FlushKind, GS, SS};
use x86::interp;

use runtime::Entries;

/// The Fx hash from rustc-hash (originally Firefox's hash, by the Rust project
/// developers): far cheaper than SipHash for the integer keys used here.
#[derive(Default, Clone, Copy)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7cc1_b727_220a_95);
    }

    fn write_u32(&mut self, v: u32) {
        self.write_u64(v as u64);
    }

    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type HashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;
type HashSet<K> = std::collections::HashSet<K, BuildHasherDefault<FxHasher>>;
use shadow::Shadow;
use state::{State, ctl, reason, resume};

/// VGA window plus the PAM-controlled ROM/shadow area: left unmapped so every
/// access goes through the board, which knows the current PAM settings.
const LEGACY_HOLE: std::ops::Range<u32> = 0xa_0000..0x10_0000;

struct BlockInfo {
    va: u64,
    boundaries: Vec<(u32, u32)>,
    insns: usize,
    interp_after: Option<usize>,
    x87: Vec<(u32, translate::X87Snap)>,
    /// Executions checked against the interpreter in verify mode.
    verified: u32,
}

/// Invalidations after which a page switches to self-checking blocks.
const SELF_CHECK_AFTER: u32 = 4;
/// More than any block's code. Blocks branch to the runtime with B, which
/// reaches 128 MiB, so none may start closer than this to the cache's end.
const MAX_BLOCK_BYTES: usize = 256 << 10;

/// Executions of each block that verify mode checks before running it freely.
const VERIFY_RUNS: u32 = 3;

/// Reads through to the real bus, keeps writes to itself; used to run the
/// interpreter alongside translated code without touching guest memory.
struct Overlay<'a, B: Bus> {
    inner: &'a mut B,
    writes: HashMap<u32, u8>,
}

impl<B: Bus> Bus for Overlay<'_, B> {
    fn read8(&mut self, addr: u32) -> u8 {
        match self.writes.get(&addr) {
            Some(&v) => v,
            None => self.inner.read8(addr),
        }
    }

    fn write8(&mut self, addr: u32, value: u8) {
        self.writes.insert(addr, value);
    }

    fn io_read(&mut self, _port: u16, _size: u8) -> u32 {
        0xffff_ffff
    }

    fn io_write(&mut self, _port: u16, _size: u8, _value: u32) {}
}

/// Adds the time until it is dropped to the stats bucket for one exit kind.
struct ExitTimer {
    start: std::time::Instant,
    slot: u8,
    stats: *mut Stats,
}

impl Drop for ExitTimer {
    fn drop(&mut self) {
        let ns = self.start.elapsed().as_nanos() as u64;
        let s = unsafe { &mut *self.stats };
        match self.slot {
            0 => s.miss_ns += ns,
            1 => s.interp_ns += ns,
            2 => s.fault_ns += ns,
            3 => s.mmio_ns += ns,
            _ => {}
        }
    }
}

/// Snapshots of the x86 paging structures the shadow tables were built from,
/// so a TLB flush can be skipped when nothing they depend on has changed.
#[derive(Default)]
struct PtWatch {
    key: Option<(u32, u32, u32)>,
    pages: HashMap<u32, Box<[u32; 1024]>>,
    /// Set while shadow entries exist that no snapshot covers, so no flush may be skipped.
    incomplete: bool,
}

/// Paging-structure pages watched before a full flush is cheaper than comparing.
const PT_WATCH_LIMIT: usize = 256;

impl PtWatch {
    fn cr_key(cpu: &Cpu) -> (u32, u32, u32) {
        use x86::cpu::{cr0, cr4};
        (cpu.cr3 & !0xfff, cpu.cr0 & (cr0::PG | cr0::WP | cr0::PE), cpu.cr4 & (cr4::PSE | cr4::PGE | cr4::PAE))
    }

    /// For an empty shadow tree.
    fn clear(&mut self) {
        self.key = None;
        self.pages.clear();
        self.incomplete = false;
    }

    /// For a new address space whose shadow tree keeps entries from the old one.
    fn forget(&mut self) {
        self.key = None;
        self.pages.clear();
        self.incomplete = true;
    }

    fn snapshot<B: Bus>(&mut self, bus: &mut B, page: u32) -> bool {
        if self.pages.contains_key(&page) {
            return true;
        }
        if self.pages.len() >= PT_WATCH_LIMIT {
            return false;
        }
        let mut copy = Box::new([0u32; 1024]);
        for (i, w) in copy.iter_mut().enumerate() {
            *w = bus.read32(page + i as u32 * 4);
        }
        self.pages.insert(page, copy);
        true
    }

    /// Takes in the Accessed/Dirty bits our walk just set.
    fn watch<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B, lin: u32) {
        let pde = bus.read32((cpu.cr3 & !0xfff) + (lin >> 22) * 4);
        let big = pde & 0x80 != 0 && cpu.cr4 & x86::cpu::cr4::PSE != 0;
        let pte = if pde & 1 != 0 && !big { bus.read32((pde & !0xfff) + ((lin >> 12) & 0x3ff) * 4) } else { 0 };
        self.watch_logged(cpu, bus, lin, pde, pte, big || pde & 1 == 0);
    }

    /// Records that a shadow entry for `lin` was built from `pde` and `pte`
    /// (unused for a big page).
    fn watch_logged<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B, lin: u32, pde: u32, pte: u32, big: bool) {
        let key = Self::cr_key(cpu);
        if self.key != Some(key) {
            if self.key.is_some() {
                self.forget();
            }
            self.key = Some(key);
        }
        let pd = cpu.cr3 & !0xfff;
        if !self.snapshot(bus, pd) {
            self.incomplete = true;
            return;
        }
        if let Some(copy) = self.pages.get_mut(&pd) {
            copy[(lin >> 22) as usize] = pde;
        }
        if !big {
            let pt = pde & !0xfff;
            if !self.snapshot(bus, pt) {
                self.incomplete = true;
                return;
            }
            if let Some(copy) = self.pages.get_mut(&pt) {
                copy[((lin >> 12) & 0x3ff) as usize] = pte;
            }
        }
    }

    fn still_valid<B: Bus>(&self, cpu: &Cpu, bus: &mut B) -> bool {
        if self.incomplete || self.key != Some(Self::cr_key(cpu)) {
            return false;
        }
        self.pages.iter().all(|(&page, copy)| {
            copy.iter().enumerate().all(|(i, &w)| bus.read32(page + i as u32 * 4) == w)
        })
    }
}

struct Expected {
    cpu: Cpu,
    writes: HashMap<u32, u8>,
    eip: u32,
    key: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub blocks: u64,
    pub translated_insns: u64,
    pub interp_exits: u64,
    pub miss_exits: u64,
    pub shadow_fills: u64,
    pub page_faults: u64,
    pub mmio_exits: u64,
    pub smc_invalidations: u64,
    pub stale_blocks: u64,
    pub vcache_bumps: u64,
    /// MISS exits for blocks that exist but had lost their lookup slot.
    pub lookup_misses: u64,
    /// Shadow entries filled inside the VM without an exit.
    pub vm_fills: u64,
    pub flushes: u64,
    pub flush_tlb_gen: u64,
    pub nonglobal_flushes: u64,
    pub flushes_avoided: u64,
    pub flush_single_all: u64,
    pub flush_multi: u64,
    pub flush_pool: u64,
    /// Shadow entries that map the graphics aperture straight to RAM.
    pub aperture_fills: u64,
    /// Times those entries were dropped because the aperture changed.
    pub aperture_flushes: u64,
    /// APIC TPR accesses served inside the VM.
    pub tpr_fast: u64,
    /// Nanoseconds inside `hv_vcpu_run`, and handling each kind of exit.
    pub vm_ns: u64,
    pub miss_ns: u64,
    pub interp_ns: u64,
    pub fault_ns: u64,
    pub mmio_ns: u64,
}

/// Why `Jit::run` returned.
#[derive(Debug)]
pub enum RunExit {
    /// The CPU left the modes the translator handles, halted, or the
    /// interpreter reported something other than `Continue`.
    Interp(interp::Exit),
    /// The vCPU was kicked; the caller should service devices.
    Kicked,
}

pub struct Jit {
    vcpu: Vcpu,
    mem: GuestMemory,
    entries: Entries,
    /// Shadow trees for CPL 0 and CPL 3 (only x86 user pages).
    shadow: [Shadow; 2],
    /// Which tree TTBR0 points at.
    root: usize,
    shadow_key: Option<u64>,
    /// Whether the shadow tables were filled with x86 paging enabled.
    shadow_paged: bool,
    /// Physical APIC page and whether its x86 mapping is global, while
    /// `State::apic_va` is set.
    apic: Option<(u32, bool)>,
    code_next: usize,
    blocks: HashMap<u64, BlockInfo>,
    ahead_cache: HashMap<u64, Ahead>,
    /// Blocks translated while their first instruction could not be read.
    retry: HashSet<u64>,
    by_va: BTreeMap<u64, u64>,
    code_pages: HashMap<u32, Vec<u64>>,
    /// Times each physical page lost its translations to writes.
    smc_counts: HashMap<u32, u32>,
    /// Pages written too often to write-protect: their blocks check their own
    /// code bytes on entry instead.
    self_check: HashSet<u32>,
    pt_watch: PtWatch,
    /// Chain sites (code cache offsets) waiting for a block at a physical address.
    chain_wanted: HashMap<u64, Vec<usize>>,
    /// Chain sites patched to jump straight into the block at a physical address.
    chain_in: HashMap<u64, Vec<usize>>,
    /// Shadow leaf entries for non-global x86 mappings, cleared on CR3 loads.
    nonglobal: [Vec<usize>; 2],
    /// Linear pages mapped writable in the shadow tables, by physical page.
    writable: [HashMap<u32, Vec<u32>>; 2],
    /// Shadow leaf entries for physical addresses above RAM (the aperture
    /// among them), cleared when `Bus::aperture_epoch` changes.
    aperture_leaves: [HashSet<usize>; 2],
    aperture_epoch: u64,
    pub stats: Stats,
    /// Runs each block against the interpreter and stops at the first difference.
    pub verify: bool,
    /// Fills shadow entries inside the VM; SLOPPER_JIT_NO_VM_FILL turns it off.
    pub vm_fill: bool,
    /// FEAT_AFP works in the guest, so SSE arithmetic can run on the A64 FPU.
    afp: bool,
    fpcr: u64,
    /// Verify only blocks with recent translations (SLOPPER_JIT_VERIFY=new).
    verify_recent_only: bool,
    /// Measures time spent in the VM and per exit kind (SLOPPER_JIT_PROFILE).
    timing: bool,
    audit: bool,
    /// Audit every this many runs (the value of SLOPPER_JIT_AUDIT).
    audit_every: u32,
    audit_tick: u32,
    audit_seen: HashSet<(usize, u32, u32)>,
    /// Interpreter exits by opcode, when profiling is on.
    interp_profile: Option<HashMap<u16, u64>>,
    /// Interpreter exits by (opcode key, EIP), when profiling is on.
    interp_sites: HashMap<(u16, u32), u64>,
    /// Where the vCPU was at each kick while profiling: guest EIP of the
    /// translated block, or a runtime offset (negative keys, 64-byte buckets).
    pc_samples: HashMap<i64, u64>,
    /// Profile samples per block key.
    block_samples: HashMap<u64, u64>,
    /// Profile samples per host PC in the code cache.
    host_samples: HashMap<u64, u64>,
    /// Declared last so it is dropped after the vCPU.
    vm: Vm,
    fb: Option<Framebuffer>,
    ram: usize,
    shadowed_rom: [Option<bool>; 16],
    pm_timer: Option<(u16, u64, u64)>,
}

/// Offset of the two-entry lookup bucket for `key`; the runtime's dispatcher
/// computes the same Fibonacci hash.
fn bucket(key: u64) -> usize {
    let shift = 64 - (layout::LOOKUP_ENTRIES.trailing_zeros() - 1);
    layout::LOOKUP + ((key.wrapping_mul(runtime::HASH_MUL) >> shift) as usize) * 32
}

struct Framebuffer {
    ipa: u64,
    host: usize,
    len: usize,
    /// Mapped writable after a write fault, until the renderer takes it.
    written: bool,
}

impl Framebuffer {
    fn contains(&self, ipa: u64) -> bool {
        (self.ipa..self.ipa + self.len as u64).contains(&ipa)
    }
}

/// Blocks are looked up by physical address, linear page (their code embeds
/// linear EIPs) and whether they run at CPL 3.
fn block_key(phys: u32, lin: u32, user: bool) -> u64 {
    phys as u64 | (user as u64) << 32 | ((lin >> 12) as u64) << 33
}

/// How `interp_run` treats the instruction at an address; REP string ops Stop,
/// since the interpreter would run them to the end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ahead {
    Exits,
    Runs,
    Stop,
}

const INTERP_RUN_MAX: usize = 4096;
const INTERP_RUN_TAIL: usize = 12;
const INTERP_RUN_SHORT: usize = 4;

/// True for CPU states whose code the translator can run: 32-bit protected
/// mode at CPL 0 or 3 with flat CS, SS, DS and ES.
pub fn supported(cpu: &Cpu) -> bool {
    if !cpu.protected_mode() || cpu.v86_mode() || !matches!(cpu.cpl(), 0 | 3) || !cpu.segs[CS].big() {
        return false;
    }
    [CS, SS, DS, ES].iter().all(|&s| {
        let seg = &cpu.segs[s];
        seg.base == 0 && seg.limit == 0xffff_ffff
    })
}

impl Jit {
    /// Creates the VM with `ram` (the board's RAM, at guest physical 0) mapped in.
    ///
    /// # Safety
    /// `ram` must stay valid for `ram_len` bytes for the lifetime of the `Jit`.
    pub unsafe fn new(ram: *mut u8, ram_len: usize) -> hvf::Result<Jit> {
        let vm = Vm::new(layout::IPA_BITS)?;
        let low = LEGACY_HOLE.start as usize;
        let high = LEGACY_HOLE.end as usize;
        unsafe {
            vm.map(ram, 0, low, mem::READ | mem::WRITE)?;
            vm.map(ram.add(high), high as u64, ram_len - high, mem::READ | mem::WRITE)?;
        }
        let mut rt = GuestMemory::new(layout::RUNTIME_SIZE).expect("runtime memory");
        let (code, entries) = runtime::build();
        rt.write_words(layout::VECTORS, &code);
        shadow::build_ttbr1(rt.as_mut_slice());
        rt.as_mut_slice()[layout::LOOKUP..layout::LOOKUP + layout::LOOKUP_ENTRIES * 16].fill(0xff);
        rt.as_mut_slice()[layout::RSTACK..layout::RSTACK + layout::RSTACK_BYTES].fill(0xff);
        unsafe { vm.map(rt.as_ptr(), layout::RUNTIME_IPA, layout::RUNTIME_SIZE, mem::RWX)? };

        let vcpu = Vcpu::new(&vm)?;
        let tcr: u64 = 32 | 1 << 8 | 1 << 10 | 3 << 12 | 25 << 16 | 1 << 24 | 1 << 26 | 3 << 28 | 2 << 30 | 1 << 32;
        let mut jit = Jit {
            vcpu,
            mem: rt,
            entries,
            shadow: {
                let mid = (layout::SHADOW_POOL + layout::SHADOW_POOL_END) / 2;
                [Shadow::new(layout::SHADOW_POOL, mid), Shadow::new(mid, layout::SHADOW_POOL_END)]
            },
            root: 0,
            shadow_key: None,
            shadow_paged: false,
            apic: None,
            code_next: layout::CODE_CACHE,
            blocks: HashMap::default(),
            ahead_cache: HashMap::default(),
            retry: HashSet::default(),
            by_va: BTreeMap::new(),
            code_pages: HashMap::default(),
            smc_counts: HashMap::default(),
            self_check: Default::default(),
            writable: [HashMap::default(), HashMap::default()],
            nonglobal: [Vec::new(), Vec::new()],
            aperture_leaves: [HashSet::default(), HashSet::default()],
            aperture_epoch: 0,
            pt_watch: PtWatch::default(),
            chain_wanted: HashMap::default(),
            chain_in: HashMap::default(),
            stats: Stats::default(),
            interp_profile: std::env::var_os("SLOPPER_JIT_PROFILE").map(|_| HashMap::default()),
            interp_sites: HashMap::default(),
            pc_samples: HashMap::default(),
            block_samples: HashMap::default(),
            host_samples: HashMap::default(),
            verify: std::env::var_os("SLOPPER_JIT_VERIFY").is_some(),
            vm_fill: std::env::var_os("SLOPPER_JIT_NO_VM_FILL").is_none(),
            afp: false,
            fpcr: 0,
            verify_recent_only: std::env::var("SLOPPER_JIT_VERIFY").is_ok_and(|v| v == "new"),
            timing: std::env::var_os("SLOPPER_JIT_PROFILE").is_some(),
            audit: std::env::var_os("SLOPPER_JIT_AUDIT").is_some(),
            audit_every: std::env::var("SLOPPER_JIT_AUDIT").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1),
            audit_tick: 0,
            audit_seen: HashSet::default(),
            vm,
            fb: None,
            ram: ram as usize,
            shadowed_rom: [None; 16],
            pm_timer: None,
        };
        for t in &mut jit.shadow {
            t.flush(jit.mem.as_mut_slice());
        }
        let root = jit.shadow[0].root_va();
        jit.state().shadow_root = root;
        jit.state().ram_size = ram_len as u32;
        let v = &jit.vcpu;
        v.set_sys_reg(sysreg::MAIR_EL1, 0xff)?;
        v.set_sys_reg(sysreg::TCR_EL1, tcr)?;
        v.set_sys_reg(sysreg::TTBR0_EL1, jit.shadow[0].root_ipa() | 1 << 48)?;
        v.set_sys_reg(sysreg::TTBR1_EL1, layout::ipa(layout::TTBR1_L1))?;
        v.set_sys_reg(sysreg::VBAR_EL1, layout::va(layout::VECTORS))?;
        v.set_sys_reg(sysreg::SP_EL1, layout::va(layout::STACK_TOP))?;
        v.set_sys_reg(sysreg::CPACR_EL1, 3 << 20)?;
        v.set_vtimer_offset(0)?;
        v.set_sys_reg(sysreg::SCTLR_EL1, 0x30d0_0800 | 1 | 1 << 2 | 1 << 12)?;
        v.set_reg(reg::CPSR, 0x3c5)?;
        v.set_reg(reg::PC, layout::va(entries.resume))?;
        v.set_reg(reg::X0 + 28, layout::va(layout::STATE))?;
        let probe = state::fpcr_for(x86::cpu::mxcsr::RESET);
        v.set_reg(reg::FPCR, probe)?;
        jit.afp = v.reg(reg::FPCR)? == probe && std::env::var_os("SLOPPER_JIT_NO_SSE").is_none();
        jit.fpcr = if jit.afp { probe } else { 0 };
        if !jit.afp {
            v.set_reg(reg::FPCR, 0)?;
        }
        jit.state().lookup = layout::va(layout::LOOKUP);
        jit.state().vlookup = layout::va(layout::VLOOKUP);
        Ok(jit)
    }

    /// Compares every shadow mapping with the x86 page tables and reports
    /// mappings that disagree (SLOPPER_JIT_AUDIT).
    fn audit<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B) {
        if cpu.cr0 & x86::cpu::cr0::PG == 0 {
            return;
        }
        self.audit_tick += 1;
        if self.audit_tick % self.audit_every != 0 {
            return;
        }
        let pse = cpu.cr4 & x86::cpu::cr4::PSE != 0;
        let pge = cpu.cr4 & x86::cpu::cr4::PGE != 0;
        let mut reported = 0;
        for r in 0..2 {
            let tracked: HashSet<usize> = self.nonglobal[r].iter().copied().collect();
            for (lin, phys, writable, off) in self.shadow[r].leaves(self.mem.as_slice()) {
                let pde = bus.read32((cpu.cr3 & !0xfff) + (lin >> 22) * 4);
                let want = if pde & 1 == 0 {
                    None
                } else if pde & 0x80 != 0 && pse {
                    Some(((pde & 0xffc0_0000) | (lin & 0x3f_f000), pde, pde))
                } else {
                    let pte = bus.read32((pde & !0xfff) + ((lin >> 12) & 0x3ff) * 4);
                    (pte & 1 != 0).then_some((pte & !0xfff, pde & pte, pte))
                };
                if writable && self.code_pages.contains_key(&phys) {
                    let listed = self.writable[r].get(&phys).is_some_and(|l| l.contains(&lin));
                    eprintln!("AUDIT writable code page: root {r} lin {lin:#010x} phys {phys:#x} listed {listed} x86 {want:x?}");
                }
                let want = want.map(|(p, perms, leaf)| (if p != phys { bus.aperture_page(p).unwrap_or(p) } else { p }, perms, leaf));
                let why = match want {
                    None => "not present",
                    Some((p, _, _)) if p != phys => "other page",
                    Some((_, perms, _)) if writable && perms & 2 == 0 => "writable",
                    Some((_, perms, _)) if r == 1 && perms & 4 == 0 => "supervisor page in user tree",
                    Some((_, _, leaf)) if !(pge && leaf & 0x100 != 0) && !tracked.contains(&off) => "untracked non-global",
                    _ => continue,
                };
                if reported < 10 && self.audit_seen.insert((r, lin, phys)) {
                    reported += 1;
                    eprintln!(
                        "AUDIT {why}: root {r} lin {lin:#010x} shadow {phys:#x}{} x86 {want:x?} gen {}",
                        if writable { " rw" } else { " ro" },
                        cpu.tlb.generation
                    );
                }
            }
        }
    }

    /// The runtime's shared state fields, for the control socket.
    pub fn debug_state(&mut self) -> String {
        let s = self.state();
        format!(
            "jit eip={:#x} reason={} pending={} irq={} if={} budget={} user={} ctl={}",
            s.eip, s.reason, s.pending, s.irq, s.if_flag, s.budget, s.user, s.ctl
        )
    }

    pub fn profiling(&self) -> bool {
        self.interp_profile.is_some()
    }

    /// Turns exit timing and the interpreter-exit histogram on or off.
    pub fn set_profiling(&mut self, on: bool) {
        self.timing = on;
        self.interp_profile = on.then(HashMap::default);
        self.interp_sites.clear();
        self.pc_samples.clear();
        self.block_samples.clear();
        self.host_samples.clear();
        let s = &mut self.stats;
        (s.vm_ns, s.miss_ns, s.interp_ns, s.fault_ns, s.mmio_ns) = (0, 0, 0, 0, 0);
    }

    /// Time per exit kind and the most frequent interpreted opcodes since profiling began.
    pub fn profile_report(&self) -> String {
        let Some(p) = &self.interp_profile else { return "profiling is off (profile on)".into() };
        let s = &self.stats;
        let ms = |ns: u64| ns as f64 / 1e6;
        let mut v: Vec<_> = p.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        let top: Vec<String> = v.iter().take(40).map(|(k, n)| format!("{k:04x}:{n}")).collect();
        let mut sites: Vec<_> = self.interp_sites.iter().collect();
        sites.sort_by(|a, b| b.1.cmp(a.1));
        let sites: Vec<String> = sites.iter().take(20).map(|((k, eip), n)| format!("{k:04x}@{eip:08x}:{n}")).collect();
        let total: u64 = self.pc_samples.values().sum();
        let mut pcs: Vec<_> = self.pc_samples.iter().collect();
        pcs.sort_by(|a, b| b.1.cmp(a.1));
        let runtime: u64 = self.pc_samples.iter().filter(|(k, _)| **k < 0).map(|(_, n)| n).sum();
        let hot: Vec<String> = pcs
            .iter()
            .take(30)
            .map(|(k, n)| if **k < 0 { format!("rt+{:#x}:{n}", -**k - 1) } else { format!("{:#x}:{n}", **k) })
            .collect();
        format!(
            "ms in vm {:.0} miss {:.0} interp {:.0} fault {:.0} mmio {:.0}\ninterpreter exits by opcode: {}\nby site: {}\n{total} samples, {runtime} in the runtime; hottest: {}",
            ms(s.vm_ns),
            ms(s.miss_ns),
            ms(s.interp_ns),
            ms(s.fault_ns),
            ms(s.mmio_ns),
            top.join(" "),
            sites.join(" "),
            hot.join(" ")
        )
    }

    /// Writes the `n` blocks with the most profile samples into `dir`: for
    /// each, `NN.x86` (guest code from the block's first instruction) and
    /// `NN.a64` (its translation), plus `index.txt` listing rank, samples,
    /// guest address, instruction count and host size. `scripts/jit-blocks`
    /// disassembles them side by side.
    pub fn dump_hot_blocks<B: Bus>(&self, bus: &mut B, dir: &std::path::Path, n: usize) -> std::io::Result<String> {
        std::fs::create_dir_all(dir)?;
        let mut hot: Vec<(u64, u64)> = self.block_samples.iter().map(|(k, n)| (*k, *n)).collect();
        hot.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u64 = self.pc_samples.values().sum();
        let mut index = format!("# rank samples share eip phys user insns host_bytes (of {total} samples)\n");
        for (rank, (key, samples)) in hot.iter().take(n).enumerate() {
            let Some(b) = self.blocks.get(key) else { continue };
            let end = self.by_va.range(b.va + 1..).next().map_or(layout::va(self.code_next), |(va, _)| *va);
            let off = (b.va - layout::RUNTIME_VA) as usize;
            let host = &self.mem.as_slice()[off..off + (end - b.va) as usize];
            let phys = *key as u32;
            let first = b.boundaries.first().map_or(0, |(_, e)| *e);
            let last = b.boundaries.last().map_or(first, |(_, e)| *e);
            let len = (last.wrapping_sub(first) + 16).min(4096);
            let guest: Vec<u8> = (0..len).map(|i| bus.read8(phys.wrapping_add(i))).collect();
            std::fs::write(dir.join(format!("{rank:02}.x86")), &guest)?;
            std::fs::write(dir.join(format!("{rank:02}.a64")), host)?;
            let mut hits = String::new();
            for word in 0..host.len() / 4 {
                if let Some(n) = self.host_samples.get(&(b.va + 4 * word as u64)) {
                    hits += &format!("{word} {n}\n");
                }
            }
            std::fs::write(dir.join(format!("{rank:02}.hits")), hits)?;
            index += &format!(
                "{rank:02} {samples} {:.1}% {first:08x} {phys:08x} {} {} {}\n",
                *samples as f64 * 100.0 / total.max(1) as f64,
                (key >> 32) & 1,
                b.insns,
                host.len()
            );
        }
        std::fs::write(dir.join("index.txt"), &index)?;
        Ok(index)
    }

    /// Writes back the x87 state held in registers at the boundary before `pc`
    /// when the host abandons a block; `xregs` are x2..x4 (TOP, TAGS, SW).
    fn x87_deopt(&mut self, pc: u64, xregs: [u64; 3]) -> hvf::Result<()> {
        let Some((&va, &key)) = self.by_va.range(..=pc).next_back() else { return Ok(()) };
        let b = &self.blocks[&key];
        let word = ((pc - va) / 4) as u32;
        let Some(&(start, _)) = b.boundaries.iter().rev().find(|(w, _)| *w <= word) else { return Ok(()) };
        let Some(&(_, snap)) = b.x87.iter().find(|(w, _)| *w == start) else { return Ok(()) };
        let top = xregs[0] as usize & 7;
        for r in 0..8 {
            if snap.dirty & 1 << r != 0 {
                let v = f64::from_bits(self.vcpu.simd_reg(8 + r as u32)? as u64);
                let f = x86::fpu::F80::from_f64(v);
                let p = (top + r) & 7;
                let s = self.state();
                s.mm[p] = f.mant;
                s.fse[p] = f.se;
            }
        }
        if snap.meta {
            let s = self.state();
            s.ftags = xregs[1] as u32;
            s.fsw = xregs[2] as u32;
            s.fip = snap.note.0;
            s.fnote = snap.note.1;
        }
        Ok(())
    }

    /// `x87_deopt` for a FAULT exit, where the runtime saved x0..x17 in `scratch`.
    fn deopt_at_fault(&mut self, elr: u64) {
        let s = self.state();
        let xregs = [s.scratch[2], s.scratch[3], s.scratch[4]];
        self.x87_deopt(elr, xregs).expect("reading the vCPU's SIMD registers");
    }

    fn sample_pc(&mut self) -> hvf::Result<()> {
        let pc = self.vcpu.reg(reg::PC)?;
        let code = layout::va(layout::CODE_CACHE)..layout::va(layout::CODE_CACHE_END);
        let key = if code.contains(&pc) {
            if let Some((_, &block)) = self.by_va.range(..=pc).next_back() {
                *self.block_samples.entry(block).or_default() += 1;
            }
            *self.host_samples.entry(pc).or_default() += 1;
            self.eip_at(pc).map_or(-1, |e| e as i64)
        } else {
            -((pc.wrapping_sub(layout::RUNTIME_VA) & !63) as i64) - 1
        };
        *self.pc_samples.entry(key).or_default() += 1;
        Ok(())
    }

    /// Maps a display's framebuffer (guest physical base, host memory, size)
    /// so translated code accesses it without exits, or unmaps it with `None`.
    /// The first write after each `take_framebuffer_written` faults once to
    /// record it.
    ///
    /// # Safety
    /// The host memory must stay valid and 16 KiB aligned while mapped.
    pub unsafe fn set_framebuffer(&mut self, fb: Option<(u32, *mut u8, usize)>) -> hvf::Result<()> {
        let want = fb.map(|(ipa, host, len)| (ipa as u64, host as usize, len));
        if self.fb.as_ref().map(|f| (f.ipa, f.host, f.len)) == want {
            return Ok(());
        }
        if let Some(f) = self.fb.take() {
            self.vm.unmap(f.ipa, f.len)?;
        }
        if let Some((ipa, host, len)) = want {
            unsafe { self.vm.map(host as *mut u8, ipa, len, mem::READ)? };
            self.fb = Some(Framebuffer { ipa, host, len, written: false });
        }
        Ok(())
    }

    /// Maps the 16 KiB chunks of 0xC0000..0x100000 that read from RAM, as
    /// `Board::shadowed_rom` reports them, so translated code reads shadowed
    /// ROMs without exits; writes exit unless the chunk is writable.
    /// The ACPI PM timer's port and formula from `Board::pm_timer`, so
    /// translated kernel code reads it without an exit.
    pub fn set_pm_timer(&mut self, timer: Option<(u16, u64, u64)>) {
        self.pm_timer = timer;
    }

    pub fn set_shadowed_rom(&mut self, chunks: [Option<bool>; 16]) -> hvf::Result<()> {
        for (i, &want) in chunks.iter().enumerate() {
            if want == self.shadowed_rom[i] {
                continue;
            }
            let ipa = 0xc_0000 + i * 0x4000;
            if self.shadowed_rom[i].is_some() {
                self.vm.unmap(ipa as u64, 0x4000)?;
            }
            if let Some(writable) = want {
                let flags = if writable { mem::READ | mem::WRITE } else { mem::READ };
                unsafe { self.vm.map((self.ram + ipa) as *mut u8, ipa as u64, 0x4000, flags)? };
            }
            self.shadowed_rom[i] = want;
        }
        Ok(())
    }

    /// True if translated code wrote the framebuffer since the last call.
    pub fn take_framebuffer_written(&mut self) -> hvf::Result<bool> {
        let Some(f) = self.fb.as_mut().filter(|f| f.written) else { return Ok(false) };
        self.vm.protect(f.ipa, f.len, mem::READ)?;
        f.written = false;
        Ok(true)
    }

    /// Drops every translation and shadow mapping, for when guest memory
    /// and CPU state were replaced wholesale, as by restoring a snapshot.
    pub fn forget_guest(&mut self) {
        self.reset_code_cache();
        self.ahead_cache.clear();
        self.smc_counts.clear();
        self.self_check.clear();
        self.shadow_key = None;
        self.shadow_paged = false;
        self.state().ctl |= ctl::IC_IALLU;
    }

    /// x86 instructions retired by translated code so far.
    pub fn instructions(&mut self) -> u64 {
        self.state().icount
    }

    pub fn kicker(&self) -> VcpuKicker {
        self.vcpu.kicker()
    }

    fn state(&mut self) -> &mut State {
        unsafe { &mut *(self.mem.as_ptr().add(layout::STATE) as *mut State) }
    }

    fn load_state(&mut self, cpu: &Cpu) {
        let pm_timer = self.pm_timer;
        let vm_fill = self.vm_fill;
        let afp = self.afp;
        let ttbr0 = [0, 1].map(|r| self.shadow[r].root_ipa() | (r as u64 + 1) << 48);
        let roots = [0, 1].map(|r| self.shadow[r].root_va());
        let s = self.state();
        let x87_fpcr = std::mem::take(&mut s.fpcr_x87) != 0;
        (s.x87_taint, s.fpsr_saved) = (0, 0);
        if x87_fpcr {
            self.fpcr = u64::MAX;
        }
        if afp {
            let fpcr = state::fpcr_for(cpu.mxcsr);
            if fpcr != self.fpcr {
                self.fpcr = fpcr;
                self.vcpu.set_reg(reg::FPCR, fpcr).expect("FPCR write");
            }
        }
        let s = self.state();
        s.regs = cpu.regs;
        s.eip = cpu.eip;
        s.set_eflags(cpu.eflags);
        {
            use x86::cpu::flags::*;
            s.eflags_rest = cpu.eflags & !(ARITH | IF | DF);
        }
        s.if_flag = cpu.flag(x86::cpu::flags::IF) as u32;
        s.df = cpu.flag(x86::cpu::flags::DF) as u32;
        s.fs_base = cpu.segs[FS].base;
        s.cs_sel = cpu.segs[CS].selector as u32;
        s.gs_base = cpu.segs[GS].base;
        {
            use x86::cpu::{cr0, cr4};
            let mut f = 0;
            if vm_fill && cpu.cr0 & cr0::PG != 0 && cpu.cr4 & cr4::PAE == 0 {
                f |= state::walk::ON;
            }
            if cpu.cr4 & cr4::PSE != 0 {
                f |= state::walk::PSE;
            }
            if cpu.cr4 & cr4::PGE != 0 {
                f |= state::walk::PGE;
            }
            if cpu.cr0 & cr0::WP != 0 {
                f |= state::walk::WP;
            }
            s.walk_flags = f;
            s.walk_cr3 = cpu.cr3 & !0xfff;
        }
        let ts_em = x86::cpu::cr0::TS | x86::cpu::cr0::EM;
        s.mm = std::array::from_fn(|i| cpu.fpu.mm(i));
        s.mmx_ok = (cpu.cr0 & ts_em == 0 && cpu.fpu.sw & x86::fpu::sw::ES == 0) as u32;
        s.mm_used = 0;
        s.fp64_valid = 0;
        let f = &cpu.fpu;
        s.fse = std::array::from_fn(|i| f.regs[i].se);
        s.fcw = f.cw as u32;
        s.fsw = f.sw as u32;
        s.ftags = f.tags as u32;
        let base = cpu.cr0 & ts_em == 0 && f.sw & x86::fpu::sw::ES == 0;
        s.x87_base = base as u32;
        s.x87_ok = state::x87_ok(base, f.cw) as u32;
        s.fip = f.fip;
        s.fdp = f.fdp;
        s.fnote = 0;
        s.fdseg = 0;
        let sse_ok = cpu.cr0 & ts_em == 0 && cpu.cr4 & x86::cpu::cr4::OSFXSR != 0;
        s.sse_ok = sse_ok as u32;
        s.xmm = cpu.xmm;
        s.mxcsr = cpu.mxcsr;
        s.fpsr = 0;
        s.ds_sel = cpu.segs[DS].selector as u32;
        s.ss_sel = cpu.segs[SS].selector as u32;
        s.es_sel = cpu.segs[ES].selector as u32;
        s.fs_sel = cpu.segs[FS].selector as u32;
        s.gs_sel = cpu.segs[GS].selector as u32;
        s.gdt_base = cpu.gdtr.base;
        s.gdt_limit = cpu.gdtr.limit as u32;
        s.sysenter_cs = cpu.sysenter_cs;
        s.sysenter_esp = cpu.sysenter_esp;
        s.sysenter_eip = cpu.sysenter_eip;
        s.cr0 = cpu.cr0;
        s.cr2 = cpu.cr2;
        s.cr3 = cpu.cr3;
        s.cr4 = cpu.cr4;
        s.ttbr0 = ttbr0;
        s.roots = roots;
        s.tsc_origin = cpu.tsc_origin;
        s.tsc_mul = cpu.tsc_mul;
        let (port, mul, add) = pm_timer.map_or((u32::MAX, 0, 0), |(p, m, a)| (p as u32, m, a));
        (s.pm_port, s.pm_mul, s.pm_add) = (port, mul, add);
        s.dr = cpu.dr;
        s.ldtr_nulled = 0;
        s.sse_fp_ok = (sse_ok && afp && cpu.mxcsr & 0x1f80 == 0x1f80) as u32;
    }

    fn store_state(&mut self, cpu: &mut Cpu) {
        let s = self.state();
        cpu.regs = s.regs;
        cpu.eip = s.eip;
        let (if_flag, df) = (s.if_flag != 0, s.df != 0);
        {
            use x86::cpu::flags::{ARITH, DF, IF};
            cpu.eflags = s.merge_eflags(cpu.eflags) & (ARITH | IF | DF) | s.eflags_rest;
        }
        cpu.set_flag(x86::cpu::flags::IF, if_flag);
        cpu.set_flag(x86::cpu::flags::DF, df);
        cpu.xmm = s.xmm;
        cpu.dr = s.dr;
        cpu.mxcsr = s.mxcsr | state::mxcsr_flags(s.fpsr);
        s.fpsr = 0;
        let f = &mut cpu.fpu;
        for i in 0..8 {
            f.regs[i] = x86::fpu::F80 { mant: s.mm[i], se: s.fse[i] };
        }
        f.cw = s.fcw as u16;
        f.sw = s.fsw as u16;
        f.tags = s.ftags as u8;
        if s.fnote != 0 {
            f.fop = s.fnote as u16 & 0x7ff;
            f.fip = s.fip;
            f.fcs = cpu.segs[CS].selector;
        }
        if s.fdseg != 0 {
            f.fdp = s.fdp;
            f.fds = cpu.segs[(s.fdseg & 7) as usize].selector;
        }
        let used = s.mm_used;
        if used != 0 {
            for i in 0..8 {
                if used & 1 << i != 0 {
                    cpu.fpu.set_mm(i, s.mm[i]);
                }
            }
            cpu.fpu.enter_mmx();
        }
    }

    /// Takes in a SYSENTER, SYSEXIT or FS/GS load that translated code did,
    /// which switched CPL, segments and the shadow tree without an exit.
    fn sync_mode<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) {
        use x86::cpu::{Segment, attr};
        let (user, cs, fs, gs, fs_base, gs_base) = {
            let s = self.state();
            (s.user, s.cs_sel, s.fs_sel, s.gs_sel, s.fs_base, s.gs_base)
        };
        if std::mem::take(&mut self.state().ldtr_nulled) != 0 {
            let sel = self.state().ldtr_sel as u16;
            cpu.ldtr = x86::cpu::Segment { selector: sel, ..Default::default() };
        }
        self.root = user as usize;
        if cs != cpu.segs[CS].selector as u32 {
            let flat = attr::P | attr::S | attr::RW | attr::ACCESSED | attr::DB | attr::G;
            let dpl = if user != 0 { 3 << 5 } else { 0 };
            let base = cpu.sysenter_cs as u16 & !3;
            let (cs_sel, ss_sel) = if user != 0 { ((base + 16) | 3, (base + 24) | 3) } else { (base, base + 8) };
            cpu.segs[CS] = Segment { selector: cs_sel, attr: flat | attr::CODE | dpl, base: 0, limit: 0xffff_ffff };
            cpu.segs[SS] = Segment { selector: ss_sel, attr: flat | dpl, base: 0, limit: 0xffff_ffff };
        }
        // The same selector can come back with another base: XP rewrites the
        // TEB descriptor on every thread switch.
        for (seg, sel, base) in [(FS, fs, fs_base), (GS, gs, gs_base)] {
            if sel != cpu.segs[seg].selector as u32 || base != cpu.segs[seg].base {
                let ok = interp::load_segment(cpu, bus, seg, sel as u16);
                assert!(ok, "translated code loaded selector {sel:#x} that the interpreter rejects");
                cpu.segs[seg].base = base;
            }
        }
    }

    /// Points TTBR0 at the shadow tree for the current CPL, each with its own ASID.
    fn set_root(&mut self, cpu: &Cpu) {
        let r = (cpu.cpl() == 3) as usize;
        if r != self.root {
            self.root = r;
            let ttbr = self.shadow[r].root_ipa() | (r as u64 + 1) << 48;
            self.vcpu.set_sys_reg(sysreg::TTBR0_EL1, ttbr).expect("TTBR0 write");
            let root = self.shadow[r].root_va();
            self.state().shadow_root = root;
        }
        self.state().user = r as u32;
    }

    /// Invalidates every dispatcher cache entry.
    fn bump_vcache(&mut self) {
        self.stats.vcache_bumps += 1;
        let s = self.state();
        s.vepoch = s.vepoch.wrapping_add(1);
        s.gepoch = s.gepoch.wrapping_add(1);
    }

    /// Invalidates the dispatcher cache entries for code on non-global pages.
    fn bump_vcache_nonglobal(&mut self) {
        self.stats.vcache_bumps += 1;
        let s = self.state();
        s.vepoch = s.vepoch.wrapping_add(1);
    }

    fn disarm_apic(&mut self) {
        self.apic = None;
        self.state().apic_va = 0;
    }

    fn flush_shadow(&mut self) {
        self.disarm_apic();
        self.bump_vcache();
        self.pt_watch.clear();
        for t in &mut self.shadow {
            t.flush(self.mem.as_mut_slice());
        }
        for r in 0..2 {
            self.writable[r].clear();
            self.nonglobal[r].clear();
            self.aperture_leaves[r].clear();
        }
        self.state().ctl |= ctl::TLBI_ALL;
        self.stats.flushes += 1;
    }

    /// Follows the interpreter's TLB: CR3/CR0/CR4 writes and INVLPG flush it.
    fn check_shadow<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B) {
        self.check_aperture(bus);
        let key = cpu.tlb.generation;
        let paged = cpu.cr0 & x86::cpu::cr0::PG != 0;
        let was_paged = std::mem::replace(&mut self.shadow_paged, paged);
        match self.shadow_key {
            Some(k) if k == key => {}
            Some(_) if !paged && !was_paged => self.shadow_key = Some(key),
            Some(_) if paged && was_paged && self.pt_watch.still_valid(cpu, bus) => {
                self.shadow_key = Some(key);
                self.stats.flushes_avoided += 1;
            }
            Some(k) if k + 1 == key && cpu.tlb.last_flush != FlushKind::All => {
                self.shadow_key = Some(key);
                match cpu.tlb.last_flush {
                    FlushKind::Page(lin) => {
                        if self.state().apic_va == u64::from(lin & !0xfff) | 1 {
                            self.disarm_apic();
                        }
                        self.bump_vcache();
                        for t in &mut self.shadow {
                            t.unmap(self.mem.as_mut_slice(), lin);
                        }
                        self.tlbi_page(lin);
                    }
                    _ => {
                        if self.apic.is_some_and(|(_, global)| !global) {
                            self.disarm_apic();
                        }
                        self.bump_vcache_nonglobal();
                        self.pt_watch.forget();
                        for r in 0..2 {
                            for off in std::mem::take(&mut self.nonglobal[r]) {
                                self.shadow[r].clear_leaf(self.mem.as_mut_slice(), off);
                            }
                        }
                        self.state().ctl |= ctl::TLBI_ALL;
                        self.stats.nonglobal_flushes += 1;
                    }
                }
            }
            Some(k) => {
                if k + 1 == key {
                    self.stats.flush_single_all += 1;
                } else {
                    self.stats.flush_multi += 1;
                }
                self.shadow_key = Some(key);
                self.stats.flush_tlb_gen += 1;
                self.flush_shadow();
            }
            None => {
                self.shadow_key = Some(key);
                self.flush_shadow();
            }
        }
    }

    /// Drops the shadow entries of addresses above RAM once the chipset's
    /// aperture translations may have changed, so they fill again.
    fn check_aperture<B: Bus>(&mut self, bus: &mut B) {
        let epoch = bus.aperture_epoch();
        if epoch == self.aperture_epoch {
            return;
        }
        self.aperture_epoch = epoch;
        if self.aperture_leaves.iter().all(HashSet::is_empty) {
            return;
        }
        for r in 0..2 {
            for off in std::mem::take(&mut self.aperture_leaves[r]) {
                self.shadow[r].clear_leaf(self.mem.as_mut_slice(), off);
            }
        }
        self.bump_vcache();
        self.state().ctl |= ctl::TLBI_ALL;
        self.stats.aperture_flushes += 1;
    }

    fn tlbi_page(&mut self, lin: u32) {
        let s = self.state();
        if s.ctl & ctl::TLBI_PAGE != 0 && s.tlbi_page != (lin >> 12) as u64 {
            s.ctl |= ctl::TLBI_ALL;
        }
        s.tlbi_page = (lin >> 12) as u64;
        s.ctl |= ctl::TLBI_PAGE;
    }

    fn map_shadow<B: Bus>(&mut self, bus: &mut B, cpu: &Cpu, lin: u32, phys: u32, writable: bool, global: bool) -> usize {
        if cpu.cr0 & x86::cpu::cr0::PG != 0 {
            self.pt_watch.watch(cpu, bus, lin);
        }
        let r = self.root;
        // The TLB may hold the entry this replaces, possibly writable to a page
        // that is about to become code.
        if let Some(old) = self.shadow[r].phys(self.mem.as_slice(), lin) {
            self.tlbi_page(lin);
            if old != phys & !0xfff {
                self.bump_vcache();
            }
        }
        let leaf = match self.shadow[r].map(self.mem.as_mut_slice(), lin, phys, writable, global) {
            Some(off) => off,
            None => {
                self.stats.flush_pool += 1;
                self.flush_shadow();
                self.shadow[r].map(self.mem.as_mut_slice(), lin, phys, writable, global).expect("shadow pool smaller than one mapping")
            }
        };
        if writable {
            self.writable[r].entry(phys & !0xfff).or_default().push(lin & !0xfff);
        }
        if !global {
            self.nonglobal[r].push(leaf);
        }
        self.stats.shadow_fills += 1;
        leaf
    }

    /// Points the `b` at `site` to `target_va`, or back to the slow path right after it.
    fn patch_chain(&mut self, site: usize, target_va: Option<u64>) {
        let word = match target_va {
            Some(va) => {
                let delta = (va as i64 - layout::va(site) as i64) / 4;
                0x1400_0000 | (delta as u32 & 0x03ff_ffff)
            }
            None => 0x1400_0001,
        };
        self.mem.write_words(site, &[word]);
        self.state().ctl |= ctl::IC_IALLU;
    }

    fn invalidate_code_page(&mut self, page: u32) {
        let Some(keys) = self.code_pages.remove(&page) else { return };
        self.set_code_bit(page, false);
        for key in keys {
            self.remove_block(key);
        }
        self.stats.smc_invalidations += 1;
        let n = self.smc_counts.entry(page).or_default();
        *n += 1;
        if *n >= SELF_CHECK_AFTER && !LEGACY_HOLE.contains(&page) {
            self.self_check.insert(page);
        }
    }

    /// Forgets the block for `key`: unlinks chains into it and clears its lookup slot.
    fn remove_block(&mut self, key: u64) {
        self.bump_vcache();
        self.retry.remove(&key);
        for site in self.chain_in.remove(&key).unwrap_or_default() {
            self.patch_chain(site, None);
            self.chain_wanted.entry(key).or_default().push(site);
        }
        if let Some(b) = self.blocks.remove(&key) {
            self.by_va.remove(&b.va);
        }
        self.clear_lookup(key);
    }

    fn clear_lookup(&mut self, key: u64) {
        self.bump_vcache();
        let b = bucket(key);
        for off in [b, b + 16] {
            let slot = &mut self.mem.as_mut_slice()[off..off + 16];
            if u64::from_le_bytes(slot[..8].try_into().unwrap()) == key {
                slot.fill(0xff);
            }
        }
    }

    fn reset_code_cache(&mut self) {
        self.bump_vcache();
        self.retry.clear();
        self.chain_wanted.clear();
        self.chain_in.clear();
        self.blocks.clear();
        self.by_va.clear();
        self.code_pages.clear();
        self.mem.as_mut_slice()[layout::CODE_BITMAP..layout::CODE_BITMAP_END].fill(0);
        self.code_next = layout::CODE_CACHE;
        self.mem.as_mut_slice()[layout::LOOKUP..layout::LOOKUP + layout::LOOKUP_ENTRIES * 16].fill(0xff);
        self.flush_shadow();
    }

    fn translate_block<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B, eip: u32, phys: u32, user: bool) {
        if self.code_next + MAX_BLOCK_BYTES > layout::CODE_CACHE_END {
            self.reset_code_cache();
        }
        let key = block_key(phys, eip, user);
        let page = phys & !0xfff;
        let lin_page = eip & !0xfff;
        let block_va = layout::va(self.code_next);
        let rt_va = layout::RUNTIME_VA;
        let entries = self.entries;
        let next_lin = lin_page.wrapping_add(0x1000);
        let mut next_phys: Option<Option<u32>> = None;
        let compiled = translate::compile(
            eip,
            |a| {
                if a & !0xfff == lin_page {
                    return Some(bus.read8(page | (a & 0xfff)));
                }
                if a & !0xfff != next_lin || next_lin == 0 {
                    return None;
                }
                let p = *next_phys.get_or_insert_with(|| interp::translate(cpu, bus, next_lin, false).ok());
                p.map(|p| bus.read8(p | (a & 0xfff)))
            },
            block_va,
            rt_va,
            &entries,
            !self.verify,
            user,
            self.self_check.contains(&page),
            self.verify,
        );
        let bytes = compiled.code.len() * 4;
        if self.code_next + bytes > layout::CODE_CACHE_END {
            self.reset_code_cache();
            return self.translate_block(cpu, bus, eip, phys, user);
        }
        let code_off = self.code_next;
        self.mem.write_words(code_off, &compiled.code);
        self.code_next += bytes.next_multiple_of(16);
        for &(word, target) in &compiled.chains {
            let target = block_key(page | (target & 0xfff), target, user);
            let site = code_off + word as usize * 4;
            match self.blocks.get(&target).map(|b| b.va) {
                Some(va) if target != key => {
                    self.patch_chain(site, Some(va));
                    self.chain_in.entry(target).or_default().push(site);
                }
                _ if target == key => {
                    self.patch_chain(site, Some(block_va));
                    self.chain_in.entry(key).or_default().push(site);
                }
                _ => self.chain_wanted.entry(target).or_default().push(site),
            }
        }
        for site in self.chain_wanted.remove(&key).unwrap_or_default() {
            self.patch_chain(site, Some(block_va));
            self.chain_in.entry(key).or_default().push(site);
        }
        if !self.verify {
            self.insert_lookup(key, block_va);
        }
        // REP string ops can run for millions of iterations; replaying them on
        // the verifier's overlay is too slow, so those blocks are trusted.
        let trusted = compiled.has_rep || (self.verify_recent_only && !compiled.recent);
        let verified = if trusted { VERIFY_RUNS } else { 0 };
        if self.verify && trusted {
            self.insert_lookup(key, block_va);
        }
        if compiled.retry {
            self.retry.insert(key);
        }
        self.blocks.insert(key, BlockInfo { va: block_va, boundaries: compiled.boundaries, insns: compiled.insns, interp_after: compiled.interp_after, x87: compiled.x87, verified });
        self.by_va.insert(block_va, key);
        self.stats.blocks += 1;
        self.stats.translated_insns += compiled.insns as u64;
        self.state().ctl |= ctl::IC_IALLU;
        if self.self_check.contains(&page) {
            return;
        }
        let pages = self.code_pages.entry(page).or_default();
        let new_page = pages.is_empty();
        pages.push(key);
        if new_page {
            self.set_code_bit(page, true);
        }
        bus.watch_code_page(phys);
        if new_page {
            for r in 0..2 {
                for lin in self.writable[r].remove(&page).unwrap_or_default() {
                    if self.shadow[r].write_protect(self.mem.as_mut_slice(), lin, page) {
                        self.tlbi_page(lin);
                    }
                }
            }
        }
    }

    /// Puts `key` first in its bucket; the entry it displaces moves to the second slot.
    fn insert_lookup(&mut self, key: u64, va: u64) {
        let b = bucket(key);
        let m = self.mem.as_mut_slice();
        let first = u64::from_le_bytes(m[b..b + 8].try_into().unwrap());
        if first != key {
            let second = u64::from_le_bytes(m[b + 16..b + 24].try_into().unwrap());
            if second != key {
                m.copy_within(b..b + 16, b + 16);
            }
        }
        m[b..b + 8].copy_from_slice(&key.to_le_bytes());
        m[b + 8..b + 16].copy_from_slice(&va.to_le_bytes());
    }

    fn eip_at(&self, elr: u64) -> Option<u32> {
        let (&va, &key) = self.by_va.range(..=elr).next_back()?;
        let b = &self.blocks[&key];
        let word = ((elr - va) / 4) as u32;
        b.boundaries.iter().rev().find(|(w, _)| *w <= word).map(|(_, e)| *e)
    }

    /// Runs translated code until the CPU needs the interpreter for more than
    /// single instructions, or until the vCPU is kicked.
    pub fn run<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) -> hvf::Result<RunExit> {
        if cpu.interrupt_shadow {
            let e = self.step_interp(cpu, bus);
            if e != interp::Exit::Continue || cpu.halted || !supported(cpu) {
                return Ok(RunExit::Interp(e));
            }
        }
        for page in bus.take_written_code_pages() {
            self.invalidate_code_page(page << 12);
        }
        self.check_shadow(cpu, bus);
        if self.audit {
            self.audit(cpu, bus);
        }
        self.load_state(cpu);
        self.set_root(cpu);
        self.state().irq = bus.intr() as u32;
        let mut at_dispatch = true;
        loop {
            let expected = if self.verify && at_dispatch { self.expect_block(cpu, bus) } else { None };
            if self.verify && expected.is_none() {
                self.state().budget = 0;
            }
            if self.apic.is_some_and(|(page, _)| bus.apic_page() != Some(page)) {
                self.disarm_apic();
            }
            let (tpr, threshold) = (bus.apic_tpr(), bus.tpr_threshold());
            {
                let s = self.state();
                s.tpr = tpr.into();
                s.tpr_threshold = threshold.into();
            }
            let t0 = self.timing.then(std::time::Instant::now);
            let exit = self.vcpu.run()?;
            bus.set_apic_tpr(self.state().tpr as u8);
            self.stats.tpr_fast = self.state().tpr_fast;
            self.take_fill_log(cpu, bus);
            self.sync_mode(cpu, bus);
            let t1 = self.timing.then(std::time::Instant::now);
            if let (Some(a), Some(b)) = (t0, t1) {
                self.stats.vm_ns += (b - a).as_nanos() as u64;
            }
            at_dispatch = true;
            let kind = match exit {
                Exit::Exception { .. } if exit.ec() == Some(0x24) => 3,
                Exit::Exception { .. } if exit.ec() == Some(0x16) => match self.state().reason {
                    reason::MISS => 0,
                    reason::INTERP => 1,
                    reason::FAULT => 2,
                    _ => 4,
                },
                _ => 4,
            };
            let _timer = t1.map(|start| ExitTimer { start, slot: kind, stats: &mut self.stats as *mut Stats });
            if let Some(exp) = expected {
                let why = self.state().reason;
                let budget_spent = self.state().budget == 0;
                let left_early = self.state().eip != exp.cpu.eip;
                if exit.ec() == Some(0x16) && ((why == reason::PENDING && budget_spent) || why == reason::INTERP) && !left_early {
                    self.check_block(cpu, bus, exp);
                    if why == reason::PENDING {
                        continue;
                    }
                }
            }
            match exit {
                Exit::Canceled => {
                    if self.interp_profile.is_some() {
                        self.sample_pc()?;
                    }
                    // The vCPU may have stopped mid-block with guest state still in
                    // registers; let it run to the next dispatch, which saves it.
                    self.state().pending = 1;
                    at_dispatch = false;
                }
                Exit::Exception { ipa, .. } if exit.ec() == Some(0x24) && self.fb.as_ref().is_some_and(|f| f.contains(ipa)) => {
                    let f = self.fb.as_mut().unwrap();
                    self.vm.protect(f.ipa, f.len, mem::READ | mem::WRITE)?;
                    f.written = true;
                    at_dispatch = false;
                }
                Exit::Exception { syndrome, ipa, .. } if exit.ec() == Some(0x24) => {
                    let no_syndrome = syndrome & 1 << 24 == 0;
                    if let Some(done) = self.finish_in_interp(cpu, bus, no_syndrome)? {
                        if let Some(e) = done {
                            return Ok(RunExit::Interp(e));
                        }
                        continue;
                    }
                    self.mmio(bus, syndrome, ipa as u32)?;
                    self.state().irq = bus.intr() as u32;
                    at_dispatch = false;
                }
                Exit::Exception { .. } if exit.ec() == Some(0x16) => {
                    let why = self.state().reason;
                    match why {
                        reason::MISS => self.on_miss(cpu, bus),
                        reason::INTERP => {
                            self.stats.interp_exits += 1;
                            self.store_state(cpu);
                            self.drop_retry_block(cpu, bus);
                            if self.interp_profile.is_some() {
                                self.profile_interp(cpu, bus);
                            }
                            let e = self.step_interp(cpu, bus);
                            if e != interp::Exit::Continue || cpu.halted || !supported(cpu) {
                                return Ok(RunExit::Interp(e));
                            }
                            if let Some(e) = self.interp_run(cpu, bus) {
                                return Ok(RunExit::Interp(e));
                            }
                            self.check_shadow(cpu, bus);
                            self.load_state(cpu);
                            self.set_root(cpu);
                            self.state().irq = bus.intr() as u32;
                        }
                        reason::PENDING => {
                            self.store_state(cpu);
                            return Ok(RunExit::Kicked);
                        }
                        reason::STALE => {
                            self.stats.stale_blocks += 1;
                            let eip = self.state().eip;
                            if let Ok(phys) = interp::translate(cpu, bus, eip, false) {
                                self.remove_block(block_key(phys, eip, cpu.cpl() == 3));
                            }
                        }
                        reason::FAULT => {
                            if let Some(e) = self.on_fault(cpu, bus) {
                                return Ok(RunExit::Interp(e));
                            }
                            at_dispatch = self.state().resume == resume::DISPATCH;
                        }
                        _ => {
                            let s = self.state();
                            panic!("unexpected exception in the VM: ESR {:#x} FAR {:#x} ELR {:#x}", s.info[0], s.info[1], s.info[2]);
                        }
                    }
                }
                other => panic!("unexpected VM exit {other:?}"),
            }
        }
    }

    /// Runs the block at the current EIP in the interpreter on an overlay and
    /// arms the runtime to stop after that block.
    fn expect_block<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) -> Option<Expected> {
        let s = self.state();
        if s.pending != 0 || (s.irq != 0 && s.if_flag != 0) {
            return None;
        }
        let mut c = cpu.clone();
        self.store_state(&mut c);
        let phys = interp::translate(&mut c.clone(), bus, c.eip, false).ok()?;
        let key = block_key(phys, c.eip, cpu.cpl() == 3);
        let block = self.blocks.get(&key)?;
        if block.verified >= VERIFY_RUNS {
            return None;
        }
        let (n, va) = (block.insns, block.va);
        let eip = c.eip;
        let mut ov = Overlay { inner: bus, writes: HashMap::default() };
        for _ in 0..n {
            if interp::step(&mut c, &mut ov) != interp::Exit::Continue {
                return None;
            }
        }
        let writes = ov.writes;
        self.insert_lookup(key, va);
        self.state().budget = 2;
        Some(Expected { cpu: c, writes, eip, key })
    }

    fn check_block<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B, exp: Expected) {
        self.state().budget = 0;
        if let Some(b) = self.blocks.get_mut(&exp.key) {
            b.verified += 1;
            if b.verified < VERIFY_RUNS {
                self.clear_lookup(exp.key);
            }
        }
        let mut got = cpu.clone();
        self.store_state(&mut got);
        let mask = x86::cpu::flags::ARITH | x86::cpu::flags::IF;
        let mut diffs = Vec::new();
        if got.regs != exp.cpu.regs {
            diffs.push(format!("regs jit {:08x?}\n     interp {:08x?}", got.regs, exp.cpu.regs));
        }
        let de = x86::fpu::exc::DE as u32;
        if got.xmm != exp.cpu.xmm || (got.mxcsr | de) != (exp.cpu.mxcsr | de) {
            diffs.push(format!("sse jit {:x?} {:#x}\n     interp {:x?} {:#x}", got.xmm, got.mxcsr, exp.cpu.xmm, exp.cpu.mxcsr));
        }
        if got.fpu != exp.cpu.fpu {
            diffs.push(format!("fpu jit {:x?}\n     interp {:x?}", got.fpu, exp.cpu.fpu));
        }
        if got.eip != exp.cpu.eip {
            diffs.push(format!("eip jit {:#x} interp {:#x}", got.eip, exp.cpu.eip));
        }
        if got.eflags & mask != exp.cpu.eflags & mask {
            diffs.push(format!("flags jit {:#x} interp {:#x}", got.eflags & mask, exp.cpu.eflags & mask));
        }
        for (&a, &v) in &exp.writes {
            let real = bus.read8(a);
            if real != v {
                diffs.push(format!("mem[{a:#x}] jit {real:#04x} interp {v:#04x}"));
            }
        }
        if diffs.is_empty() {
            return;
        }
        let mut c = cpu.clone();
        let bytes = match interp::translate(&mut c, bus, exp.eip, false) {
            Ok(p) => (0..48).map(|i| format!("{:02x}", bus.read8(p + i))).collect::<Vec<_>>().join(" "),
            Err(_) => String::from("?"),
        };
        panic!("JIT block at {:#x} differs from the interpreter:\n{}\ncode: {}", exp.eip, diffs.join("\n"), bytes);
    }

    fn profile_interp<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) {
        let eip = cpu.eip;
        let Ok(phys) = interp::translate(cpu, bus, eip, false) else { return };
        let insn = x86::decode::decode::<()>(true, |i| Ok(bus.read8(phys.wrapping_add(i))));
        if let (Ok(i), Some(p)) = (insn, self.interp_profile.as_mut()) {
            let key = if matches!(i.op, 0xd8..=0xdf) && i.mem.is_none() {
                i.op << 8 | i.modrm as u16
            } else if i.has_modrm && matches!(i.op, 0x80..=0x83 | 0xc0 | 0xc1 | 0xd0..=0xd3 | 0xd8..=0xdf | 0xf6 | 0xf7 | 0xfe | 0xff) {
                i.op << 4 | i.reg() as u16 | 0x8000
            } else {
                i.op | if i.op32 { 0 } else { 0x4000 } | if i.rep != x86::decode::Rep::None { 0x2000 } else { 0 }
            };
            *p.entry(key).or_default() += 1;
            *self.interp_sites.entry((key, eip)).or_default() += 1;
        }
    }

    /// Takes in the shadow fills the runtime did inside the VM since the last exit.
    fn take_fill_log<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B) {
        let n = std::mem::take(&mut self.state().fill_count) as usize;
        for i in 0..n {
            let off = layout::FILL_LOG + i * 32;
            let word = |j: usize| u32::from_le_bytes(self.mem.as_slice()[off + 4 * j..off + 4 * j + 4].try_into().unwrap());
            let (leaf, lin, phys_flags, pde, pte) = (word(0) as usize, word(1), word(2), word(3), word(4));
            let phys = phys_flags & !0xfff;
            let r = (phys_flags & state::fill::USER != 0) as usize;
            if phys_flags & state::fill::GLOBAL == 0 {
                self.nonglobal[r].push(leaf);
            }
            if phys_flags & state::fill::WRITABLE != 0 {
                self.writable[r].entry(phys).or_default().push(lin & !0xfff);
            }
            self.stats.vm_fills += 1;
            self.pt_watch.watch_logged(cpu, bus, lin, pde, pte, phys_flags & state::fill::BIG != 0);
        }
    }

    fn set_code_bit(&mut self, page: u32, on: bool) {
        let i = layout::CODE_BITMAP + (page >> 15) as usize;
        let bit = 1u8 << ((page >> 12) & 7);
        let b = &mut self.mem.as_mut_slice()[i];
        if on {
            *b |= bit;
        } else {
            *b &= !bit;
        }
    }

    /// Interprets on while the blocks ahead would soon exit to the interpreter
    /// again, since a VM round trip costs more than a dozen interpreted steps.
    fn interp_run<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) -> Option<interp::Exit> {
        let mut since_interp = 0;
        for _ in 0..INTERP_RUN_MAX {
            if since_interp >= INTERP_RUN_TAIL || (cpu.flag(x86::cpu::flags::IF) && bus.intr()) {
                break;
            }
            since_interp = match self.ahead(cpu, bus) {
                Ahead::Exits => 0,
                Ahead::Runs => since_interp + 1,
                Ahead::Stop => break,
            };
            let e = self.step_interp(cpu, bus);
            if e != interp::Exit::Continue || cpu.halted || !supported(cpu) {
                return Some(e);
            }
        }
        None
    }

    /// Drops the block at EIP if it was translated while its first instruction
    /// could not be read, so the next visit translates it properly.
    fn drop_retry_block<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) {
        if self.retry.is_empty() {
            return;
        }
        let Ok(phys) = interp::translate(&mut cpu.clone(), bus, cpu.eip, false) else { return };
        let key = block_key(phys, cpu.eip, cpu.cpl() == 3);
        if self.retry.contains(&key) {
            self.remove_block(key);
        }
    }

    /// What the code at EIP would do in the translator.
    fn ahead<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) -> Ahead {
        let Ok(phys) = interp::translate(cpu, bus, cpu.eip, false) else { return Ahead::Stop };
        let user = cpu.cpl() == 3;
        let key = block_key(phys, cpu.eip, user);
        let insn = match self.ahead_cache.get(&key) {
            Some(&a) => a,
            None => {
                let Ok(insn) = x86::decode::decode::<()>(true, |i| Ok(bus.read8(phys.wrapping_add(i)))) else { return Ahead::Stop };
                let a = if insn.rep != x86::decode::Rep::None && matches!(insn.op, 0xa4..=0xa7 | 0xaa..=0xaf) {
                    Ahead::Stop
                } else if translate::translatable(&insn, cpu.eip, user) {
                    Ahead::Runs
                } else {
                    Ahead::Exits
                };
                if self.ahead_cache.len() >= 1 << 16 {
                    self.ahead_cache.clear();
                }
                self.ahead_cache.insert(key, a);
                a
            }
        };
        let block_exits = self.blocks.get(&key).and_then(|b| b.interp_after).is_some_and(|n| n <= INTERP_RUN_SHORT);
        if insn == Ahead::Runs && block_exits { Ahead::Exits } else { insn }
    }

    /// Steps the interpreter, and once more while an STI, MOV SS or POP SS
    /// holds off interrupts, since the dispatcher does not know about that.
    fn step_interp<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) -> interp::Exit {
        let mut e = interp::step(cpu, bus);
        while e == interp::Exit::Continue && cpu.interrupt_shadow && !cpu.halted {
            e = interp::step(cpu, bus);
        }
        for page in bus.take_written_code_pages() {
            self.invalidate_code_page(page << 12);
        }
        e
    }

    fn on_miss<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) {
        self.stats.miss_exits += 1;
        if self.audit && self.root != (cpu.cpl() == 3) as usize {
            let (root, eip) = (self.root, self.state().eip);
            eprintln!("AUDIT miss with root {root} at cpl {} eip {eip:#x}", cpu.cpl());
        }
        let eip = self.state().eip;
        match interp::translate_global(cpu, bus, eip, false) {
            Ok((phys, global)) => {
                self.map_shadow(bus, cpu, eip, phys, false, global);
                let user = cpu.cpl() == 3;
                let key = block_key(phys, eip, user);
                match self.blocks.get(&key) {
                    Some(b) => {
                        self.stats.lookup_misses += 1;
                        if !self.verify || b.verified >= VERIFY_RUNS {
                            let va = b.va;
                            self.insert_lookup(key, va);
                        }
                    }
                    None => self.translate_block(cpu, bus, eip, phys, user),
                }
            }
            Err(e) => {
                self.stats.page_faults += 1;
                self.store_state(cpu);
                let _ = interp::deliver_fault(cpu, bus, e);
                self.load_state(cpu);
                self.set_root(cpu);
            }
        }
    }

    /// Handles a fault in translated code. Some(exit) means the CPU must leave
    /// translated code.
    fn on_fault<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) -> Option<interp::Exit> {
        let [esr, far, elr, _] = self.state().info;
        if self.audit && self.root != (cpu.cpl() == 3) as usize {
            eprintln!("AUDIT fault with root {} at cpl {} far {far:#x} elr {elr:#x}", self.root, cpu.cpl());
        }
        let ec = esr >> 26;
        assert!(ec == 0x25, "unhandled fault in translated code: ESR {esr:#x} FAR {far:#x} ELR {elr:#x}");
        assert!(far >> 32 == 0, "translated code faulted outside the x86 address space: FAR {far:#x} ELR {elr:#x} ESR {esr:#x}");
        let write = esr & (1 << 6) != 0;
        let mut lin = far as u32;
        if self.shadow[self.root].maps(self.mem.as_slice(), lin, write) {
            // FAR can name the start of an access that faulted on the following page.
            lin = (lin | 0xfff).wrapping_add(1);
        }
        match interp::translate_global(cpu, bus, lin, write) {
            Ok((x86_phys, global)) => {
                let above_ram = x86_phys >= self.state().ram_size;
                let gart = if above_ram { bus.aperture_page(x86_phys) } else { None };
                let phys = gart.map_or(x86_phys, |p| p | (x86_phys & 0xfff));
                let page = phys & !0xfff;
                if bus.apic_page() == Some(page) && cpu.cpl() == 0 && lin == far as u32 {
                    let at = elr.wrapping_sub(layout::va(0)) as usize;
                    let insn = u32::from_le_bytes(self.mem.as_slice()[at..at + 4].try_into().unwrap());
                    if let Some(access) = decode_access(insn) {
                        self.apic_access(cpu, bus, access, phys, global);
                        return None;
                    }
                    self.disarm_apic();
                }
                if write && self.code_pages.contains_key(&page) {
                    // The store may rewrite the running block, so finish it in the
                    // interpreter and continue from fresh translations.
                    let eip = self.eip_at(elr).expect("fault outside any translated block");
                    self.deopt_at_fault(elr);
                    self.invalidate_code_page(page);
                    let leaf = self.map_shadow(bus, cpu, lin, phys, write, global);
                    self.note_aperture_leaf(leaf, above_ram, gart.is_some());
                    self.tlbi_page(lin);
                    self.state().eip = eip;
                    self.store_state(cpu);
                    let e = self.step_interp(cpu, bus);
                    if e != interp::Exit::Continue || cpu.halted || !supported(cpu) {
                        return Some(e);
                    }
                    self.check_shadow(cpu, bus);
                    self.load_state(cpu);
                    self.set_root(cpu);
                    self.state().irq = bus.intr() as u32;
                    self.state().resume = resume::DISPATCH;
                    return None;
                }
                let leaf = self.map_shadow(bus, cpu, lin, phys, write, global);
                self.note_aperture_leaf(leaf, above_ram, gart.is_some());
                self.tlbi_page(lin);
                self.state().resume = resume::RETRY;
            }
            Err(e) => {
                self.stats.page_faults += 1;
                let eip = self.eip_at(elr).expect("fault outside any translated block");
                self.deopt_at_fault(elr);
                self.state().eip = eip;
                self.store_state(cpu);
                let _ = interp::deliver_fault(cpu, bus, e);
                self.load_state(cpu);
                self.set_root(cpu);
                self.state().resume = resume::DISPATCH;
            }
        }
        None
    }

    fn note_aperture_leaf(&mut self, leaf: usize, above_ram: bool, direct: bool) {
        if above_ram {
            self.aperture_leaves[self.root].insert(leaf);
        }
        if direct {
            self.stats.aperture_fills += 1;
        }
    }

    /// Performs a faulting access to the APIC page from the syndrome and arms
    /// the in-VM TPR path for its linear page, which stays unmapped.
    fn apic_access<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B, access: Access, phys: u32, global: bool) {
        let [_, far, elr, _] = self.state().info;
        let lin = far as u32;
        for t in &mut self.shadow {
            t.unmap(self.mem.as_mut_slice(), lin);
        }
        self.tlbi_page(lin);
        if cpu.cr0 & x86::cpu::cr0::PG != 0 {
            self.pt_watch.watch(cpu, bus, lin);
        }
        self.apic = Some((phys & !0xfff, global));
        self.state().apic_va = u64::from(lin & !0xfff) | 1;

        self.stats.mmio_exits += 1;
        let Access { load, size, sign, rt: srt } = access;
        let reg_value = |j: &mut Self| match srt {
            0..18 => j.state().scratch[srt],
            31 => 0,
            _ => j.vcpu.reg(reg::X0 + srt as u32).expect("vCPU register read"),
        };
        if !load {
            let v = reg_value(self);
            match size {
                1 => bus.write8(phys, v as u8),
                2 => bus.write16(phys, v as u16),
                _ => bus.write32(phys, v as u32),
            }
        } else {
            let mut v = match size {
                1 => bus.read8(phys) as u64,
                2 => bus.read16(phys) as u64,
                _ => bus.read32(phys) as u64,
            };
            if sign {
                let shift = 64 - 8 * size;
                v = (((v << shift) as i64) >> shift) as u64;
            }
            match srt {
                0..18 => self.state().scratch[srt] = v,
                31 => {}
                _ => self.vcpu.set_reg(reg::X0 + srt as u32, v).expect("vCPU register write"),
            }
        }
        self.vcpu.set_sys_reg(sysreg::ELR_EL1, elr + 4).expect("ELR write");
        self.state().irq = bus.intr() as u32;
        self.state().resume = resume::RETRY;
    }

    /// Interprets a REP string op that hit MMIO, or with `any` any instruction
    /// (SIMD accesses carry no syndrome). Some(Some(exit)): leave translated code.
    fn finish_in_interp<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B, any: bool) -> hvf::Result<Option<Option<interp::Exit>>> {
        let pc = self.vcpu.reg(reg::PC)?;
        let Some(eip) = self.eip_at(pc) else { return Ok(None) };
        let Ok(phys) = interp::translate(&mut cpu.clone(), bus, eip, false) else { return Ok(None) };
        let insn = x86::decode::decode::<()>(true, |i| Ok(bus.read8(phys.wrapping_add(i))));
        let Ok(insn) = insn else { return Ok(None) };
        let string = insn.rep != x86::decode::Rep::None && matches!(insn.op, 0xa4..=0xa7 | 0xaa..=0xaf);
        if !string && !any {
            return Ok(None);
        }
        let mut regs = [0u32; 8];
        for (i, r) in regs.iter_mut().enumerate() {
            *r = self.vcpu.reg(reg::X0 + 19 + i as u32)? as u32;
        }
        let nzcv = (self.vcpu.reg(reg::CPSR)? & 0xf000_0000) as u32;
        let pf = self.vcpu.reg(reg::X0 + 27)? as u32;
        let af = self.vcpu.reg(reg::X0 + 18)? as u32;
        let icount = self.vcpu.reg(reg::X0 + 17)?;
        {
            let s = self.state();
            s.regs = regs;
            s.eip = eip;
            s.nzcv = nzcv;
            s.pf_result = pf;
            s.af_aux = af;
            s.icount = icount;
        }
        let xregs = [self.vcpu.reg(reg::X0 + 2)?, self.vcpu.reg(reg::X0 + 3)?, self.vcpu.reg(reg::X0 + 4)?];
        self.x87_deopt(pc, xregs)?;
        self.store_state(cpu);
        self.stats.interp_exits += 1;
        let e = self.step_interp(cpu, bus);
        if e != interp::Exit::Continue || cpu.halted || !supported(cpu) {
            return Ok(Some(Some(e)));
        }
        self.check_shadow(cpu, bus);
        self.load_state(cpu);
        self.set_root(cpu);
        self.state().irq = bus.intr() as u32;
        self.vcpu.set_reg(reg::CPSR, 0x3c5)?;
        self.vcpu.set_reg(reg::PC, layout::va(self.entries.resume))?;
        Ok(Some(None))
    }

    fn mmio<B: Bus>(&mut self, bus: &mut B, syndrome: u64, addr: u32) -> hvf::Result<()> {
        self.stats.mmio_exits += 1;
        let iss = syndrome & 0x1ff_ffff;
        assert!(iss & (1 << 24) != 0, "MMIO access without a valid syndrome at {addr:#x}");
        let size = 1u32 << ((iss >> 22) & 3);
        let sign = iss & (1 << 21) != 0;
        let srt = ((iss >> 16) & 31) as u32;
        let write = iss & (1 << 6) != 0;
        if write {
            let v = if srt == 31 { 0 } else { self.vcpu.reg(reg::X0 + srt)? };
            match size {
                1 => bus.write8(addr, v as u8),
                2 => bus.write16(addr, v as u16),
                4 => bus.write32(addr, v as u32),
                _ => {
                    bus.write32(addr, v as u32);
                    bus.write32(addr.wrapping_add(4), (v >> 32) as u32);
                }
            }
        } else {
            let mut v = match size {
                1 => bus.read8(addr) as u64,
                2 => bus.read16(addr) as u64,
                4 => bus.read32(addr) as u64,
                _ => bus.read32(addr) as u64 | (bus.read32(addr.wrapping_add(4)) as u64) << 32,
            };
            if sign && size < 8 {
                let shift = 64 - 8 * size;
                v = (((v << shift) as i64) >> shift) as u64;
            }
            if srt != 31 {
                self.vcpu.set_reg(reg::X0 + srt, v)?;
            }
        }
        let pc = self.vcpu.reg(reg::PC)?;
        self.vcpu.set_reg(reg::PC, pc + 4)
    }
}

/// A load or store decoded from the ARM instruction that faulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Access {
    load: bool,
    /// Bytes: 1, 2 or 4.
    size: u32,
    sign: bool,
    rt: usize,
}

/// Decodes the single-register, non-writeback integer loads and stores of up
/// to 32 bits that translated code makes to guest memory.
fn decode_access(insn: u32) -> Option<Access> {
    let size = 1u32 << (insn >> 30);
    let opc = (insn >> 22) & 3;
    let unsigned_imm = insn & 0x3f00_0000 == 0x3900_0000;
    let unscaled_or_reg = insn & 0x3f00_0000 == 0x3800_0000 && matches!(insn >> 10 & 3 | (insn >> 19 & 4), 0b000 | 0b110);
    if size > 4 || !(unsigned_imm || unscaled_or_reg) {
        return None;
    }
    Some(Access { load: opc != 0, size, sign: opc >= 2, rt: (insn & 31) as usize })
}

#[cfg(test)]
mod tests;

//! Runs x86 code by translating it to ARM64 and executing it inside a
//! Hypervisor.framework VM. See docs/architecture.md, "VM runtime layout".

pub mod layout;
mod runtime;
pub mod shadow;
pub mod state;
mod translate;

use std::collections::{BTreeMap, HashMap};

use hvf::{Exit, GuestMemory, Vcpu, VcpuKicker, Vm, mem, reg, sysreg};
use x86::bus::Bus;
use x86::cpu::{CS, Cpu, DS, ES, FS, FlushKind, GS, SS};
use x86::interp;

use runtime::Entries;
use shadow::Shadow;
use state::{State, ctl, reason, resume};

/// VGA window plus the PAM-controlled ROM/shadow area: left unmapped so every
/// access goes through the board, which knows the current PAM settings.
const LEGACY_HOLE: std::ops::Range<u32> = 0xa_0000..0x10_0000;

struct BlockInfo {
    va: u64,
    boundaries: Vec<(u32, u32)>,
    insns: usize,
    /// Executions checked against the interpreter in verify mode.
    verified: u32,
}

/// Invalidations after which a page switches to self-checking blocks.
const SELF_CHECK_AFTER: u32 = 4;

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
}

/// Paging-structure pages watched before a full flush is cheaper than comparing.
const PT_WATCH_LIMIT: usize = 256;

impl PtWatch {
    fn cr_key(cpu: &Cpu) -> (u32, u32, u32) {
        use x86::cpu::{cr0, cr4};
        (cpu.cr3 & !0xfff, cpu.cr0 & (cr0::PG | cr0::WP | cr0::PE), cpu.cr4 & (cr4::PSE | cr4::PGE | cr4::PAE))
    }

    fn clear(&mut self) {
        self.key = None;
        self.pages.clear();
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

    /// Takes in the Accessed/Dirty bits our walk just set. False means the
    /// watch is full or for another CR3, and the caller must flush.
    fn watch<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B, lin: u32) -> bool {
        let key = Self::cr_key(cpu);
        match self.key {
            None => self.key = Some(key),
            Some(k) if k != key => return false,
            _ => {}
        }
        let pd = cpu.cr3 & !0xfff;
        if !self.snapshot(bus, pd) {
            return false;
        }
        let pde = bus.read32(pd + (lin >> 22) * 4);
        if let Some(copy) = self.pages.get_mut(&pd) {
            copy[(lin >> 22) as usize] = pde;
        }
        let big = pde & 0x80 != 0 && cpu.cr4 & x86::cpu::cr4::PSE != 0;
        if pde & 1 != 0 && !big {
            let pt = pde & !0xfff;
            if !self.snapshot(bus, pt) {
                return false;
            }
            let i = ((lin >> 12) & 0x3ff) as usize;
            let pte = bus.read32(pt + i as u32 * 4);
            if let Some(copy) = self.pages.get_mut(&pt) {
                copy[i] = pte;
            }
        }
        true
    }

    fn still_valid<B: Bus>(&self, cpu: &Cpu, bus: &mut B) -> bool {
        if self.key != Some(Self::cr_key(cpu)) {
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
    pub flushes: u64,
    pub flush_tlb_gen: u64,
    pub nonglobal_flushes: u64,
    pub flushes_avoided: u64,
    pub flush_single_all: u64,
    pub flush_multi: u64,
    pub flush_pool: u64,
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
    code_next: usize,
    blocks: HashMap<u64, BlockInfo>,
    by_va: BTreeMap<u64, u64>,
    code_pages: HashMap<u32, Vec<u64>>,
    /// Times each physical page lost its translations to writes.
    smc_counts: HashMap<u32, u32>,
    /// Pages written too often to write-protect: their blocks check their own
    /// code bytes on entry instead.
    self_check: std::collections::HashSet<u32>,
    pt_watch: PtWatch,
    /// Chain sites (code cache offsets) waiting for a block at a physical address.
    chain_wanted: HashMap<u64, Vec<usize>>,
    /// Chain sites patched to jump straight into the block at a physical address.
    chain_in: HashMap<u64, Vec<usize>>,
    /// Shadow leaf entries for non-global x86 mappings, cleared on CR3 loads.
    nonglobal: [Vec<usize>; 2],
    /// Linear pages mapped writable in the shadow tables, by physical page.
    writable: [HashMap<u32, Vec<u32>>; 2],
    pub stats: Stats,
    /// Runs each block against the interpreter and stops at the first difference.
    pub verify: bool,
    /// Measures time spent in the VM and per exit kind (SLOPPER_JIT_PROFILE).
    timing: bool,
    audit: bool,
    audit_seen: std::collections::HashSet<(usize, u32, u32)>,
    /// Interpreter exits by opcode, when profiling is on.
    pub interp_profile: Option<HashMap<u16, u64>>,
    /// Declared last so it is dropped after the vCPU.
    _vm: Vm,
}

fn hash(key: u64) -> usize {
    ((key ^ (key >> 13)) as usize) & (layout::LOOKUP_ENTRIES - 1)
}

/// Blocks are looked up by physical address plus whether they run at CPL 3.
fn block_key(phys: u32, user: bool) -> u64 {
    phys as u64 | (user as u64) << 32
}

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
            code_next: layout::CODE_CACHE,
            blocks: HashMap::new(),
            by_va: BTreeMap::new(),
            code_pages: HashMap::new(),
            smc_counts: HashMap::new(),
            self_check: Default::default(),
            writable: [HashMap::new(), HashMap::new()],
            nonglobal: [Vec::new(), Vec::new()],
            pt_watch: PtWatch::default(),
            chain_wanted: HashMap::new(),
            chain_in: HashMap::new(),
            stats: Stats::default(),
            interp_profile: std::env::var_os("SLOPPER_JIT_PROFILE").map(|_| HashMap::new()),
            verify: std::env::var_os("SLOPPER_JIT_VERIFY").is_some(),
            timing: std::env::var_os("SLOPPER_JIT_PROFILE").is_some(),
            audit: std::env::var_os("SLOPPER_JIT_AUDIT").is_some(),
            audit_seen: std::collections::HashSet::new(),
            _vm: vm,
        };
        for t in &mut jit.shadow {
            t.flush(jit.mem.as_mut_slice());
        }
        let v = &jit.vcpu;
        v.set_sys_reg(sysreg::MAIR_EL1, 0xff)?;
        v.set_sys_reg(sysreg::TCR_EL1, tcr)?;
        v.set_sys_reg(sysreg::TTBR0_EL1, jit.shadow[0].root_ipa() | 1 << 48)?;
        v.set_sys_reg(sysreg::TTBR1_EL1, layout::ipa(layout::TTBR1_L1))?;
        v.set_sys_reg(sysreg::VBAR_EL1, layout::va(layout::VECTORS))?;
        v.set_sys_reg(sysreg::SP_EL1, layout::va(layout::STACK_TOP))?;
        v.set_sys_reg(sysreg::CPACR_EL1, 3 << 20)?;
        v.set_sys_reg(sysreg::SCTLR_EL1, 0x30d0_0800 | 1 | 1 << 2 | 1 << 12)?;
        v.set_reg(reg::CPSR, 0x3c5)?;
        v.set_reg(reg::PC, layout::va(entries.resume))?;
        v.set_reg(reg::X0 + 28, layout::va(layout::STATE))?;
        jit.state().lookup = layout::va(layout::LOOKUP);
        Ok(jit)
    }

    /// Compares every shadow mapping with the x86 page tables and reports
    /// mappings that disagree (SLOPPER_JIT_AUDIT).
    fn audit<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B) {
        if cpu.cr0 & x86::cpu::cr0::PG == 0 {
            return;
        }
        let pse = cpu.cr4 & x86::cpu::cr4::PSE != 0;
        let mut reported = 0;
        for r in 0..2 {
            for (lin, phys, writable, off) in self.shadow[r].leaves(self.mem.as_slice()) {
                let pde = bus.read32((cpu.cr3 & !0xfff) + (lin >> 22) * 4);
                let want = if pde & 1 == 0 {
                    None
                } else if pde & 0x80 != 0 && pse {
                    Some(((pde & 0xffc0_0000) | (lin & 0x3f_f000), pde))
                } else {
                    let pte = bus.read32((pde & !0xfff) + ((lin >> 12) & 0x3ff) * 4);
                    (pte & 1 != 0).then_some((pte & !0xfff, pde & pte))
                };
                let bad = match want {
                    None => true,
                    Some((p, perms)) => p != phys || (writable && perms & 2 == 0 && r == 1),
                };
                if bad && reported < 10 && self.audit_seen.insert((r, lin, phys)) {
                    reported += 1;
                    eprintln!(
                        "AUDIT root {r} lin {lin:#010x} shadow {phys:#x}{} x86 {want:x?} nonglobal_tracked {} gen {}",
                        if writable { " rw" } else { " ro" },
                        self.nonglobal[r].contains(&off),
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

    /// Turns exit timing and the interpreter-exit histogram on or off.
    pub fn set_profiling(&mut self, on: bool) {
        self.timing = on;
        self.interp_profile = on.then(HashMap::new);
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
        format!(
            "ms in vm {:.0} miss {:.0} interp {:.0} fault {:.0} mmio {:.0}\ninterpreter exits by opcode: {}",
            ms(s.vm_ns),
            ms(s.miss_ns),
            ms(s.interp_ns),
            ms(s.fault_ns),
            ms(s.mmio_ns),
            top.join(" ")
        )
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
        let ts_em = x86::cpu::cr0::TS | x86::cpu::cr0::EM;
        s.sse_ok = (cpu.cr0 & ts_em == 0 && cpu.cr4 & x86::cpu::cr4::OSFXSR != 0) as u32;
        s.xmm = cpu.xmm;
    }

    fn store_state(&mut self, cpu: &mut Cpu) {
        let s = self.state();
        cpu.regs = s.regs;
        cpu.eip = s.eip;
        let (if_flag, df) = (s.if_flag != 0, s.df != 0);
        cpu.eflags = s.merge_eflags(cpu.eflags);
        cpu.set_flag(x86::cpu::flags::IF, if_flag);
        cpu.set_flag(x86::cpu::flags::DF, df);
        cpu.xmm = s.xmm;
    }

    /// Points TTBR0 at the shadow tree for the current CPL, each with its own ASID.
    fn set_root(&mut self, cpu: &Cpu) {
        let r = (cpu.cpl() == 3) as usize;
        if r != self.root {
            self.root = r;
            let ttbr = self.shadow[r].root_ipa() | (r as u64 + 1) << 48;
            self.vcpu.set_sys_reg(sysreg::TTBR0_EL1, ttbr).expect("TTBR0 write");
        }
        self.state().user = r as u32;
    }

    fn flush_shadow(&mut self) {
        self.pt_watch.clear();
        for t in &mut self.shadow {
            t.flush(self.mem.as_mut_slice());
        }
        for r in 0..2 {
            self.writable[r].clear();
            self.nonglobal[r].clear();
        }
        self.state().ctl |= ctl::TLBI_ALL;
        self.stats.flushes += 1;
    }

    /// Follows the interpreter's TLB: CR3/CR0/CR4 writes and INVLPG flush it.
    fn check_shadow<B: Bus>(&mut self, cpu: &Cpu, bus: &mut B) {
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
                        for t in &mut self.shadow {
                            t.unmap(self.mem.as_mut_slice(), lin);
                        }
                        self.tlbi_page(lin);
                    }
                    _ => {
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

    fn tlbi_page(&mut self, lin: u32) {
        let s = self.state();
        if s.ctl & ctl::TLBI_PAGE != 0 && s.tlbi_page != (lin >> 12) as u64 {
            s.ctl |= ctl::TLBI_ALL;
        }
        s.tlbi_page = (lin >> 12) as u64;
        s.ctl |= ctl::TLBI_PAGE;
    }

    fn map_shadow<B: Bus>(&mut self, bus: &mut B, cpu: &Cpu, lin: u32, phys: u32, writable: bool, global: bool) {
        if cpu.cr0 & x86::cpu::cr0::PG != 0 && !self.pt_watch.watch(cpu, bus, lin) {
            self.flush_shadow();
            self.pt_watch.watch(cpu, bus, lin);
        }
        let r = self.root;
        let leaf = match self.shadow[r].map(self.mem.as_mut_slice(), lin, phys, writable) {
            Some(off) => off,
            None => {
                self.stats.flush_pool += 1;
                self.flush_shadow();
                self.shadow[r].map(self.mem.as_mut_slice(), lin, phys, writable).expect("shadow pool smaller than one mapping")
            }
        };
        if writable {
            self.writable[r].entry(phys & !0xfff).or_default().push(lin & !0xfff);
        }
        if !global {
            self.nonglobal[r].push(leaf);
        }
        self.stats.shadow_fills += 1;
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
        for site in self.chain_in.remove(&key).unwrap_or_default() {
            self.patch_chain(site, None);
            self.chain_wanted.entry(key).or_default().push(site);
        }
        if let Some(b) = self.blocks.remove(&key) {
            self.by_va.remove(&b.va);
        }
        let off = layout::LOOKUP + hash(key) * 16;
        let slot = &mut self.mem.as_mut_slice()[off..off + 16];
        if u64::from_le_bytes(slot[..8].try_into().unwrap()) == key {
            slot.fill(0xff);
        }
    }

    fn reset_code_cache(&mut self) {
        self.chain_wanted.clear();
        self.chain_in.clear();
        self.blocks.clear();
        self.by_va.clear();
        self.code_pages.clear();
        self.code_next = layout::CODE_CACHE;
        self.mem.as_mut_slice()[layout::LOOKUP..layout::LOOKUP + layout::LOOKUP_ENTRIES * 16].fill(0xff);
        self.flush_shadow();
    }

    fn translate_block<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B, eip: u32, phys: u32, user: bool) {
        let key = block_key(phys, user);
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
            let target = block_key(page | (target & 0xfff), user);
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
        let verified = if compiled.has_rep { VERIFY_RUNS } else { 0 };
        if self.verify && compiled.has_rep {
            self.insert_lookup(key, block_va);
        }
        self.blocks.insert(key, BlockInfo { va: block_va, boundaries: compiled.boundaries, insns: compiled.insns, verified });
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

    fn insert_lookup(&mut self, key: u64, va: u64) {
        let off = layout::LOOKUP + hash(key) * 16;
        let slot = &mut self.mem.as_mut_slice()[off..off + 16];
        slot[..8].copy_from_slice(&key.to_le_bytes());
        slot[8..].copy_from_slice(&va.to_le_bytes());
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
            let t0 = self.timing.then(std::time::Instant::now);
            let exit = self.vcpu.run()?;
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
                let left_early = why == reason::INTERP && self.state().eip != exp.cpu.eip;
                if exit.ec() == Some(0x16) && ((why == reason::PENDING && budget_spent) || why == reason::INTERP) && !left_early {
                    self.check_block(cpu, bus, exp);
                    if why == reason::PENDING {
                        continue;
                    }
                }
            }
            match exit {
                Exit::Canceled => {
                    // The vCPU may have stopped mid-block with guest state still in
                    // registers; let it run to the next dispatch, which saves it.
                    self.state().pending = 1;
                    at_dispatch = false;
                }
                Exit::Exception { syndrome, ipa, .. } if exit.ec() == Some(0x24) => {
                    if let Some(done) = self.finish_string_in_interp(cpu, bus)? {
                        if let Some(e) = done {
                            return Ok(RunExit::Interp(e));
                        }
                        continue;
                    }
                    self.mmio(bus, syndrome, ipa as u32)?;
                    at_dispatch = false;
                }
                Exit::Exception { .. } if exit.ec() == Some(0x16) => {
                    let why = self.state().reason;
                    match why {
                        reason::MISS => self.on_miss(cpu, bus),
                        reason::INTERP => {
                            self.stats.interp_exits += 1;
                            self.store_state(cpu);
                            if self.interp_profile.is_some() {
                                self.profile_interp(cpu, bus);
                            }
                            let e = self.step_interp(cpu, bus);
                            if e != interp::Exit::Continue || cpu.halted || !supported(cpu) {
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
                                self.remove_block(block_key(phys, cpu.cpl() == 3));
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
        let key = block_key(phys, cpu.cpl() == 3);
        let block = self.blocks.get(&key)?;
        if block.verified >= VERIFY_RUNS {
            return None;
        }
        let (n, va) = (block.insns, block.va);
        let eip = c.eip;
        let mut ov = Overlay { inner: bus, writes: HashMap::new() };
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
                let off = layout::LOOKUP + hash(exp.key) * 16;
                self.mem.as_mut_slice()[off..off + 16].fill(0xff);
            }
        }
        let mut got = cpu.clone();
        self.store_state(&mut got);
        let mask = x86::cpu::flags::ARITH | x86::cpu::flags::IF;
        let mut diffs = Vec::new();
        if got.regs != exp.cpu.regs {
            diffs.push(format!("regs jit {:08x?}\n     interp {:08x?}", got.regs, exp.cpu.regs));
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
            let key = if i.has_modrm && matches!(i.op, 0x80..=0x83 | 0xc0 | 0xc1 | 0xd0..=0xd3 | 0xf6 | 0xf7 | 0xfe | 0xff) {
                i.op << 4 | i.reg() as u16 | 0x8000
            } else {
                i.op | if i.op32 { 0 } else { 0x4000 } | if i.rep != x86::decode::Rep::None { 0x2000 } else { 0 }
            };
            *p.entry(key).or_default() += 1;
        }
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
                let key = block_key(phys, user);
                match self.blocks.get(&key) {
                    Some(b) => {
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
        assert!(far >> 32 == 0, "translated code faulted outside the x86 address space: FAR {far:#x}");
        let lin = far as u32;
        let write = esr & (1 << 6) != 0;
        match interp::translate_global(cpu, bus, lin, write) {
            Ok((phys, global)) => {
                let page = phys & !0xfff;
                if write && self.code_pages.contains_key(&page) {
                    // The store may rewrite the running block, so finish it in the
                    // interpreter and continue from fresh translations.
                    let eip = self.eip_at(elr).expect("fault outside any translated block");
                    self.invalidate_code_page(page);
                    self.map_shadow(bus, cpu, lin, phys, write, global);
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
                self.map_shadow(bus, cpu, lin, phys, write, global);
                self.tlbi_page(lin);
                self.state().resume = resume::RETRY;
            }
            Err(e) => {
                self.stats.page_faults += 1;
                let eip = self.eip_at(elr).expect("fault outside any translated block");
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

    /// Hands a REP string op that hit MMIO to the interpreter, avoiding one exit
    /// per element. Some(Some(exit)) means the CPU must leave translated code.
    fn finish_string_in_interp<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) -> hvf::Result<Option<Option<interp::Exit>>> {
        let pc = self.vcpu.reg(reg::PC)?;
        let Some(eip) = self.eip_at(pc) else { return Ok(None) };
        let Ok(phys) = interp::translate(&mut cpu.clone(), bus, eip, false) else { return Ok(None) };
        let insn = x86::decode::decode::<()>(true, |i| Ok(bus.read8(phys.wrapping_add(i))));
        let Ok(insn) = insn else { return Ok(None) };
        if insn.rep == x86::decode::Rep::None || !matches!(insn.op, 0xa4..=0xa7 | 0xaa..=0xaf) {
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
                _ => bus.write32(addr, v as u32),
            }
        } else {
            let mut v = match size {
                1 => bus.read8(addr) as u64,
                2 => bus.read16(addr) as u64,
                _ => bus.read32(addr) as u64,
            };
            if sign {
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

#[cfg(test)]
mod tests;

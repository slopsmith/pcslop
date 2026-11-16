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
use x86::cpu::{CS, Cpu, DS, ES, FS, GS, SS};
use x86::interp;

use runtime::Entries;
use shadow::Shadow;
use state::{State, ctl, reason, resume};

const VGA_HOLE: std::ops::Range<u32> = 0xa_0000..0xc_0000;

struct BlockInfo {
    va: u64,
    boundaries: Vec<(u32, u32)>,
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
    pub flushes: u64,
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
    shadow: Shadow,
    shadow_key: Option<(u32, u32, u32)>,
    code_next: usize,
    blocks: HashMap<u32, BlockInfo>,
    by_va: BTreeMap<u64, u32>,
    code_pages: HashMap<u32, Vec<u32>>,
    pub stats: Stats,
    /// Declared last so it is dropped after the vCPU.
    _vm: Vm,
}

fn hash(pa: u32) -> usize {
    let pa = pa as u64;
    ((pa ^ (pa >> 13)) as usize) & (layout::LOOKUP_ENTRIES - 1)
}

/// True for CPU states whose code the translator can run: 32-bit protected
/// mode at CPL 0 with flat CS, SS, DS and ES.
pub fn supported(cpu: &Cpu) -> bool {
    if !cpu.protected_mode() || cpu.v86_mode() || cpu.cpl() != 0 || !cpu.segs[CS].big() {
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
        let low = VGA_HOLE.start as usize;
        let high = VGA_HOLE.end as usize;
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
            _vm: vm,
            vcpu,
            mem: rt,
            entries,
            shadow: Shadow::default(),
            shadow_key: None,
            code_next: layout::CODE_CACHE,
            blocks: HashMap::new(),
            by_va: BTreeMap::new(),
            code_pages: HashMap::new(),
            stats: Stats::default(),
        };
        jit.shadow.flush(jit.mem.as_mut_slice());
        let v = &jit.vcpu;
        v.set_sys_reg(sysreg::MAIR_EL1, 0xff)?;
        v.set_sys_reg(sysreg::TCR_EL1, tcr)?;
        v.set_sys_reg(sysreg::TTBR0_EL1, jit.shadow.root_ipa())?;
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
        s.fs_base = cpu.segs[FS].base;
        s.gs_base = cpu.segs[GS].base;
    }

    fn store_state(&mut self, cpu: &mut Cpu) {
        let s = self.state();
        cpu.regs = s.regs;
        cpu.eip = s.eip;
        cpu.eflags = s.merge_eflags(cpu.eflags);
    }

    fn flush_shadow(&mut self) {
        self.shadow.flush(self.mem.as_mut_slice());
        self.state().ctl |= ctl::TLBI_ALL;
        self.stats.flushes += 1;
    }

    /// The x86 TLB is flushed by CR3 writes and by paging-related CR0/CR4 changes.
    fn check_shadow(&mut self, cpu: &Cpu) {
        let key = (cpu.cr3, cpu.cr0 & 0x8001_0001, cpu.cr4 & 0xb0);
        if self.shadow_key != Some(key) {
            self.shadow_key = Some(key);
            self.flush_shadow();
        }
    }

    fn map_shadow(&mut self, lin: u32, phys: u32, writable: bool) {
        if !self.shadow.map(self.mem.as_mut_slice(), lin, phys, writable) {
            self.flush_shadow();
            let ok = self.shadow.map(self.mem.as_mut_slice(), lin, phys, writable);
            assert!(ok, "shadow pool smaller than one mapping");
        }
        self.stats.shadow_fills += 1;
    }

    fn invalidate_code_page(&mut self, page: u32) {
        let Some(eips) = self.code_pages.remove(&page) else { return };
        for pa in eips {
            if let Some(b) = self.blocks.remove(&pa) {
                self.by_va.remove(&b.va);
            }
            let off = layout::LOOKUP + hash(pa) * 16;
            let slot = &mut self.mem.as_mut_slice()[off..off + 16];
            if u64::from_le_bytes(slot[..8].try_into().unwrap()) == pa as u64 {
                slot.fill(0xff);
            }
        }
        self.stats.smc_invalidations += 1;
        self.flush_shadow();
    }

    fn reset_code_cache(&mut self) {
        self.blocks.clear();
        self.by_va.clear();
        self.code_pages.clear();
        self.code_next = layout::CODE_CACHE;
        self.mem.as_mut_slice()[layout::LOOKUP..layout::LOOKUP + layout::LOOKUP_ENTRIES * 16].fill(0xff);
        self.flush_shadow();
    }

    fn translate_block<B: Bus>(&mut self, bus: &mut B, eip: u32, phys: u32) {
        let page = phys & !0xfff;
        let lin_page = eip & !0xfff;
        let block_va = layout::va(self.code_next);
        let rt_va = layout::RUNTIME_VA;
        let entries = self.entries;
        let compiled = translate::compile(
            eip,
            |a| (a & !0xfff == lin_page).then(|| bus.read8(page | (a & 0xfff))),
            block_va,
            rt_va,
            &entries,
        );
        let bytes = compiled.code.len() * 4;
        if self.code_next + bytes > layout::CODE_CACHE_END {
            self.reset_code_cache();
            return self.translate_block(bus, eip, phys);
        }
        self.mem.write_words(self.code_next, &compiled.code);
        self.code_next += bytes.next_multiple_of(16);
        let off = layout::LOOKUP + hash(phys) * 16;
        let slot = &mut self.mem.as_mut_slice()[off..off + 16];
        slot[..8].copy_from_slice(&(phys as u64).to_le_bytes());
        slot[8..].copy_from_slice(&block_va.to_le_bytes());
        self.blocks.insert(phys, BlockInfo { va: block_va, boundaries: compiled.boundaries });
        self.by_va.insert(block_va, phys);
        self.code_pages.entry(page).or_default().push(phys);
        self.stats.blocks += 1;
        self.stats.translated_insns += compiled.insns as u64;
        self.state().ctl |= ctl::IC_IALLU;
    }

    fn eip_at(&self, elr: u64) -> Option<u32> {
        let (&va, &pa) = self.by_va.range(..=elr).next_back()?;
        let b = &self.blocks[&pa];
        let word = ((elr - va) / 4) as u32;
        b.boundaries.iter().rev().find(|(w, _)| *w <= word).map(|(_, e)| *e)
    }

    /// Runs translated code until the CPU needs the interpreter for more than
    /// single instructions, or until the vCPU is kicked.
    pub fn run<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) -> hvf::Result<RunExit> {
        self.check_shadow(cpu);
        self.load_state(cpu);
        loop {
            let exit = self.vcpu.run()?;
            match exit {
                Exit::Canceled => {
                    self.store_state(cpu);
                    return Ok(RunExit::Kicked);
                }
                Exit::Exception { syndrome, ipa, .. } if exit.ec() == Some(0x24) => {
                    self.mmio(bus, syndrome, ipa as u32)?;
                }
                Exit::Exception { .. } if exit.ec() == Some(0x16) => {
                    let why = self.state().reason;
                    match why {
                        reason::MISS => self.on_miss(cpu, bus),
                        reason::INTERP => {
                            self.stats.interp_exits += 1;
                            self.store_state(cpu);
                            let e = interp::step(cpu, bus);
                            if e != interp::Exit::Continue || cpu.halted || !supported(cpu) {
                                return Ok(RunExit::Interp(e));
                            }
                            self.check_shadow(cpu);
                            self.load_state(cpu);
                        }
                        reason::PENDING => {
                            self.store_state(cpu);
                            return Ok(RunExit::Kicked);
                        }
                        reason::FAULT => self.on_fault(cpu, bus),
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

    fn on_miss<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) {
        self.stats.miss_exits += 1;
        let eip = self.state().eip;
        match interp::translate(cpu, bus, eip, false) {
            Ok(phys) => {
                self.map_shadow(eip, phys, false);
                if !self.blocks.contains_key(&phys) {
                    self.translate_block(bus, eip, phys);
                }
            }
            Err(e) => {
                self.stats.page_faults += 1;
                self.store_state(cpu);
                let _ = interp::deliver_fault(cpu, bus, e);
                self.load_state(cpu);
            }
        }
    }

    fn on_fault<B: Bus>(&mut self, cpu: &mut Cpu, bus: &mut B) {
        let [esr, far, elr, _] = self.state().info;
        let ec = esr >> 26;
        assert!(ec == 0x25, "unhandled fault in translated code: ESR {esr:#x} FAR {far:#x} ELR {elr:#x}");
        assert!(far >> 32 == 0, "translated code faulted outside the x86 address space: FAR {far:#x}");
        let lin = far as u32;
        let write = esr & (1 << 6) != 0;
        match interp::translate(cpu, bus, lin, write) {
            Ok(phys) => {
                let page = phys & !0xfff;
                if write && self.code_pages.contains_key(&page) {
                    self.invalidate_code_page(page);
                }
                self.map_shadow(lin, phys, write);
                let s = self.state();
                s.tlbi_page = (lin >> 12) as u64;
                s.ctl |= ctl::TLBI_PAGE;
                s.resume = resume::RETRY;
            }
            Err(e) => {
                self.stats.page_faults += 1;
                let eip = self.eip_at(elr).expect("fault outside any translated block");
                self.state().eip = eip;
                self.store_state(cpu);
                let _ = interp::deliver_fault(cpu, bus, e);
                self.load_state(cpu);
                self.state().resume = resume::DISPATCH;
            }
        }
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

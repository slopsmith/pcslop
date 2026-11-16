//! Stage-1 page tables for the VM: static TTBR1 tables for the runtime and a
//! lazily filled TTBR0 shadow of the x86 page tables.

use crate::layout;

const VALID: u64 = 1;
const TABLE: u64 = 1 << 1;
const PAGE: u64 = 1 << 1;
const AF: u64 = 1 << 10;
const SH_INNER: u64 = 3 << 8;
const AP_RO: u64 = 1 << 7;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

const PAGE_SIZE: usize = 4096;

fn write_desc(mem: &mut [u8], off: usize, desc: u64) {
    mem[off..off + 8].copy_from_slice(&desc.to_le_bytes());
}

fn read_desc(mem: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(mem[off..off + 8].try_into().unwrap())
}

/// Builds the TTBR1 tables: the runtime region (executable at EL1) and a
/// linear view of the first 4 GiB of guest physical memory.
pub fn build_ttbr1(mem: &mut [u8]) {
    let l1 = layout::TTBR1_L1;
    let l2 = layout::TTBR1_L2;
    mem[l1..l1 + PAGE_SIZE].fill(0);
    mem[l2..l2 + PAGE_SIZE].fill(0);
    write_desc(mem, l1, layout::ipa(l2) | TABLE | VALID);
    for i in 0..layout::RUNTIME_SIZE >> 21 {
        let block = layout::ipa(i << 21) | AF | SH_INNER | UXN | VALID;
        write_desc(mem, l2 + i * 8, block);
    }
    for gib in 0..4u64 {
        let block = (gib << 30) | AF | SH_INNER | PXN | UXN | VALID;
        write_desc(mem, l1 + (256 + gib as usize) * 8, block);
    }
}

/// TTBR0 shadow tables, allocated from a pool in the runtime region.
pub struct Shadow {
    next: usize,
}

impl Default for Shadow {
    fn default() -> Self {
        Shadow { next: layout::SHADOW_POOL + PAGE_SIZE }
    }
}

impl Shadow {
    pub fn root_ipa(&self) -> u64 {
        layout::ipa(layout::SHADOW_POOL)
    }

    /// Drops every mapping. The caller must invalidate the TLB.
    pub fn flush(&mut self, mem: &mut [u8]) {
        let root = layout::SHADOW_POOL;
        mem[root..root + PAGE_SIZE].fill(0);
        self.next = root + PAGE_SIZE;
    }

    fn table(&mut self, mem: &mut [u8], entry_off: usize) -> Option<usize> {
        let d = read_desc(mem, entry_off);
        if d & VALID != 0 {
            return Some((d & 0x0000_ffff_ffff_f000) as usize - layout::RUNTIME_IPA as usize);
        }
        if self.next + PAGE_SIZE > layout::SHADOW_POOL_END {
            return None;
        }
        let t = self.next;
        self.next += PAGE_SIZE;
        mem[t..t + PAGE_SIZE].fill(0);
        write_desc(mem, entry_off, layout::ipa(t) | TABLE | VALID);
        Some(t)
    }

    fn leaf(&self, mem: &[u8], lin: u32) -> Option<usize> {
        let mut table = layout::SHADOW_POOL;
        for shift in [30, 21] {
            let d = read_desc(mem, table + (((lin >> shift) & 511) as usize) * 8);
            if d & VALID == 0 {
                return None;
            }
            table = (d & 0x0000_ffff_ffff_f000) as usize - layout::RUNTIME_IPA as usize;
        }
        Some(table + (((lin >> 12) & 511) as usize) * 8)
    }

    /// Removes the mapping of one linear page. The caller must invalidate the TLB.
    pub fn unmap(&mut self, mem: &mut [u8], lin: u32) {
        if let Some(off) = self.leaf(mem, lin) {
            write_desc(mem, off, 0);
        }
    }

    /// Makes the mapping of `lin` read-only if it points at `phys_page`.
    /// Returns true when it changed; the caller must invalidate the TLB.
    pub fn write_protect(&mut self, mem: &mut [u8], lin: u32, phys_page: u32) -> bool {
        let Some(off) = self.leaf(mem, lin) else { return false };
        let d = read_desc(mem, off);
        if d & VALID == 0 || d & AP_RO != 0 || (d & 0xffff_f000) as u32 != phys_page {
            return false;
        }
        write_desc(mem, off, d | AP_RO);
        true
    }

    /// Maps the x86 linear page of `lin` to physical `phys`. Returns false when
    /// the pool is exhausted; flush and retry in that case.
    #[must_use]
    pub fn map(&mut self, mem: &mut [u8], lin: u32, phys: u32, writable: bool) -> bool {
        let root = layout::SHADOW_POOL;
        let Some(l2) = self.table(mem, root + ((lin >> 30) as usize) * 8) else { return false };
        let Some(l3) = self.table(mem, l2 + (((lin >> 21) & 511) as usize) * 8) else { return false };
        let mut d = (phys as u64 & !0xfff) | AF | SH_INNER | PXN | UXN | PAGE | VALID;
        if !writable {
            d |= AP_RO;
        }
        write_desc(mem, l3 + (((lin >> 12) & 511) as usize) * 8, d);
        true
    }
}

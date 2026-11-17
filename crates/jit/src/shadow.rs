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
/// Not global: TLB entries are tagged with the ASID of the root.
const NG: u64 = 1 << 11;

const PAGE_SIZE: usize = 4096;

/// Attributes of a writable shadow leaf, without the output address.
pub const LEAF_ATTRS: u64 = AF | SH_INNER | PXN | UXN | NG | PAGE | VALID;
pub const LEAF_RO: u64 = AP_RO;

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

/// One TTBR0 shadow tree, allocated from its own slice of the runtime region.
pub struct Shadow {
    root: usize,
    end: usize,
    next: usize,
}

impl Shadow {
    /// A tree whose root and tables live in `start..end` (runtime-region offsets).
    pub fn new(start: usize, end: usize) -> Self {
        Shadow { root: start, end, next: start + PAGE_SIZE }
    }

    pub fn root_va(&self) -> u64 {
        layout::va(self.root)
    }

    pub fn root_ipa(&self) -> u64 {
        layout::ipa(self.root)
    }

    /// Drops every mapping. The caller must invalidate the TLB.
    pub fn flush(&mut self, mem: &mut [u8]) {
        mem[self.root..self.root + PAGE_SIZE].fill(0);
        self.next = self.root + PAGE_SIZE;
    }

    fn table(&mut self, mem: &mut [u8], entry_off: usize) -> Option<usize> {
        let d = read_desc(mem, entry_off);
        if d & VALID != 0 {
            return Some((d & 0x0000_ffff_ffff_f000) as usize - layout::RUNTIME_IPA as usize);
        }
        if self.next + PAGE_SIZE > self.end {
            return None;
        }
        let t = self.next;
        self.next += PAGE_SIZE;
        mem[t..t + PAGE_SIZE].fill(0);
        write_desc(mem, entry_off, layout::ipa(t) | TABLE | VALID);
        Some(t)
    }

    fn leaf(&self, mem: &[u8], lin: u32) -> Option<usize> {
        let mut table = self.root;
        for shift in [30, 21] {
            let d = read_desc(mem, table + (((lin >> shift) & 511) as usize) * 8);
            if d & VALID == 0 {
                return None;
            }
            table = (d & 0x0000_ffff_ffff_f000) as usize - layout::RUNTIME_IPA as usize;
        }
        Some(table + (((lin >> 12) & 511) as usize) * 8)
    }

    /// True if `lin` is mapped, and writable when `write` is set.
    pub fn maps(&self, mem: &[u8], lin: u32, write: bool) -> bool {
        let Some(off) = self.leaf(mem, lin) else { return false };
        let d = read_desc(mem, off);
        d & VALID != 0 && (!write || d & AP_RO == 0)
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

    /// Every valid leaf as (linear page, physical page, writable, leaf offset).
    pub fn leaves(&self, mem: &[u8]) -> Vec<(u32, u32, bool, usize)> {
        let mut out = Vec::new();
        let table_of = |d: u64| (d & 0x0000_ffff_ffff_f000) as usize - layout::RUNTIME_IPA as usize;
        for i1 in 0..4usize {
            let d1 = read_desc(mem, self.root + i1 * 8);
            if d1 & VALID == 0 {
                continue;
            }
            let l2 = table_of(d1);
            for i2 in 0..512usize {
                let d2 = read_desc(mem, l2 + i2 * 8);
                if d2 & VALID == 0 {
                    continue;
                }
                let l3 = table_of(d2);
                for i3 in 0..512usize {
                    let off = l3 + i3 * 8;
                    let d = read_desc(mem, off);
                    if d & VALID != 0 {
                        let lin = ((i1 << 30) | (i2 << 21) | (i3 << 12)) as u32;
                        out.push((lin, (d & 0xffff_f000) as u32, d & AP_RO == 0, off));
                    }
                }
            }
        }
        out
    }

    /// Clears the leaf entry at `off`, as returned by `map`.
    pub fn clear_leaf(&mut self, mem: &mut [u8], off: usize) {
        write_desc(mem, off, 0);
    }

    /// Maps the x86 linear page of `lin` to physical `phys` and returns the
    /// offset of the leaf entry. Returns None when the pool is exhausted;
    /// flush and retry in that case.
    #[must_use]
    pub fn map(&mut self, mem: &mut [u8], lin: u32, phys: u32, writable: bool) -> Option<usize> {
        let l2 = self.table(mem, self.root + ((lin >> 30) as usize) * 8)?;
        let l3 = self.table(mem, l2 + (((lin >> 21) & 511) as usize) * 8)?;
        let mut d = (phys as u64 & !0xfff) | LEAF_ATTRS;
        if !writable {
            d |= AP_RO;
        }
        let off = l3 + (((lin >> 12) & 511) as usize) * 8;
        write_desc(mem, off, d);
        Some(off)
    }
}

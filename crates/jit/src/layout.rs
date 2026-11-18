//! Addresses of the runtime inside the VM. See docs/architecture.md, "VM runtime layout".

/// Guest physical address of the runtime region, above any x86 physical address.
pub const RUNTIME_IPA: u64 = 0x8_0000_0000;
pub const RUNTIME_SIZE: usize = 128 << 20;
pub const IPA_BITS: u32 = 36;

/// Virtual address of the runtime region (TTBR1, first 1 GiB slot).
pub const RUNTIME_VA: u64 = 0xffff_ff80_0000_0000;
/// Virtual address of the linear view of x86 physical memory (TTBR1, slots 256..260).
pub const PHYS_VA: u64 = 0xffff_ffc0_0000_0000;

pub const VECTORS: usize = 0x0;
pub const RUNTIME_CODE: usize = 0x800;
pub const STATE: usize = 0x1_0000;
pub const STACK_TOP: usize = 0x2_0000;
pub const TTBR1_L1: usize = 0x2_0000;
pub const TTBR1_L2: usize = 0x2_1000;
pub const LOOKUP: usize = 0x100_0000;
pub const LOOKUP_ENTRIES: usize = 1 << 20;
/// Shadow fills done inside the VM, for the host to account for (`FILL_LOG_ENTRIES` of 32 bytes).
pub const FILL_LOG: usize = 0x14_0000;
pub const FILL_LOG_ENTRIES: usize = 256;
/// One bit per 4 KiB physical page that holds translated code, for the in-VM fill.
pub const CODE_BITMAP: usize = 0x16_0000;
pub const CODE_BITMAP_END: usize = 0x18_0000;
/// Ring of predicted returns, 16 bytes each: the tag of the return address
/// (as in `VLOOKUP`) and the host code that continues there.
pub const RSTACK: usize = 0x18_0000;
pub const RSTACK_BYTES: usize = 64 * 16;
/// Offsets of the shadow leaves of non-global x86 pages, `NONGLOBAL_ENTRIES`
/// u32 per tree (CPL 0, then CPL 3), so a CR3 switch can clear them in the VM.
pub const NONGLOBAL: usize = 0x19_0000;
pub const NONGLOBAL_ENTRIES: usize = 8192;
pub const SHADOW_POOL: usize = 0x20_0000;
pub const SHADOW_POOL_END: usize = 0x100_0000;
/// The dispatcher's cache from (EIP, CPL 3) to block, ahead of the address translation.
pub const VLOOKUP: usize = 0x200_0000;
pub const VLOOKUP_ENTRIES: usize = 1 << 16;
pub const CODE_CACHE: usize = VLOOKUP + VLOOKUP_ENTRIES * 16;
pub const CODE_CACHE_END: usize = COUNTERS;
/// Execution counters of translated code, u32 each: a block's entries and
/// the exits it leaves through.
pub const COUNTERS: usize = RUNTIME_SIZE - (8 << 20);
pub const COUNTERS_END: usize = RUNTIME_SIZE;

pub const fn va(offset: usize) -> u64 {
    RUNTIME_VA + offset as u64
}

pub const fn ipa(offset: usize) -> u64 {
    RUNTIME_IPA + offset as u64
}

//! The second tier: recompiles hot code through an IR. See docs/jit-tier2.md.

mod back;
mod front;
pub mod ir;
mod opt;

use a64::Asm;

use super::{Compiled, Emit, X87Loop, flags, x87};
use crate::runtime::Entries;

/// The region starting at `eip` compiled for `block_va`, or None when tier 2
/// does not handle its first instruction or runs out of registers. Its code
/// ends where the tier-1 translation of the same entry must follow, which
/// it falls back to when its entry checks fail.
/// `x87` is the control word and the slots full (relative to TOP) as the
/// region is entered, when x87 code may be compiled.
#[allow(clippy::too_many_arguments)]
pub fn compile_region(eip: u32, byte: &mut dyn FnMut(u32) -> Option<u8>, block_va: u64, rt_va: u64, rt: &Entries, user: bool, x87: Option<(u16, u8)>) -> Option<Compiled> {
    let mut f = front::build(eip, byte, x87)?;
    // The x87 entry checks only pay off when the entry block runs x87 code.
    let entry_x87 = f.blocks[0].insts.iter().any(|&v| f.op(v).is_x87());
    if f.x87.is_some() && !entry_x87 {
        f = front::build(eip, byte, None)?;
    }
    opt::run(&mut f);
    let mut e = Emit {
        a: Asm::new(),
        block_va,
        dispatch_va: rt_va + rt.dispatch as u64,
        exit_va: rt_va + rt.exit as u64,
        unstale_va: rt_va + rt.unstale as u64,
        page: Some(eip & !0xfff),
        chains: Vec::new(),
        user,
        stored: false,
        need: flags::ALL,
        x87: x87::X87Block::default(),
        sse_clean: false,
        sse_checked: 0,
        mmx_known: None,
        xmm: Default::default(),
        superblock: false,
        fold: None,
        fold_ok: false,
        at: eip,
        lp: X87Loop::default(),
        counters: None,
        exits: Vec::new(),
        cold: Vec::new(),
        starts: Vec::new(),
    };
    if std::env::var_os("SLOPPER_TIER2_DUMP").is_some() {
        eprintln!("{f}");
    }
    let fallback = e.a.label();
    let lowered = back::lower(&mut e, &f, fallback)?;
    e.emit_cold();
    e.a.bind(fallback);
    let code = e.a.finish();
    Some(Compiled {
        code,
        boundaries: lowered.boundaries,
        insns: lowered.insns,
        chains: e.chains,
        has_rep: false,
        recent: false,
        interp_after: None,
        retry: false,
        x87: lowered.x87,
        x87_loop: false,
        exits: e.exits,
    })
}

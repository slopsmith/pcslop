# JIT tier 2: design

Tier 1 is the translator in `crates/jit/src/translate*`: it turns each x86
instruction straight into A64 while it decodes, with some look-ahead
(flag liveness, address folding, the x87 register cache, superblocks, x87
loop heads). It translates quickly and every block goes through it, but it
cannot see across instructions well enough to remove the waste measured in
hot code: guest state kept in the state block, constants loaded into
registers for operations that take immediates, copies, and per-operation
x87 checks (about 2 to 4 host instructions per x86 instruction for integer
code and 20 to 45 for x87 code).

Tier 2 recompiles code that has proven hot through an intermediate
representation (IR), on a background thread, and may take as long as it
needs. Tier 1 stays as it is: it runs everything first, it profiles, and it
is where tier 2 falls back to.

## Goals and the exactness bar

- Speed of the generated code first. Compile time matters only in that tier 2
  runs off the vCPU thread and should not lag far behind the workload.
- Tier 2 keeps every behaviour software relies on: integer results and
  flags, memory order, faults at the right instruction with precise state,
  self-modifying code, interrupts arriving within bounded time.
- Tier 2 may drop behaviour no program relies on in practice, and tier 1 keeps
  the exact version for anything that does:
  - x87 FIP, FDP and FOP are written only when a region is left, with the
    values of the last x87 instruction run.
  - Empty x87 registers keep whatever the last value there was, not the
    exact 80-bit pattern the interpreter leaves.
  - x87 arithmetic runs on doubles without a check per operation, under
    conditions listed in the x87 section; the result in extreme cases
    (overflow past the double range, double denormals) differs from the
    80-bit interpreter.
- When something breaks, tier 2 must step back for that code: a region that
  deopts too often is dropped and its entry marked tier-1 only;
  `SLOPPER_JIT_TIER2=off` turns tier 2 off, and `SLOPPER_JIT_TIER2_X87=exact`
  keeps x87 regions on tier 1's exact semantics, to tell tier-2 bugs from
  others.

## Profiling and tier-up

1. **Hotness from samples.** The emulator already kicks the vCPU every
   500 us. Each kicked exit reads the PC; a PC in a tier-1 block adds one to
   that block's sample count. This costs nothing in translated code.
2. **Instrumenting a hot block.** A block that collects enough samples is
   translated again with the execution counters of stage 0 (entry and per
   exit, `profile counts`), along with the blocks it chains to on its page.
   Counting costs about 6% where it runs, so only hot code pays it, and only
   for a short time.
3. **Requesting tier 2.** After enough counted entries the host builds a
   region request and hands it to the compiler thread.

## Regions

A region has one entry and is made of tier-1 blocks (basic blocks of x86
code) on one 4 KiB page, chosen from the counters:

- start at the hot block, add successors whose edge was taken often enough,
  stop at a size limit;
- loops are allowed: a back edge to the region entry or to another block of
  the region stays inside;
- cold edges leave the region through exits that chain to tier-1 code.

One page first, because chains already assume that a page keeps its
linear-to-physical mapping while its blocks live (the dispatcher epochs and
shadow invalidation enforce it). Regions across pages need a mapping guard at
each page crossing and come later.

A region is keyed like a block (physical address, EIP, CPL 3) and replaces
its entry block: the host patches the tier-1 block's first word into a branch
to the region and points the lookup tables at it, so dispatches, chains and
returns all reach it. A write to the page, or anything that removes the
entry block, drops the region and restores the tier-1 entry.

## The IR

- **Form:** a control-flow graph of IR blocks in SSA form, one per x86 basic
  block of the region, with explicit loop back edges.
- **Values:** 32-bit integers, 64-bit doubles, 128-bit vectors, and flag
  values. A flag value is the operation that produced it with its operands
  and result (as the lazy flags of an interpreter); consumers ask for the bits
  they need (one condition for a jcc, CF for ADC, all of them for PUSHF), so
  whatever no one asks for is never computed.
- **Guest registers:** SSA variables. Each IR block has the eight registers
  as inputs; the entry takes them from their pinned host registers
  (w19..w26). Exits and fault points read them back from wherever the
  register allocator put them (see Precise state).
- **Guest state** (PF and AF results, FS and GS bases, x87 TOP, tags and status
  word, MXCSR, the pending-interrupt word) is loaded once and stored when
  needed, so repeated accesses become register uses.
- **Memory:** loads and stores of guest memory keep their order among
  themselves. A load may be removed when an earlier load or store of the
  same address expression reaches it with no store in between that could
  alias. MMIO is not known at compile time, so a load is never removed across
  a store, and stores are never removed.
- **Interrupts:** each loop back edge checks the pending word, as tier 1's
  backward branches do.

## Passes

In order, each simple and separately testable:

1. Build the IR from the x86 code of the region (a front end that mirrors
   tier 1's instruction coverage; anything it does not handle ends the region
   there with an exit to tier 1).
2. Constant and copy propagation, including known values of guest registers
   along a path (`xor eax, eax` then `test eax, eax`).
3. Flag demand: compute only the flag bits that are read; an x86 compare and
   its jcc become one A64 compare and branch.
4. Redundant load and store removal for guest state; for guest memory within
   the limits above.
5. Dead code removal.
6. Loop-invariant code motion: constants, address bases and state loads move
   to the loop preheader.
7. x87 lowering (below).
8. Instruction selection to A64, using address modes with immediate offsets
   and shifted or extended registers, and flag-setting instructions where the
   flags are used.
9. Register allocation: linear scan over the region, with guest registers
   preferring their pinned registers so entry and exits need few moves.
10. Layout: the hot successor of every branch falls through, branches are
    inverted to make that so, cold blocks (rare exits, bails) go after the
    hot code.

## x87 in tier 2

Within a region, TOP is fixed relative to its value at the entry, which the
entry checks (with the tags the region relies on), as tier 1's x87 loop heads
already do. Stack slots then become plain virtual double registers.

Arithmetic runs on doubles without a check per operation when, at region
entry, the control word masks every exception. Precision control is honoured
by rounding results to single precision where the control word asks for it.
The status word bits software reads (C0 to C3 from compares, TOP) are exact;
PE is set from the FPU's sticky inexact flag when the region is left. A
control word that unmasks exceptions keeps that code on tier 1.

## Precise state at faults

A guest memory access in a region can fault: a shadow fill (the access is
retried, no state needed), an x86 page fault, a write to a code page, or an
access the host must finish. For the last three the host needs the exact
x86 state at the faulting instruction.

For each memory access, tier 2 records a deopt entry in the region's side
table: the x86 EIP and, for every guest register and live flag, where its
value is (a host register, a spill slot or a constant), and the x87 state
the same way. The host's fault path, which today calls `deopt_at_fault` and
`eip_at` for tier-1 blocks, uses these entries for regions: it rebuilds the
`Cpu`, and the interpreter continues at that EIP as for tier 1.

Liveness keeps this cheap: a value only needs a location at a fault point if
the x86 code could read it after that point. The first version may instead
require guest registers to sit in their pinned registers at every access.
That is simpler to check, and still allows the other optimisations.

## Background compilation

- The host takes a snapshot of the region's x86 bytes (as tier 1 does for
  block revival) and its profile, and queues it.
- The compiler thread builds and optimises the IR and generates code with
  relocations (branches to runtime entries and to tier-1 blocks).
- At its next exit the vCPU thread installs finished regions. It first checks
  that the code pages were not written since the snapshot, then copies the
  code into the code cache, applies relocations, patches the entry block and
  links chains.
- Which exit installs a region varies from run to run, so
  `SLOPPER_JIT_TIER2=sync` compiles on the vCPU thread at the moment of the
  request, for reproducible debugging.

## Testing

- The random-program tests and verify mode compare tier 1 with the
  interpreter today. They extend to tier 2 with a mode that makes every
  block a region at once (`SLOPPER_JIT_TIER2=eager`). The integer tests then
  compare all three; x87 tests compare tier 2 within the documented
  tolerance.
- Each pass gets unit tests on IR snippets.
- The XP benchmark, the Ubuntu benchmark, 3DMark Lobby and a Vista boot are
  the end-to-end checks, with `profile counts` and the block dump showing
  what tier 2 made of the hot code.

## Phases

1. Instrumented re-translation of sampled hot blocks; region selection; the
   IR, front end for integer code, flag demand, propagation, dead code,
   instruction selection, register allocation, layout, side tables, install
   and drop. Synchronous compilation. Measure against tier 1.
2. Background compilation.
3. Loops with loop-invariant code motion.
4. x87, then SSE.
5. Regions across pages, and guest registers kept out of their pinned
   registers between fault points.

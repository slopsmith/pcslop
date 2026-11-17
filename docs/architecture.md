# Architecture

86slopper emulates a 2003-era x86-32 PC well enough to run Windows XP SP3, and aims to beat `qemu-system-i386 -accel tcg` on the same machine. It targets macOS on Apple Silicon only. Linux (Ubuntu 8.04) is a second guest, mainly as a more lenient test for the GPU.

## Big picture

```
 macOS process (Rust)                      Hypervisor.framework ARM64 VM
+-------------------------------+         +-------------------------------------+
| device models (chipset, IDE,  |  hvc /  | EL1 runtime (hand-written asm):     |
|   VGA, PIT, PIC, ...)         | <-----> |   exception vectors, shadow-page    |
| x86 decoder + translator      |  exits  |   fault handler, dispatcher         |
| code cache manager            |         | translated code (EL0 or EL1)        |
| interpreter (reference/slow)  |         | x86 RAM, mapped in two places       |
+-------------------------------+         +-------------------------------------+
```

Translated ARM64 code does not run in the macOS process. It runs inside a Hypervisor.framework VM that we fully own. We provide the VM's exception vectors and stage-1 page tables, so:

- **x86 paging becomes ARM paging.** TTBR0 covers exactly 4 GiB (`T0SZ = 32`) and holds a *shadow* of the x86 page tables. A guest load or store compiles to a single ARM load or store at the x86 linear address. The hardware TLB does the translation, with no software TLB lookup like TCG's.
- **x86 page faults are cheap.** A missing shadow entry raises an ARM data abort, which our EL1 handler takes without leaving the VM. It walks the x86 page table, fills the shadow entry, and returns. If the x86 entry is not present, it delivers `#PF` to the guest instead.
- **Memory ordering is free when we need it.** The `ACTLR_EL1.EnTSO` bit gives x86-TSO in hardware. While the guest has one CPU, it only has to be ordered against our device threads, so we leave TSO off: it doubles the cost of an exception (see below). We turn it on when SMP arrives.

TTBR1 (the high half) holds what the guest must never reach: the runtime, the code cache, CPU state, and a linear map of x86 physical RAM for the page walker. Translated code always forms addresses as zero-extended 32-bit values, so it can never touch the high half.

### Measured on this M4 Pro (`experiments/hvf-probe`)

| What | Cost |
| --- | --- |
| VM exit round trip (`hvc` → `hv_vcpu_run` returns → re-enter) | ~700 ns |
| Exception taken and returned inside the guest, TSO off | ~17 ns |
| Same, TSO on | ~31 ns |
| Message-passing litmus test, 400 M reads, TSO off / on | ~190 M reorderings / 0 |

Other facts we checked:

- Max IPA size is 40 bits.
- Stage-1 tables support a 4 KiB granule, and stage-2 does too with `hv_vm_config_set_ipa_granule` (macOS 26+).
- `HAFDBS = 0`, so the hardware does not set Accessed/Dirty bits. We emulate x86 A/D bits by mapping pages read-only first and taking the write fault.
- Apple's private parity/adjust flag extension (`ACTLR_EL1.EnAPFLG`) is **not** exposed under HVF. We compute PF and AF in software, lazily.

Design rule from these numbers: **anything hot must be handled inside the VM**. A VM exit costs about as much as 40 in-guest exceptions.

## CPU model

- **Model:** a single-core Pentium 4 (Northwood): i686 plus MMX, SSE and SSE2, no Hyper-Threading, no PAE at first. XP needs at most PSE, `CMPXCHG8B`, `SYSENTER`, `FXSAVE`, and SSE/SSE2.
- **Privilege mapping:** x86 CPL 3 runs at ARM EL0, and CPL 0 at EL1. The shadow PTE's AP bits then enforce the x86 U/S bit in hardware, and CR0.WP maps onto read-only AP. Changing CPL means switching exception level, which costs about 17 ns.
- **Segmentation:** XP uses flat 4 GiB segments, except FS (the TEB in user mode, the KPCR in kernel mode). Translation assumes the current segment layout, and blocks are keyed on it. Flat segments cost nothing, and FS gets a base add. Real mode and odd segments go through the general path, which adds bases and checks limits.
- **Flags:** arithmetic flags live in ARM NZCV wherever possible, using FlagM (`CFINV`, `RMIF`, `SETF8/16`) and storing the carry inverted for subtraction, as Rosetta 2 does. PF and AF come from a saved result and operands only when something reads them. Flag-liveness analysis inside a block removes dead flag work.
- **FPU:** x87 runs on 64-bit doubles in NEON registers (agreed default), and an accurate 80-bit mode built on SoftFloat 3 may come later. FEAT_AFP covers the x86 denormal and flush-to-zero cases. SSE/SSE2 maps onto NEON.
- **Interrupts:** the host raises an interrupt by writing a "pending" word in shared memory. Translated code checks it at block ends and on backward branches (one `ldr` + `cbnz`). `hv_vcpus_exit` is the fallback when the guest is halted or stuck.

## Translator

1. An x86 decoder (shared with the interpreter) produces a small IR per basic block, which later grows into traces and superblocks.
2. The eight x86 GPRs stay in fixed ARM registers for the whole block, and EFLAGS and segment state get registers of their own.
3. The ARM64 emitter is hand-written. LLVM and Cranelift translate too slowly for a JIT like this.
4. The translator runs in the host process. A missing block costs an exit plus translation time, and that happens rarely once the guest is warm. Block chaining patches branches directly in the code cache, which is shared host/guest memory.
5. **Self-modifying code:** a physical page with translated code gets a read-only shadow mapping. A write to it faults, which invalidates that page's blocks; the faulting instruction then finishes in the interpreter, so the rest of a block that rewrote itself is translated again. Writes by the interpreter and by DMA reach the JIT through a per-page watch in the board. Any mapping that replaces a valid shadow entry also invalidates its TLB entry; otherwise a stale writable TLB entry lets code change unnoticed.
6. **Pages rewritten often** (four invalidations) switch to self-checking blocks instead: they stay writable, and every block compares its own code bytes on entry and asks to be retranslated when they differ. An instruction that straddles a page end is translated too, with a run-time check of its bytes on the second page.

### Interpreter

A plain host-side interpreter comes first, for three reasons:

- It is the correctness oracle. We check it against the SingleStepTests suites and test386, and then use it to differential-test the JIT.
- It is the fallback for rare instructions and odd modes (V86, task switches).
- It boots firmware before the JIT exists.

## Platform and firmware (LLE)

- **Firmware:** SeaBIOS + SeaVGABIOS run as real firmware. We do not emulate BIOS calls at a high level. SeaBIOS is LGPLv3 and runs as a separate ROM image, so its license does not affect ours.
- **Board:** i440FX + PIIX3/PIIX4, the board SeaBIOS knows best.
- **Configuration:** goes through QEMU's `fw_cfg` interface (I/O ports `0x510`/`0x511`), so SeaBIOS can run its QEMU code path unchanged.
- **ACPI:** current SeaBIOS no longer builds ACPI tables itself. It expects the VMM to pass them through `fw_cfg` (`etc/table-loader`). The plan:
  - First boot XP with **no ACPI and no MP table** ("Standard PC" HAL, PIC + PIT). Our SeaBIOS build sets `CONFIG_MPTABLE=n`: with an MP table present, Setup would pick the MPS uniprocessor HAL, which needs a local APIC and an I/O APIC.
  - Generate our own ACPI tables later, for the ACPI uniprocessor HAL and power-off.
- **Hot MMIO and ports, handled inside the VM:** APIC TPR (if we move to an APIC HAL), PIC EOI, and PIT counter reads. Everything else exits to the host.

## VM runtime layout

These are the starting decisions for phase 3.

**Guest physical (IPA) map**
- `0 .. 4 GiB` mirrors x86 physical memory. RAM is the same host allocation the board and interpreter use, mapped with `hv_vm_map`. MMIO holes stay unmapped, so an access there is a stage-2 fault: a VM exit that carries the faulting register and access size in ESR (ISV), and the host emulates it.
- `4 GiB and up` holds the runtime: exception vectors, dispatcher, code cache, shadow page tables, CPU state and lookup tables. Guest code cannot form addresses there.

**Stage-1 translation**
- TTBR0 (`T0SZ = 32`, exactly 4 GiB, 4 KiB granule) is the shadow of the x86 page tables. VA equals the x86 linear address, so a guest load is one ARM load at a zero-extended 32-bit address.
- TTBR1 maps the runtime and a linear view of x86 physical memory, for the page walker.
- Shadow entries are filled lazily by the EL1 data-abort handler, which walks the non-PAE x86 page tables itself (4 KiB and 4 MiB pages), sets Accessed and Dirty as the CPU would, and logs each fill. The host takes the log in at the next exit, for its CR3-switch and self-modifying-code bookkeeping. A missing shadow table, an x86 page fault, a write to a code page or a full log go to the host instead. `SLOPPER_JIT_NO_VM_FILL` sends every fill to the host. Pages start read-only so the first write can set the x86 Dirty bit (the hardware does no A/D updates here). Pages holding translated code stay read-only to catch self-modifying code.
- There are two shadow trees, one for CPL 0 and one for CPL 3, each with its own ASID. A CR3 load clears only the non-global leaves and keeps the tables and global (kernel) entries. Snapshots of the paging structures let a flush be skipped when nothing changed; after a CR3 switch they count as incomplete until the next full flush.
- x86 user pages are EL0-accessible and supervisor pages EL1-only. CPL 3 runs at EL0, so the MMU enforces U/S and CR0.WP.

**Register assignment in translated code**
- `w19..w26`: EAX, ECX, EDX, EBX, ESP, EBP, ESI, EDI (callee-saved, so runtime calls do not spill them).
- `x27`: base of FS (TEB/KPCR accesses add it).
- `x28`: pointer to the CPU state block.
- NZCV: x86 CF, ZF, SF and OF, with carry inverted after subtraction (FlagM `CFINV` fixes it up only when needed). PF and AF come from the last result and operands, saved in the state block when a later instruction might read them.

**Control flow**
- A lookup table in runtime memory maps x86 EIP to translated code: the dispatcher translates EIP to a physical address with `AT S1E1R` and looks the block up in a 2-way table of 2^18 entries indexed by a Fibonacci hash. A miss makes a VM exit, the host translates the block into the shared code cache, and the guest resumes. Jumps within a page are chained directly.
- Port I/O, unimplemented instructions and mode changes exit to the host. The interpreter runs anything the JIT does not handle: real mode, V86, task switches.
- Interrupts: the host sets a pending word in the state block, which translated code checks on backward branches and block exits.

## Phases

1. **Interpreter + minimal PC:** RAM, ROM, PIC, PIT, CMOS, PCI host bridge, `fw_cfg`, serial debug port. Goal: SeaBIOS prints its banner over the debug port.
2. **VGA + IDE/ATAPI + keyboard:** boot the XP CD into text-mode setup.
3. **HVF runtime:** EL1 runtime, shadow paging, block translator for 32-bit flat code; the interpreter covers everything else.
4. Finish the XP install. **Benchmark** boot time against QEMU TCG.
5. Optimise: traces, flag liveness, in-VM device fast paths, ACPI HAL. Then SMP with TSO.
6. **GPU:** an emulated ATI Radeon 9700 that draws with Metal, running the stock radeon drivers on Linux and Catalyst on XP. See [gpu.md](gpu.md).

## Open questions

- (Decided) x86 CPL 0 runs at EL1 alongside the runtime; vic agreed that the speed is worth the risk.

- (Decided) Shadow page tables across CR3 switches: one tree per privilege level, kept across switches; only non-global leaves are cleared. Caching a tree per address space would cut the refills further.
- I-cache maintenance when the host patches code that the guest executes. `CTR_EL0` traps in macOS user space, so we need to read it inside the VM.

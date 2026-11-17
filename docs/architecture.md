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
- **Board:** i440FX + PIIX3/PIIX4, the board SeaBIOS knows best. `--board bx` swaps in a 440BX with an AGP port for the Radeon (see [gpu.md](gpu.md#bus-agp-on-a-440bx)); our SeaBIOS build carries a small patch for it.
- **Configuration:** goes through QEMU's `fw_cfg` interface (I/O ports `0x510`/`0x511`), so SeaBIOS can run its QEMU code path unchanged.
- **ACPI:** current SeaBIOS no longer builds ACPI tables itself. We build ACPI 1.0b tables (RSDP, RSDT, FADT, FACS and a DSDT encoded by a small AML builder in `crates/pc/src/acpi`) and pass them through `fw_cfg` as QEMU does: `etc/acpi/tables`, `etc/acpi/rsdp`, and the linker/loader script `etc/table-loader`. The DSDT has the PCI root with `_PRT` and PIRQ link devices, the ISA devices, a control-method power button on GPE0 bit 0, and `\_S5`.
  - A PIIX4 power management function at 00:01.3 holds PM1 event and control, the PM timer and GPE0 at I/O 0x600 (where SeaBIOS puts them when a table loader exists), with the SCI on IRQ 9. Writing SLP_EN with the S5 sleep type ends the run, and the control socket's `power` presses the power button.
  - A control-method battery `\_SB.BAT0` (`PNP0C0A`) and AC adapter `\_SB.AC` (`ACPI0003`) mirror the Mac's power state. A helper thread reads IOKit's power-source API and the `AppleSmartBattery` registry entry every 2 s; capacities are in mAh, and the remaining capacity follows the percentage macOS shows. The AML reads twelve 32-bit registers at I/O 0x640..0x66F:

    | Offset | Field | Contents |
    |---|---|---|
    | 0x00 | PSTS | bit 0 battery present, bit 1 AC online |
    | 0x04 | PEVT | pending events, write 1 to clear: bit 0 AC, bit 1 battery status, bit 2 battery info |
    | 0x08 | BSTA | `_BST` state: bit 0 discharging, bit 1 charging, bit 2 critical |
    | 0x0C | BRAT | present rate (mA), 0xFFFFFFFF if unknown |
    | 0x10 | BRCP | remaining capacity (mAh) |
    | 0x14 | BPVL | present voltage (mV) |
    | 0x18 | BDCP | design capacity (mAh) |
    | 0x1C | BFCP | last full charge capacity (mAh) |
    | 0x20 | BDVL | design voltage (mV) |
    | 0x24 | BWRN | warning capacity (10% of full) |
    | 0x28 | BLOW | low capacity (2% of full) |
    | 0x2C | BGRN | granularity (1% of full) |

    A change in AC state, charge state, presence, capacity or whole percent sets PEVT bits and raises GPE0 bit 1, and `\_GPE._E01` sends `Notify(AC, 0x80)`, `Notify(BAT0, 0x80)` or `Notify(BAT0, 0x81)` for each bit. `--battery host|off|PERCENT[:ac]` picks the source at start (`off` means no battery, on AC), and the control socket's `battery` shows it or changes it with the same argument.
  - XP was installed with **no ACPI and no MP table** ("Standard PC" HAL, PIC + PIT). An installed XP keeps its HAL, so neither the ACPI tables nor the MP table SeaBIOS now builds changes it.
  - With the APICs present, a MADT lists the local APIC, the I/O APIC, IRQ 0 on input 2 and level-triggered IRQs 5, 9, 10 and 11, and the DSDT declares the CPU as `\_PR.CPU0`. A fresh XP install picks the ACPI Uniprocessor HAL (halaacpi), and the processor object loads intelppm.sys, which reads `MSR_EBC_FREQUENCY_ID` (0x2C) on any family 15 CPU.
- **Interrupt controllers:** two 8259s, a local APIC at `0xFEE00000` and an 82093AA I/O APIC at `0xFEC00000`. ISA IRQs drive both the 8259s and the I/O APIC (IRQ 0 on input 2, as in QEMU); PIRQA..D also drive I/O APIC inputs 16..19. The 8259 reaches the CPU through LINT0 set up as ExtINT, which SeaBIOS does. The APIC timer runs from a 100 MHz bus clock and is polled with the other timers. SeaBIOS builds the MP table from what it reads back.
- **Clock ticks:** the APIC HALs keep Windows' clock by counting RTC periodic interrupts, and the Standard PC HAL by counting PIT ticks. The RTC and the PIT both remember ticks that came due while the emulation thread was busy and deliver them one per acknowledgement (the PIC or I/O APIC taking IRQ 0, a read of RTC register C), up to 1000, so the guest's clock keeps pace with the PM timer and the TSC.
- **Serial:** COM1 is a 16550A UART at `0x3F8`, IRQ 4. `--serial PATH` (or `-` for stderr) logs what the guest sends, and the control socket's `serial LINE` types into it. Linux takes `console=ttyS0`.
- **Hot MMIO and ports, handled inside the VM:** APIC TPR, PIC EOI, and PIT counter reads. Everything else exits to the host.
  - The ACPI HAL touches the TPR on every IRQL change, about 400k times a second while booting. The host leaves the linear page that maps the APIC unmapped in the shadow tables and records it in the CPU state block; the fault handler then serves 32-bit TPR loads and stores at CPL 0 from a copy of the TPR there. The host also publishes the priority class of the highest request that only the TPR holds back, and a TPR store below it sets the pending word. Other APIC registers exit as before.
- **Sound:** an Intel 82801AA AC'97 controller (8086:2415) at 00:04.0 on PIRQD, with a codec that reports itself as a SigmaTel STAC9700 and supports variable-rate audio. It follows QEMU's `hw/audio/ac97.c`. `--no-audio` leaves it out. XP binds its built-in `ac97intc` driver (`wdma_int.inf` matches `PCI\VEN_8086&DEV_2415` with any subsystem ID), and Linux uses `snd-intel8x0`.
  - The bus masters run on a thread of their own (`ac97::Worker`), every millisecond, so they keep time while the emulation thread is busy with JIT exits, MMIO or GPU work. The controller sits behind a mutex that the guest's NAM/NABM accesses, the worker, `reset` and save states share; INTA# is mirrored in an atomic that the board reads when it routes PCI interrupts. When the worker raises INTA# it unparks the emulation thread and kicks the vCPU out of the VM, and the next `Board::tick` delivers the interrupt. The worker reads and writes guest RAM directly; pages it writes go to the JIT's code watch on the next tick. `SLOPPER_AUDIO_SYNC=1` keeps the old model, with `Board::tick` running the bus masters, which is also what the unit tests use.
  - PCM out runs at its sample rate in host time, so CIV and PICB advance smoothly (about 48 frames a millisecond at 48 kHz) and buffer-completion interrupts come on time. It feeds a ring that the AudioUnit render callback drains on CoreAudio's thread, resampling linearly to the device rate. The ring is held at a fixed latency, `--audio-latency MS` (60 by default): whenever it falls below three quarters of that, as at the start or after the guest fell behind, the bus master tops it up from guest memory at once, and otherwise it runs up to 0.5% fast or slow to hold the level, which absorbs drift between the host clock and the device clock without changing pitch. A ring at twice the latency holds playback. An empty ring plays silence and never stalls the guest; a guest that runs out of buffers gets DCH, CELV and LVBCI as on hardware, and raising LVI resumes from the next buffer.
  - A stall of the emulation thread is covered by the ring plus what the guest has queued past CIV, since the worker keeps playing the guest's buffers while the guest cannot refill them. XP's driver queues 20 to 40 ms; Half-Life with 60 ms stalls every 250 ms plays without a gap at the default latency. Longer stalls need a longer `--audio-latency`, at the cost of delay.
  - PCM in and the mic record silence at their sample rate in host time.
  - The AudioUnit API is used directly, through a few lines of FFI, rather than `cpal`: it needs no dependencies, and the DefaultOutput unit follows changes of the default output device.
  - `--audio-wav PATH` records what the player takes from the ring, silence for an empty ring included, at the first sample rate it uses, so underruns show up as gaps. `--audio-silent` plays nothing but keeps real-time pacing, using a wall-clock thread in place of CoreAudio. `status` reports the ring's level and how often it ran dry, and the control command `stall MS EVERY_MS` sleeps the emulation thread to test this under load.
- **Network:** a Realtek RTL8139C+ (10EC:8139, revision 0x20) at 00:03.0, with the same registers behind an I/O BAR and a memory BAR, on INTA# (PIRQC, so IRQ 11 or I/O APIC input 18). `--no-net` leaves it out. The card is a port of QEMU's `hw/net/rtl8139.c`: the receive ring and four transmit descriptors that XP's built-in `rtl8139.sys` (`netrtsnt.inf`) and Linux's 8139too use, and the C+ descriptor rings with checksum, segmentation and VLAN offloads that Linux's 8139cp uses.
  - Behind the card is libslirp, the stack under QEMU's `-netdev user`, with QEMU's layout: the guest gets 10.0.2.15 by DHCP, the gateway 10.0.2.2 is the host's loopback, and 10.0.2.3 forwards DNS. `hostfwd=tcp::2222-:22` forwards a host port (on 127.0.0.1 unless an address is given) into the guest.
  - libslirp is not thread-safe, so it gets its own thread, which sleeps in poll(2) on its sockets and a wake-up pipe. Guest frames go into a queue and wake that thread; frames for the guest go into another queue that `Board::tick` drains into the card. When that queue fills from empty, the network thread unparks the emulator thread if it sleeps in HLT and kicks the vCPU, so a frame does not wait for the next timer. The vCPU never blocks on the network.
  - Every register access is a VM exit. Pulling 100 MB from a host web server: Ubuntu 8.04 gets 11 to 12 MB/s with either driver, its CPU saturated, at 3.9 register accesses per frame with 8139too (port I/O) and 0.9 with 8139cp (MMIO); XP gets about 40 MB/s at 3.5 per frame.
- **Hot MMIO and ports, handled inside the VM:** APIC TPR (if we move to an APIC HAL), PIC EOI, and PIT counter reads. Everything else exits to the host.
- **Device threads:** the board and its devices live on the emulation thread. Three devices hand work to threads of their own: the network card (libslirp, above), the AC'97 output (CoreAudio's render thread) and the Radeon's command processor, which runs the ring, the 2D and 3D engines and Metal encoding on a worker so a `CP_RB_WPTR` write returns at once (see [gpu.md](gpu.md#command-processor-thread)). Each wakes the emulation thread and kicks the vCPU when it needs an interrupt raised; the board raises it at its next `tick`.

## Window

The window runs on the main thread, as AppKit requires; the emulation thread publishes each changed frame (`0x00RRGGBB` pixels) in `Shared.frame` and wakes the window's event loop through a winit `EventLoopProxy`. Otherwise the loop sleeps, waking twice a second to put the MIPS rate in the title.

- **Window and input:** winit 0.30 (`crates/slopper/src/window`). There is no menu bar, so every Cmd chord reaches the guest. The window fits itself to each new guest resolution with `request_inner_size`, keeping its top-left corner, unless it is maximized or in full screen; the size is the picture's own, scaled down to 95% x 90% of the screen.
- **Drawing:** a `CAMetalLayer` backs the window's NSView, with `displaySyncEnabled`, the window's backing scale as `contentsScale` and a drawable as large as the view in pixels. A new frame is copied into a BGRA8 texture (the frame's bytes are B, G, R, unused), and a full-screen triangle draws it into a viewport that letterboxes it with black bars. When a whole-number scale fills at least 99% of the space, the picture is drawn at that scale with nearest-neighbour sampling, so an 800x600 desktop in an 800x600-point window on a Retina screen stays pixel-sharp; any other scale is filtered linearly. The layer is tagged sRGB, so colours look the same on wide-gamut displays. Nothing is drawn unless a frame, the size or the scale changed.
- **Keyboard:** winit's physical key codes map to PS/2 set-1 scancodes for a US layout (`keys.rs`), Cmd as the Windows key. macOS key repeat goes to the guest as repeated make codes, as a PS/2 keyboard's typematic repeat would. Caps Lock sends a whole keystroke for each change, because macOS reports it as a toggle. When the window loses focus, every key the guest still sees held is released.
- **Pointer:** absolute cursor positions go through the same letterbox as the drawing to the USB tablet; the guest falls back to PS/2 motion while it has no tablet driver. Wheel line deltas count as detents and trackpad pixel deltas as one detent per 20 points, with the remainder carried.
- **Mouse capture:** Ctrl+Option+G, a click (`--capture-mouse click|auto`) or the `capture` command captures; the toggle, the click and focus loss are decided in `capture::decide`. Capturing warps the cursor to the window's centre, locks it with `CursorGrabMode::Locked` and hides it; motion comes from winit's raw `DeviceEvent::MouseMotion`, so no event tap or Input Monitoring permission is involved. The click that captured is kept from the guest until it is released.

## Remote display (VNC)

`--vnc [ADDR:]PORT` starts an RFB 3.8 server beside the window or `--headless`. It serves the frame that the window shows and `shot` saves. Updates carry only the 32x32 tiles that changed since the client's last update. Each client gets whichever of ZRLE, Tight (lossless: fill, two-colour, palette and full-colour rectangles) and Raw comes first in its SetEncodings list, and keeps its zlib streams for the whole connection. A full 1440x900 XP desktop is about 5.2 MB in Raw and 1.1 to 1.9 MB in ZRLE or Tight, depending on how much of the photographic wallpaper shows. Typing costs about 80 bytes a character in ZRLE. When the guest changes mode, clients that support DesktopSize get the new size; other clients keep their first size, cropped or padded. Keysyms map to PS/2 scancodes for a US layout. The pointer drives the USB tablet, or PS/2 relative motion until the guest's tablet driver runs. `SLOPPER_VNC_PASSWORD` turns on classic VNC authentication (a DES challenge on the first 8 password bytes). It does not encrypt the session.

## Save states

The control socket's `save DIR` writes the running machine to a directory, `load DIR` puts it back, and `--load DIR` starts from one instead of booting. A save takes place between runs of the JIT, when the host holds the whole CPU state, and after the Radeon's command processor thread has run everything posted to it.

- **What a snapshot holds:** the interpreter's `Cpu` (registers, x87, SSE, MSRs, and its software TLB), guest RAM, every board device (PIC, PIT, RTC and CMOS, i8042, UART, FDC, IDE with any PIO or DMA transfer in flight and the ATAPI state, PCI configuration spaces with the PAM registers, local and I/O APIC, the PIIX4 PM function, the battery window, UHCI and the tablet, the stdvga's registers, VBE state and VRAM, AC'97, and the RTL8139's registers and ring pointers), and a description of the machine: RAM size, chipset, display, the PCI functions present and hashes of both firmware images.
- **What it leaves out:** the JIT's translations and shadow page tables, which are dropped on restore and rebuilt as the guest runs; host resources such as the disk file handles, audio output and the network backend, which the restoring machine already has; and libslirp's state. After a restore the network card is cabled to a fresh libslirp: DHCP and new connections work, the lease stays valid since slirp hands out the same address, and connections that were open get a reset when the guest next sends on them.
- **Disks:** saving clones the hard disk image into `DIR/hda.img` with `clonefile(2)` after flushing it. Restoring clones that again into `DIR/hda.run.img` and runs on the copy, so a snapshot can be restored any number of times. Both copies share blocks with the original until written. CD images are read-only, so only the path of the ISO in the drive (or none) is recorded. A restore puts the same ISO back without the guest seeing a disc change; `--cdrom` with `--load` inserts another one, which the guest sees as a change.
- **Format:** `DIR/state.bin` is a magic and a format version, then sections: a tag, the schema hash of what it holds, and a length-prefixed body. Devices implement a small `Snap` trait from `crates/snapshot`, mostly through `#[derive(Snap)]`. It writes fields in order as little-endian integers and loads them back into the existing objects, which keeps the host resources they hold; serde would have needed the opposite, building new objects and moving those resources over. The derive also computes a schema hash from field names and types, so a build whose devices changed shape refuses an old snapshot instead of misreading it. RAM and VRAM are split into 16 MiB chunks that compress and decompress on all cores: a bitmap of the chunk's non-zero 4 KiB pages, then those pages through raw deflate at its fastest level.
- **Time:** the board clock behind the PIT, the local APIC timer, the PM timer, UHCI frames, the AC'97 DMA pacing and the RTL8139 timer carries on from the saved instant, and the TSC is rewritten to its saved value, so the guest sees no time pass while the snapshot sat on disk. The RTC instead follows the host's wall clock (plus whatever offset the guest set), as it always does. Windows keeps its clock from timer interrupts and reads the RTC only at boot, so its clock is behind by the time between save and restore until it next synchronises.
- **Refusals:** `load` refuses a snapshot whose machine differs (pass the same `--ram`, `--no-audio`, `--no-net`, board and firmware as when saving; `--load` takes the snapshot's RAM size unless `--ram` says otherwise). A snapshot of the Radeon holds its registers, PLLs, command processor, 2D engine, I2C and DDC state, its VGA and its 128 MiB of VRAM.

## VM runtime layout

These are the starting decisions for phase 3.

**Guest physical (IPA) map**
- `0 .. 4 GiB` mirrors x86 physical memory. RAM is the same host allocation the board and interpreter use, mapped with `hv_vm_map`. MMIO holes stay unmapped, so an access there is a stage-2 fault: a VM exit that carries the faulting register and access size in ESR (ISV), and the host emulates it.
- `4 GiB and up` holds the runtime: exception vectors, dispatcher, code cache, shadow page tables, CPU state and lookup tables. Guest code cannot form addresses there.

**Stage-1 translation**
- TTBR0 (`T0SZ = 32`, exactly 4 GiB, 4 KiB granule) is the shadow of the x86 page tables. VA equals the x86 linear address, so a guest load is one ARM load at a zero-extended 32-bit address.
- TTBR1 maps the runtime and a linear view of x86 physical memory, for the page walker.
- Shadow entries are filled lazily by the EL1 data-abort handler, which walks the non-PAE x86 page tables itself (4 KiB and 4 MiB pages), sets Accessed and Dirty as the CPU would, and logs each fill. The host takes the log in at the next exit, for its CR3-switch and self-modifying-code bookkeeping. A missing shadow table, an x86 page fault, a write to a code page or a full log go to the host instead. `SLOPPER_JIT_NO_VM_FILL` sends every fill to the host. Pages start read-only so the first write can set the x86 Dirty bit (the hardware does no A/D updates here). Pages holding translated code stay read-only to catch self-modifying code. Linear pages in the 440BX's AGP aperture map straight to the RAM page their GATT entry names, until the guest flushes the chipset's GART TLB (see [gpu.md](gpu.md#bus-agp-on-a-440bx)).
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

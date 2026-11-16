# References

Nothing listed here is committed. Run `scripts/fetch-references.sh` (add `--tests` for the hardware-generated CPU test suites) to clone the source trees into `references/src/`. Spec PDFs go in `references/docs/`. Both directories are gitignored.

**Ground rule:** code copied from, or closely modelled on, any of these projects carries a comment naming the project, the file, the upstream revision, and the original authors where known. Each such use is also listed under [Code we use](#code-we-use).

## Source trees

| Project | License | Can we copy code? | What we want from it |
| --- | --- | --- | --- |
| [SeaBIOS](https://github.com/coreboot/seabios) | LGPL-3.0 | Not copied; we run it as a separate ROM | Firmware we boot. `src/fw/paravirt.c` (fw_cfg), `src/fw/pciinit.c`, `src/hw/` show which devices it expects |
| [QEMU](https://gitlab.com/qemu-project/qemu) | GPL-2.0 (some files GPL-2.0+, LGPL, BSD) | Yes, with attribution; check each file's header | `hw/i386`, `hw/isa`, `hw/ide`, `hw/display/vga*`, `hw/nvram/fw_cfg.c`; `target/i386/tcg` as the baseline we compete with; `target/arm/hvf` for HVF usage |
| [Bochs](https://github.com/bochs-emu/Bochs) | LGPL-2.1+ | Yes, with attribution | The most readable x86 semantics reference: `bochs/cpu/` (flags, exceptions, task switches, x87) |
| [86Box](https://github.com/86Box/86Box) | GPL-2.0+ | Yes, with attribution | Period-accurate chipset and device models (i440FX, PIIX, period VGA cards) |
| [box64](https://github.com/ptitSeb/box64) | MIT | Yes, with attribution | x86 → ARM64 dynarec: flag handling, x87/SSE on NEON (`src/dynarec/arm64`) |
| [FEX](https://github.com/FEX-Emu/FEX) | MIT | Yes, with attribution | x86 → ARM64 JIT with IR, TSO handling, FlagM usage |
| [Berkeley SoftFloat 3](https://github.com/ucb-bar/berkeley-softfloat-3) | BSD-3-Clause | Yes, with attribution | `extF80` for an accurate x87 mode |
| [test386.asm](https://github.com/barotto/test386.asm) | GPL-3.0 | **No**: it would force GPLv3 on the project, which clashes with GPL-2.0-only QEMU code | Assemble it and run the resulting binary as an external test ROM |
| [SingleStepTests/80386](https://github.com/SingleStepTests/80386) | MIT (code); data is public domain | Yes | Per-instruction register/memory before-and-after tests captured from real hardware (real mode for now) |
| [dougallj/applecpu](https://github.com/dougallj/applecpu) | No license | **No**, read only | Firestorm/Icestorm instruction latencies and ports, for codegen decisions |
| [AsahiLinux/docs](https://github.com/AsahiLinux/docs) | CC-BY-3.0 | Text with attribution | Apple CPU implementation-defined registers and features |

## Apple Silicon and Hypervisor.framework

- Hypervisor.framework headers in the macOS SDK (`Hypervisor.framework/Headers/hv_vcpu_types.h`, `hv_vm_config.h`). These are the authoritative source for `HV_SYS_REG_ACTLR_EL1` (EnTSO only), IPA size, the 4 KiB IPA granule, and EL2.
- [Apple developer docs: Hypervisor](https://developer.apple.com/documentation/hypervisor)
- Dougall Johnson, ["Why is Rosetta 2 fast?"](https://dougallj.wordpress.com/2022/11/09/why-is-rosetta-2-fast/): FlagM, inverted carry, AXFLAG, TSO.
- [marcan's notes on Apple TSO/ACTLR](https://gist.github.com/marcan/9ab73ca0614864bea0eea9e953c074d3)
- Apple's Linux patch for guest TSO toggling: [linux-arm-kernel, 2024-05](https://lists.infradead.org/pipermail/linux-arm-kernel/2024-May/926375.html)
- Reverse-engineered notes on Apple's private PF/AF flag extension (APFLG). It is not available under HVF, but useful to understand: [aurora-silicon/linux#868](https://github.com/aurora-silicon/linux/pull/868), [#898](https://github.com/aurora-silicon/linux/pull/898)
- [Hyperpom](https://github.com/impalabs/hyperpom) (Impalabs): an HVF-based fuzzer with 4 KiB stage-1 tables and A/D tracking through faults. Its vectors exit on every exception, which is the design we avoid.

## x86 and PC hardware

`scripts/fetch-docs.sh` downloads these into `references/docs/`. Almost none of them may be redistributed, so they stay local.

| Topic | Document | Notes |
| --- | --- | --- |
| CPU | Intel 64 and IA-32 SDM, combined (325462) | The main reference. Intel lets you download it but not redistribute it |
| CPU | Intel Optimization Reference Manual (248966) | Optional |
| CPU | Intel 80386 Programmer's Reference (1986), i486 Programmer's Reference (1990) | Simpler to read than the SDM for the base ISA; bitsavers scans |
| CPU | [felixcloutier.com/x86](https://www.felixcloutier.com/x86/), [sandpile.org](https://www.sandpile.org/) | Quick per-instruction lookup; sandpile covers opcode maps, CPUID, MSRs |
| Chipset | Intel 440FX PCIset 82441FX/82442FX (290549) | Host bridge, PAM registers (shadow BIOS) |
| Chipset | Intel PIIX3 82371SB (290550), PIIX4 82371AB (290562) | ISA bridge, IDE, USB, PIIX4 power management |
| Chipset | Intel 82093AA I/O APIC (290566), MultiProcessor Spec 1.4 (242016) | |
| Chipset | PCI Local Bus Spec 2.2 | PCI-SIG copyright; bitsavers copy, strictly local |
| Legacy | 8259A PIC, 82C54 PIT, 8237A DMA, MC146818A RTC, PC16550D UART | MIT 6.828 / Stanford Pintos mirrors |
| Legacy | 8742 (8042 family) datasheet, UPI-41A User's Manual | Keyboard controller |
| Firmware | ACPI 1.0b, ACPI 2.0c | Wayback copies of acpi.info |
| Firmware | BIOS Boot Specification 1.01, El Torito 1.0 | |
| Firmware | Ralf Brown's Interrupt List, release 61 | Free to redistribute unmodified; [HTML version](https://www.delorie.com/djgpp/doc/rbinter/) |
| Storage | ATA/ATAPI-6 (T13 1410D r3a), ATA/ATAPI-7 vol 1 and 2 (1532D r4b) | T13 working drafts |
| Storage | SFF-8020i rev 2.6 (ATAPI CD-ROM), SFF-8038i (bus-master IDE DMA) | The SFF-8020i copy comes from a forum upload, but the content checks out |
| Video | VESA BIOS Extension 3.0 | |
| Video | [FreeVGA](http://www.osdever.net/FreeVGA/home.htm) | VGA registers; no free IBM VGA technical reference exists |
| Host | Arm Architecture Reference Manual, A-profile (DDI 0487 M.d) | 129 MB |
| General | [OSDev wiki](https://wiki.osdev.org/) | Pages on PIC, PIT, CMOS, PS/2, APIC, PCI, ATA, ATAPI, VGA |

Not found as free, trustworthy copies: PCI BIOS Spec 2.1, PCI Local Bus 2.3, the IBM VGA Technical Reference.

## Code we use

None yet.

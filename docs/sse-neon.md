# SSE/SSE2 on NEON

How the JIT should run the guest's SSE and SSE2 code (the Pentium 4 CPUID has no SSE3), and what on the M4 makes that bit-exact. Nothing here is implemented yet except MOVNTPS. The facts marked *verified* were checked with small native programs on an M4 Pro at EL0; nothing has been checked inside the HVF guest yet.

Sources: Arm ARM DDI 0487 M.d (page labels as printed), Intel SDM 325462 Vol. 1, and the FEX-Emu, box64, QEMU and Bochs sources under `references/src`. Where an approach comes from one of those projects, the project and file are named.

## FPCR settings

Set FPCR to:

| Bit | Value |
|---|---|
| AH (1) | 1 |
| NEP (2) | 1 |
| FIZ (0) | MXCSR.DAZ |
| FZ (24) | MXCSR.FTZ, but 0 while UM is unmasked (x86 ignores FTZ then) |
| RMode (23:22) | MXCSR.RC (14:13) with the two bits swapped (`rbit`), as in FEX `JIT/MiscOps.cpp` and box64 `dynarec_arm64_helper.c` |
| DN (25) | 0, or NaNs stop propagating |

With FEAT_AFP's AH=1 (Arm ARM A1.5.6 to A1.5.9, pseudocode in J1):

- **NaNs.** With two NaN inputs the result is the `Vn` operand, quieted, which is x86's "first source" rule (SDM 4.8.3.5, Table 4-8). The default NaN gets the sign of AH, so invalid operations give `0xFFC00000` / `0xFFF8000000000000`, the x86 QNaN indefinite. *Verified.*
- **MAXPS/MINPS.** FMAX/FMIN return the second operand when either input is NaN (an SNaN is not even quieted), when both are zero, or when they are equal, and raise Invalid on any NaN. That is exactly MAXPS/MINPS. *Verified.* FMAXNM/FMINNM do not get this treatment; don't use them.
- **Denormals.** FZ flushes outputs only, tested after rounding, and sets UFC+IXC: x86 FTZ. FIZ flushes inputs without setting IDC: x86 DAZ. Underflow is detected after rounding, as on x86. *Verified.*
- **Flags.** FPSR maps one-to-one onto MXCSR: IOC→IE, IDC→DE, DZC→ZE, OFC→OE, UFC→UE, IXC→PE; mask out QC. This M4 lets a QNaN suppress IDC, matching x86 exception precedence. *Verified.*

**NEP** (C5-980) keeps the upper lanes of scalar results. The source of those lanes is `Vn` for two-input arithmetic (FADD, FMUL, FMAX, ...), `Vm` for scalar compares (FCMEQ, FCMGT, FCMGE), and `Vd` for one-input operations (FSQRT, FCVT, SCVTF, FRINT*, FRECPE). *Verified.* So ADDSS and friends are one instruction, and CMPSS needs its operands reversed so the destination is `Vm` (CMPLTSS x1,x2 → `fcmgt s1, s2, s1`). FEX does the same in `JIT/VectorOps.cpp` (VFScalarOperation).

## Conversions and compares

- **Float to int.** FCVTZS alone saturates, giving 0x7FFFFFFF or 0 where x86 wants the integer indefinite 0x80000000. Precede it with FEAT_FRINTTS: `frint32z` for the truncating forms (CVTT*), `frint32x` (current rounding mode) for the others. FRINT32 returns -2^31 for NaN and out-of-range inputs, which FCVTZS turns into 0x80000000. *Verified.* FEX (`OpcodeDispatcher/Vector.cpp`, CVTFPR_To_GPRImpl) and box64 (`dynarec_arm64_f30f.c`) emit the same pair. For the PD forms, narrow with `xtn`, which also zeroes the upper half as x86 requires.
- **COMISS/UCOMISS.** `fcmpe` (COMISS, Invalid on QNaN) or `fcmp` (UCOMISS), then `cset w27, vc` for PF, then FEAT_FlagM2's `axflag`. AXFLAG (C6.2.33) leaves NZCV = 0 ZF !CF 0, which is already our convention (N=SF, Z=ZF, C=NOT CF, V=OF), so no `cfinv` is needed. PF has to be taken before AXFLAG, because "unordered" has no single condition code afterwards. AF must read as 0. FEX's ComissFlags does the same.
- **RCPPS/RSQRTPS.** With FEAT_RPRES and AH=1, FRECPE/FRSQRTE give 12-bit estimates, flush denormals, ignore the rounding mode and raise no flags. The corner cases match Intel's description. Bochs's tables (`cpu/sse_rcp.cc`, by Stanislav Shwartsman) are bit-identical to Arm's formula, within Intel's 1.5×2^-12 bound; whether a real Pentium 4 agrees is unknown. The interpreter currently returns exact results, so switch it to the Arm estimate (Arm ARM J1, RecipEstimate and RecipSqrtEstimate, with Bochs as a cross-check) before the JIT uses FRECPE, or the two will disagree under `PCSLOP_JIT_VERIFY`.

## Plan

**State.** Keep XMM0-7 in fixed V registers (v0-v7, temps in v24-v31). Spill and fill them in `store_guest`/`load_guest` with `stp q`/`ldp q`. Set FPCR/FPSR from MXCSR in `load_guest` with `msr`, and fold FPSR back into MXCSR on exit, or keep FPSR as the live flags until STMXCSR/FXSAVE. Read FPCR back once inside the VM to confirm AH/NEP/FIZ stick.

**One instruction each.** Moves (`ldr/str q`, `ins`, `fmov`), logic (`and/orr/eor/bic`), unpacks (`zip1/zip2`), packed and scalar FP arithmetic, MAX/MIN, the EQ/LT/LE compares, COMISS, the conversions above, and most SSE2 integer operations (`add/sub`, saturating `sqadd/uqadd`, `cmeq/cmgt`, `urhadd`, `smull`+`uzp2` for PMULHW, `sqxtn`/`sqxtun` for the packs). MOVAPS-style alignment checks take the #GP path like the MOVNTPS translation.

**A few instructions.** The NEQ/NLT/NLE/ORD/UNORD compares, shifts by 16 or more or by a register count, PSADBW (`uabd` + `uaddlv`), PMOVMSKB/MOVMSKPS/MOVMSKPD, two-source SHUFPS and PSHUFD (`tbl` with a constant, or `dup`/`ext`/`rev64` special cases).

**Interpreter for now.** MASKMOVDQU/MASKMOVQ, LDMXCSR/STMXCSR/FXSAVE/FXRSTOR, MMX (it aliases the x87 state), x87 (80-bit, different NaN rules), 16-byte stores that straddle a page, and any SSE FP operation while an MXCSR exception is unmasked. XP keeps MXCSR at 0x1F80, so that last case is rare. The FPCR trap enables do trap on this M4 (FEAT_TRAPFPE, not listed by sysctl; *verified* at EL0), so unmasked exceptions could later exit to the interpreter on ESR EC 0x2C instead.

**Not useful here.** FHM, BF16, FP16, dot product and I8MM have no SSE2 counterpart. SME2 has no FEAT_SME_FA64, so NEON is illegal in streaming mode, and NEP and the trap enables are ignored there.

**Tests to add,** run in the interpreter and under `PCSLOP_JIT_VERIFY`: NaN ordering (QNaN destination with SNaN source), MAXPS with NaN and signed zeros, results that round up to the smallest normal, FTZ against DAZ flags, CVTT* on NaN, ±inf and ±2^31, and COMISS on all four relations with QNaN and SNaN.

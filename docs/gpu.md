# GPU

Plan for 2D and 3D acceleration. Built so far: the 440BX board with AGP, the card's PCI function, the real video BIOS's POST, VGA and VBE modes, DDC with our EDID, and the primary CRTC with its cursor. Run it with `--gpu radeon --vga-rom ROM`, and add `--gpu-trace PATH` to log every register access.

## Target: ATI Radeon 9700 (R300)

We emulate a real ATI card so that guests run their stock drivers: the open-source radeon stack on Linux and Catalyst on Windows XP. We emulate the card's interface (MMIO registers, the command processor, the GART) at a low level, and draw everything it is asked to draw with Metal. We do not emulate the GPU's rendering on the CPU.

Why this card:

- **Vendor documentation.** AMD published register references and programming guides for R3xx to R5xx through X.org in 2008. For Nvidia cards of the same era there are only reverse-engineered notes.
- **Complete open drivers.** The Linux `radeon` DRM driver, Mesa's `r300` gallium driver and xf86-video-ati cover every block we need. When a guest driver misbehaves on Linux, we can read the exact code path.
- **DirectX 9 and Shader Model 2.0.** Enough for dxdiag and games of the XP era. DXT textures map directly to Metal's BC formats.
- **Same era as our CPU.** It is from 2002, like the Northwood Pentium 4 we report in CPUID. Catalyst 9.3 is the last release for R300 to R500 on XP; confirm against its INF.
- **Its speed does not matter.** We only borrow its feature set. Metal does the drawing.

R300 and R420 share the command processor, 2D engine, memory controller and legacy display. R5xx replaces the display with AVIVO. A later R4xx card would reuse almost everything.

Rejected:

- **GeForce FX 5200 PCI (NV34):** the previous plan. It fits our PCI-only board, but its 3D engine (Rankine) is only documented through Mesa's encoders and envytools stubs, and fixed-function state may use NV2x register combiners, which nobody documents fully.

## Milestones

1. **2D under Ubuntu 8.04 (Hardy).** Linux is a more lenient guest than XP, and every driver is open source:
   - kernel 2.6.24 with the UMS `radeon` DRM in `drivers/char/drm/`;
   - xf86-video-ati 6.8.0 for mode setting and 2D acceleration (XAA or EXA, over MMIO or the CP);
   - Mesa 7.0's classic r300 DRI driver, which gives `glxgears` as an early 3D test.

   Check these versions against Hardy's package lists before relying on them.
2. **Windows XP with Catalyst, 2D only.** The desktop drawn through GDI acceleration, at the resolutions and depths Catalyst offers.
3. **3D.** dxdiag's Direct3D tests first, then games.

## Bus: AGP on a 440BX

The 9700 is an AGP card: its ROM's build string reads `R300AGP`. We put it on AGP, behind an Intel 440BX, because Catalyst on XP is likely to refuse an R300 without AGP, and AGP costs little:

- `--board bx` swaps the 82441FX for the 82443BX host bridge (8086:7190) with its AGP capability and the GART aperture registers (APBASE, APSIZE, AGPCTRL, ATTBASE). Its PCI-to-PCI bridge (8086:7191) at 00:01.0 leads to AGP bus 1, where the card sits with AGP 3.0 and power management capabilities. The PIIX3 moves to 00:07, as on real 440BX boards. `--gpu radeon` implies `--board bx`; the 440FX stays the default, so existing XP installs see no new hardware.
- Like a 440BX board's firmware, the board sets APSIZE to 64 MiB before SeaBIOS sizes APBASE.
- CPU accesses to the aperture go through the translation table at ATTBASE, which Linux's `intel-agp` fills.
- SeaBIOS 1.17 only knows the 82441FX and Q35, so `nix/seabios-i440bx.patch` lets it treat the 82443BX like the 82441FX (same PAM registers) and fixes its static $PIR table for the PIIX3 at device 7 and the AGP slot.

Linux 2.6.24 finds it as `agpgart: Detected an Intel 440BX Chipset`, with a 64M aperture, and puts the host and the card into AGP 2x.

Windows XP on `--board bx` with the stdvga reaches the desktop, then reports new devices and asks for a restart.

## Pieces

In the order we need them:

1. **PCI device.**
   - Vendor `1002`, device `4E44`. The supplied ROM's PCI data structure names `4E44` (Radeon 9700 Pro), though its file name says non-Pro (`4E45`). The driver reads config space, so we pick the ID that matches the ROM.
   - BAR0: VRAM aperture, prefetchable, 128 MiB.
   - BAR1: 256 bytes of I/O ports, which index into the MMIO registers.
   - BAR2: 64 KiB of MMIO registers.
   - An expansion ROM.
2. **Video BIOS.** The card uses ATI's legacy COMBIOS (it has no AtomBIOS tables).
   - First choice: run the real ROM. It is ATI's copyrighted firmware, so the user supplies it; it is never committed, shipped or put in a published Nix store path. Its POST programs the PLLs and memory controller and sets VGA modes through ATI's extended VGA registers, so we emulate enough for it to finish.
   - Fallback: SeaVGABIOS for VGA and VBE, plus our own COMBIOS tables (connectors, PLL limits, memory setup) for the drivers to read. `radeon_combios.c` in Linux describes these tables.
3. **Core registers:**
   - indexed PLL registers (`CLOCK_CNTL_INDEX`/`CLOCK_CNTL_DATA`);
   - memory controller and straps: `CONFIG_MEMSIZE`, `CONFIG_APER_SIZE`, `MC_FB_LOCATION`;
   - `RBBM_STATUS` and the engine idle bits that drivers poll;
   - interrupts (`GEN_INT_CNTL`, `GEN_INT_STATUS`): vblank and software interrupts;
   - scratch registers and their writeback to system memory;
   - surface registers that describe tiled regions of VRAM.
4. **Display:**
   - VGA, then the legacy CRTC and CRTC2 registers: timings, scanout offset and pitch, colour depth;
   - the palette, the DAC and TMDS (DVI) outputs, the hardware cursor;
   - DDC over the GPIO I2C lines, so drivers read an EDID that we generate;
   - TV-out stays unconnected;
   - scanout is a Metal texture that the frontend shows. It may be macro- or micro-tiled.
5. **Command processor (CP).**
   - The ring buffer (`CP_RB_BASE`, `CP_RB_RPTR`, `CP_RB_WPTR`), read-pointer writeback, and indirect buffers.
   - PM4 packets of types 0 to 3, interpreted in Rust. Drivers upload CP microcode; we accept it and ignore it.
   - Drivers can also program the engines through MMIO without the CP. Both paths feed the same register writes.
6. **GART.** The PCI GART table, or the AGP aperture if we add AGP. We follow it to read guest memory for rings, indirect buffers, vertex data and textures.
7. **2D engine.** Solid fills, screen-to-screen blits with ROP3, host data uploads, monochrome colour expansion for text, and scaled blits. These come before 3D because they give us a usable desktop. They run on the CPU at first and move to Metal later.
8. **3D engine on Metal.**
   - The vertex shader unit (PVS) and fragment shader unit (US): decode their programs and translate them to MSL.
   - The rasterizer setup (RS), texture units, blending (RB3D) and depth/stencil (ZB), mapped to Metal pipeline, depth-stencil and sampler objects, each cached by its state.
   - Fast Z clears and Z compression: track the clear state per surface and resolve it ourselves.
9. **Sync.** Drivers wait on scratch register writebacks, `WAIT_UNTIL`, cache flushes and software interrupts.

## What the video BIOS's POST needs

The supplied ROM (BIOS B8480.002, 2002/12/16) POSTs, sets text mode, and sets VBE modes. A trace of its POST is kept outside the repository. What it needs:

- **Access path:** BAR1 only. Registers above 0xFF go through MM_INDEX/MM_DATA. The ROM uses no extended VGA index registers. It reads its PCI configuration through the MMIO mirror at 0xF00.
- **Memory controller:** writes `MEM_CNTL`, `MEM_TIMING_CNTL`, the R300 indirect MC registers (`MC_IND_INDEX`/`MC_IND_DATA`), byte-wise `MEM_SDRAM_MODE_REG` sequences, `MC_FB_LOCATION` = 0x1FFF0000 and `MC_AGP_LOCATION`. It writes `CONFIG_MEMSIZE` = 128 MiB itself and runs no VRAM sizing probe. It reads `MC_STATUS` and goes on whether or not the idle bits are set.
- **PLLs:** programs MCLK, SCLK, MPLL, SPLL and the pixel PLL through CLOCK_CNTL_INDEX/DATA with PLL_WR_EN. It needs no lock bits. Drivers also poll `PPLL_REF_DIV`'s ATOMIC_UPDATE bit, which must clear.
- **Engine status:** POST polls no engine status bits.
- **Output detection:** DAC load comparison (`DAC_CNTL.DAC_CMP_EN`, read back as `DAC_CMP_OUTPUT`), the TV DAC comparator, and I2C bit-banged over `GPIO_DVI_DDC` and `GPIO_VGA_DDC`. Results go into `BIOS_0..7_SCRATCH`.
- **Text mode:** standard VGA registers.
- **VBE modes:** the extended CRTC (`CRTC_H/V_TOTAL_DISP`, `CRTC_PITCH`, `CRTC_OFFSET`, `CRTC_GEN_CNTL.CRTC_EXT_DISP_EN`) plus the pixel PLL. The A0000 window is banked through `MEM_VGA_WP_SEL`/`MEM_VGA_RP_SEL` in 8 KiB pages, or 32 KiB with `VGA_ATI_LINEAR`, as in 86Box's Rage 128 model.
- **VGA shadowing:** VGA CRTC timing writes must show up in the extended CRTC registers. The ROM writes CR12 and checks `CRTC_V_TOTAL_DISP`. If that fails, it sets `BIOS_0_SCRATCH` bit 7 and never clears `CRTC_EXT_DISP_EN` again, so a later text mode shows nothing.

## Linux drivers (Ubuntu 8.04.4)

- **radeonfb:** finds the monitor through our EDID and drives a 1024x768 console. Its fills and scrolls use the 2D engine (`DP_GUI_MASTER_CNTL`, `DP_CNTL`, `SRC_Y_X`, `DST_Y_X`, `DST_HEIGHT_WIDTH`, `DST_WIDTH_HEIGHT`, waiting on `RB2D_DSTCACHE_CTLSTAT` and `RBBM_STATUS`), so scrolling does not work until that exists. `video=radeonfb:noaccel` avoids it.
- **X with the radeon driver (xf86-video-ati 6.8.0):** reads the ROM's connector table and our EDID, then sets 1024x768x24 with the ARGB hardware cursor. With `NoAccel` and DRI off, the GNOME desktop draws correctly. With the defaults (XAA over the CP, DRI, colour tiling), X sets the mode but draws nothing, and then waits forever on `CP_RB_RPTR` and `GUI_SCRATCH_REG3`.
- **The radeon DRM:** initializes, loads the R300 microcode through `CP_ME_RAM_*`, and reports `writeback test failed` (no scratch writeback to system memory).

What the drivers want next, in order:

1. **CP:** the ring (`CP_RB_CNTL`, `CP_RB_BASE`, `CP_RB_RPTR`/`WPTR`, `CP_RB_RPTR_ADDR` writeback), microcode upload, and the scratch registers with their writeback (`SCRATCH_ADDR`, `SCRATCH_UMSK`, `GUI_SCRATCH_REG0..7`).
2. **2D engine:** the registers listed under radeonfb, plus the brush and source colours, `DP_DATATYPE`, `DP_WRITE_MASK`, `DEFAULT_OFFSET`/`SC_*` clipping, and the 2D and 3D destination cache flushes.
3. **Colour tiling:** `CRTC_OFFSET_CNTL` tiling, `SURFACE_CNTL` and the eight `SURFACEn_*` ranges, which X sets up by default.
4. **Interrupts:** `GEN_INT_CNTL`/`GEN_INT_STATUS` on the card's INTA (IRQ 10 here).
5. **Reads only, already fine as stored values:** CRTC2, the flat panel, LVDS and TMDS, the TV encoder, the overlay (`OV0_*`), capture (`CAP0/1_*`), VIP and `AGP_BASE`.

## Open questions

- **Catalyst on the 440BX.** See [Bus](#bus-agp-on-a-440bx). Check early, with Catalyst 2D, that it accepts our AGP setup before building much of the CP.
- **Fixed-function Direct3D.** We expect Catalyst to turn fixed-function state into vertex and fragment programs, since R300 runs all transform, lighting and texture combining through its shader units. Logging what the driver sends confirms it.
- **Sync with the guest.** If we write fences only when the Metal command buffer completes, every fence costs a round trip to the GPU. If we write them early, the guest can lock a render target and read stale pixels. A middle path: write early, and when the guest reads a surface, wait for pending Metal work and read the surface back.

## Apple Silicon and Metal

- **Unified memory:** guest VRAM (BAR0) can be a no-copy `MTLBuffer` over the same host memory. Linear surfaces, vertex buffers and index buffers then need no copy.
- **Dirty tracking:** guest CPU writes to VRAM that has a Metal copy, such as tiled textures, are found with HVF page protection. The pages are mapped read-only, and the first write faults and marks the copy stale.
- **Formats:**
  - DXT1, DXT3 and DXT5 map to BC1 to BC3, which Apple Silicon samples natively.
  - ATI's macro- and micro-tiled layouts are converted.
- **Gaps:**
  - No D24S8 on Apple Silicon, so we use `Depth32Float_Stencil8`. Depth bias and depth readback need conversion.
  - No quads or quad strips: we split them into triangles. Point sprites need care.
  - Alpha test and fog run in the fragment shader.
- **Precision:** R300's fragment shader computes in fp24. We run everything as fp32, which matches or beats the hardware.

## References

These are listed with their licenses in [references.md](references.md).

- **AMD documentation** (X.org):
  - *R3xx 3D Registers*: the 3D engine registers, including the PVS and US instruction formats.
  - *R5xx Acceleration* v1.5: the CP, PM4 packets and the 2D engine, which R3xx shares.
- **Linux, `drivers/gpu/drm/radeon/`:**
  - `r100.c`, `r300.c`, `r100_track.h`: the command-stream checker, which lists the packets and registers a driver may send;
  - `radeon_combios.c`: COMBIOS tables;
  - `radeon_legacy_crtc.c`, `radeon_legacy_encoders.c`: legacy mode setting;
  - `r300_reg.h`, `radeon_reg.h`: register definitions;
  - `radeon_gart.c`: the PCI GART.
- **Linux 2.6.24, `drivers/char/drm/`:** the UMS driver that Ubuntu 8.04 runs: `radeon_cp.c`, `r300_cmdbuf.c`.
- **Mesa, `src/gallium/drivers/r300/`:**
  - `compiler/`: the PVS and US encoders, which are our decoder spec;
  - `r300_state.c`, `r300_emit.c`: how state maps to registers;
  - `r300_texture.c`, `r300_texture_desc.c`: formats and tiling.
- **xf86-video-ati 6.14.6** (the last release with UMS; Ubuntu 8.04 ships 6.8.0): mode setting, BIOS parsing, XAA and EXA 2D acceleration.
- **QEMU, `hw/display/ati*.c`:** an emulated Rage 128 and Radeon RV100 (2D only). The closest existing code for the register file, the 2D engine and VGA integration.
- **OpenGL `ARB_vertex_program` and `ARB_fragment_program`:** the semantics R300's shader units were designed around.

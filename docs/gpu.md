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

Our EDID prefers 1440x900 at 60 Hz (CVT reduced blanking, 88.75 MHz) and also lists 640x480, 800x600, 1024x768, 1280x800 and 1280x1024.

- **radeonfb:** drives a 1440x900 console. Its fills and scrolls run on our 2D engine.
- **The radeon DRM:** loads the R300 microcode, which we ignore, and its writeback test succeeds. The ring and the indirect buffers sit in the AGP aperture.
- **X (xf86-video-ati 6.8.0) with an unmodified xorg.conf:** 1440x900x24, DRI on, XAA over the CP, colour tiling on (the driver turns Render acceleration off on R300). GNOME draws correctly: menus, terminal scrolling and window moves. `glxinfo` reports direct rendering with Mesa 7.0.3's R300 driver.
- **Compiz:** when the session picks Compiz, the whole desktop goes through the 3D engine (vertex and fragment programs into a back buffer), which we do not draw, so the screen stays black. Hardy's compiz-manager skips Compiz with "Found laptop using ati driver" when the guest has a battery, which `--battery` gives it by default.

What we built for this:

- **CP:** the ring buffer in the memory controller's address space, read when the guest writes `CP_RB_WPTR` (in bus-master mode, `CP_CSQ_CNTL` 2 or 4), with `CP_RB_RPTR_ADDR` writeback; indirect buffers through `CP_IB_BASE`/`CP_IB_BUFSZ`; packets of types 0 to 3; scratch registers written back to `SCRATCH_ADDR` under `SCRATCH_UMSK`. Addresses go to VRAM (`MC_FB_LOCATION`), the AGP aperture (`MC_AGP_LOCATION` and `AGP_BASE`, then the 440BX's GATT), the PCI GART (`AIC_*`) or the bus. Everything runs at once, so engine status, cache flushes and `WAIT_UNTIL` always read idle. 3D packets are skipped.
- **2D engine:** on the CPU, from MMIO writes and the type-3 2D packets (`PAINT`, `PAINT_MULTI`, `BITBLT`, `BITBLT_MULTI`, `HOSTDATA_BLT`, `POLYLINE`, `POLYSCANLINES`): fills, blits with any ROP3 that overlap either way, host data with mono colour expansion (opaque and transparent), 8x8 mono and colour brushes, lines, the scissor, `DP_WRITE_MASK`, source colour compare, and 8, 16 and 32 bpp.
- **Interrupts:** vblank and the software interrupt (`SW_INT_FIRE`) in `GEN_INT_STATUS`, on INTA.
- **Colour tiling:** we keep every surface linear in VRAM. The 2D engine ignores the tiling bits in the pitch-offset registers, the CRTC scans out linearly (with `R300_CRTC_TILE_X0_Y0` as the start when `R300_CRTC_X_Y_MODE_EN` is set), and CPU accesses through the `SURFACE_*` ranges are linear on real hardware anyway. Only the 3D engine and textures will see the real layouts, and those go to Metal.
- **Hardware I2C engine:** `I2C_CNTL_0`, `I2C_CNTL_1` and `I2C_DATA` run whole transfers to the same EDID.

## Windows XP's built-in ATI driver

On an ACPI-HAL XP with `--gpu radeon`, XP binds its own driver, "RADEON 9700 PRO Family (Microsoft Corporation)", and asks for a restart. After that it reaches a 1440x900 desktop from our EDID. Fills, blits, `TRANS_BITBLT` and text (`NEXTCHAR`) draw through the CP; icons, caption buttons, title text and theme bitmaps are missing, because they go through `3D_DRAW_IMMD` (AlphaBlend on the 3D engine), which we skip.

What it needs on the way:

- `MC_STATUS` memory power-up after it resets the SDRAM.
- EDID by bit-banging `GPIO_VGA_DDC` and `GPIO_DVI_DDC` (the hardware I2C engine is there too).
- An engine-idle wait on three undocumented registers: `0x790` (low five bits 2), `0x7B0` (low five bits 8) and `0xA10` (low half 0x3333). We report those values.
- The ring in `CP_CSQ_CNTL` mode 8 (primary and indirect bus-master), through the PCI GART (`AIC_*`), with scratch writeback.
- Fences through `CP_RESYNC_ADDR`/`CP_RESYNC_DATA`, which write a scratch register once the pipeline drains. Without them it decides the CP hung, dumps the queue through `CP_CSQ_ADDR`/`CP_CSQ_DATA` and bugchecks.

Its hottest MMIO polls: `CP_RB_RPTR` (about 900,000 reads while it waited on a fence that never came), then the DDC GPIOs (about 15,000 each).

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

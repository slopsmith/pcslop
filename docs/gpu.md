# GPU

Plan for 2D and 3D acceleration. Nothing here is built yet.

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

## Bus: PCI or AGP

The 9700 is an AGP card: its ROM's build string reads `R300AGP`. Our i440FX board has no AGP. Options:

- **Put it on the PCI bus with no AGP capability.** The Linux driver then uses its PCI GART, a page table in system RAM that the card walks. This is enough for milestone 1. Whether Catalyst accepts an R300 without AGP is unknown.
- **Add AGP.** Swap the host bridge for an Intel 440BX (82443BX), which has an AGP port and a GART aperture. This touches the chipset, SeaBIOS's PCI setup and the ACPI tables.

Start with PCI. Move to the 440BX if Catalyst refuses to work without AGP.

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

## Open questions

- **Catalyst without AGP.** See [Bus](#bus-pci-or-agp). Answer it early, with Catalyst 2D, before building much of the CP.
- **Fixed-function Direct3D.** We expect Catalyst to turn fixed-function state into vertex and fragment programs, since R300 runs all transform, lighting and texture combining through its shader units. Logging what the driver sends confirms it.
- **Sync with the guest.** If we write fences only when the Metal command buffer completes, every fence costs a round trip to the GPU. If we write them early, the guest can lock a render target and read stale pixels. A middle path: write early, and when the guest reads a surface, wait for pending Metal work and read the surface back.
- **What the ROM's POST touches.** Tracing the real ROM's register accesses tells us how much of the memory controller and PLLs must behave like hardware.

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

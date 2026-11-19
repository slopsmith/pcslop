# GPU

Plan for 2D and 3D acceleration. Built so far: the 440BX board with AGP, the card's PCI function, the real video BIOS's POST, VGA and VBE modes, DDC with our EDID, the primary CRTC with its cursor, the command processor, the 2D engine, and the 3D engine drawing with Metal or on the CPU (see [3D engine](#3d-engine)). Run it with `--gpu radeon --vga-rom ROM`, and add `--gpu-trace PATH` to log every register access.

## Target: ATI Radeon 9700 (R300)

We emulate a real ATI card so that guests run their stock drivers: the open-source radeon stack on Linux and Catalyst on Windows XP. We emulate the card's interface (MMIO registers, the command processor, the GART) at a low level, and draw everything it is asked to draw with Metal. We do not emulate the GPU's rendering on the CPU.

Why this card:

- **Vendor documentation.** AMD published register references and programming guides for R3xx to R5xx through X.org in 2008. For Nvidia cards of the same era there are only reverse-engineered notes.
- **Complete open drivers.** The Linux `radeon` DRM driver, Mesa's `r300` gallium driver and xf86-video-ati cover every block we need. When a guest driver misbehaves on Linux, we can read the exact code path.
- **DirectX 9 and Shader Model 2.0.** Enough for dxdiag and games of the XP era. DXT textures map directly to Metal's BC formats.
- **Same era as our CPU.** It is from 2002, like the Northwood Pentium 4 we report in CPUID. Catalyst 10.2 legacy is the last release for R300 to R500 on XP; its INF lists the 9700 Pro (`4E44`).
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
- CPU accesses to the aperture go through the translation table (GATT) at ATTBASE, which Linux's `intel-agp` and XP's `agp440.sys` fill. Direct3D games write their dynamic vertex and index buffers through it every frame, so the JIT maps aperture pages straight to the RAM their GATT entries point at: when a shadow fill finds a guest-physical address in the aperture with a valid entry, it maps the linear page to that RAM page, as for ordinary RAM, self-modifying code watch included. The in-VM walker leaves every address above RAM to the host, which installs these mappings; later accesses never leave the VM. Invalid entries keep the slow path (reads all ones, writes dropped).
  - The 82443BX caches GATT entries in its GTLB, and software must flush it by clearing AGPCTRL.GTLB_EN after changing them (datasheet 3.3.34); Linux's `intel_tlbflush` does so after every update. The JIT relies on the same contract: any write to the host bridge's configuration space (a GTLB flush, or APBASE, APSIZE, ATTBASE or NBXCFG changing) bumps `Bus::aperture_epoch`, and the JIT then drops its shadow entries for addresses above RAM so they fill again. While GTLB_EN is clear nothing is cached and every access is emulated. Sniper, Half-Life, dxdiag and glxgears ran without a single drop.
  - In Sniper: Path of Vengeance this took MMIO exits from about 490K/s to about 1.3K/s (the rest are Radeon register accesses) and the frame rate from 42 to 164 flips/s.
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
8. **3D engine on Metal.** Fragment work runs on Metal, vertex work on the CPU; see [3D engine](#3d-engine).
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
- **glxgears:** draws its lit gears through Mesa 7.0.3's r300 driver with hardware TCL, at about 114 frames per second with Metal and 48 on the CPU path.
- **Compiz:** composites the desktop through the 3D engine: windows, decorations and the desktop wall's slide between workspaces draw. Hardy's compiz-manager skips Compiz with "Found laptop using ati driver" whenever the guest has ACPI power devices, even with `--battery off`; `SKIP_CHECKS=yes compiz --replace` starts it anyway.

What we built for this:

- **CP:** the ring buffer in the memory controller's address space, read when the guest writes `CP_RB_WPTR` (in bus-master mode, `CP_CSQ_CNTL` 2 or 4), with `CP_RB_RPTR_ADDR` writeback; indirect buffers through `CP_IB_BASE`/`CP_IB_BUFSZ`; packets of types 0 to 3; scratch registers written back to `SCRATCH_ADDR` under `SCRATCH_UMSK`. Addresses go to VRAM (`MC_FB_LOCATION`), the AGP aperture (`MC_AGP_LOCATION` and `AGP_BASE`, then the 440BX's GATT), the PCI GART (`AIC_*`) or the bus. The ring runs on a thread of its own; see [Command processor thread](#command-processor-thread).
- **2D engine:** on the CPU, from MMIO writes and the type-3 2D packets, on surfaces in VRAM or in system memory behind the AGP aperture or the PCI GART (`PAINT`, `PAINT_MULTI`, `BITBLT`, `BITBLT_MULTI`, `HOSTDATA_BLT`, `POLYLINE`, `POLYSCANLINES`): fills, blits with any ROP3 that overlap either way, host data with mono colour expansion (opaque and transparent), 8x8 mono and colour brushes, lines, the scissor, `DP_WRITE_MASK`, source colour compare, and 8, 16 and 32 bpp.
- **Interrupts:** vblank and the software interrupt (`SW_INT_FIRE`) in `GEN_INT_STATUS`, on INTA.
- **Colour tiling:** we keep every surface linear in VRAM. The 2D engine ignores the tiling bits in the pitch-offset registers, the CRTC scans out linearly (with `R300_CRTC_TILE_X0_Y0` as the start when `R300_CRTC_X_Y_MODE_EN` is set), and CPU accesses through the `SURFACE_*` ranges are linear on real hardware anyway. Only the 3D engine and textures will see the real layouts, and those go to Metal.
- **Hardware I2C engine:** `I2C_CNTL_0`, `I2C_CNTL_1` and `I2C_DATA` run whole transfers to the same EDID.

## Windows XP's built-in ATI driver

On an ACPI-HAL XP with `--gpu radeon`, XP binds its own driver, "RADEON 9700 PRO Family (Microsoft Corporation)", and asks for a restart. After that it reaches a 1440x900 desktop from our EDID and draws it completely: fills, blits, `TRANS_BITBLT` and text (`NEXTCHAR`) through the 2D engine, and icons, caption buttons, the Start button and the Start menu through the 3D engine. dxdiag reports DirectDraw and Direct3D acceleration (DDI 9).

What it needs on the way:

- `MC_STATUS` memory power-up after it resets the SDRAM.
- EDID by bit-banging `GPIO_VGA_DDC` and `GPIO_DVI_DDC` (the hardware I2C engine is there too).
- An engine-idle wait on three undocumented registers: `0x790` (low five bits 2), `0x7B0` (low five bits 8) and `0xA10` (low half 0x3333). We report those values.
- The ring in `CP_CSQ_CNTL` mode 8 (primary and indirect bus-master), through the PCI GART (`AIC_*`), with scratch writeback.
- Fences through `CP_RESYNC_ADDR`/`CP_RESYNC_DATA`, which write a scratch register once the pipeline drains. Without them it decides the CP hung, dumps the queue through `CP_CSQ_ADDR`/`CP_CSQ_DATA` and bugchecks.
- Blits into system memory: it reads surfaces back, such as theme bitmaps before it alpha-blends them, by blitting them into PCI GART pages and reading those with the CPU.
- AlphaBlend as a point sprite (`3D_DRAW_IMMD`, one vertex, `GA_POINT_SIZE` and stuffed texture coordinates) with a one-instruction fragment program and a premultiplied blend, clipped to the destination rectangle with `SC_CLIP_0` and `SC_CLIP_RULE`. Per-pixel alpha onto the screen uses a quad with two textures, the source and the destination itself, blended in the fragment program.

dxdiag's DirectDraw tests and its Direct3D 7, 8 and 9 tests pass. They needed:

- **IDCT idle.** Before the test's mode switch the driver idles every engine, including the MPEG IDCT block at `0x1F80`-`0x1FBC`: it reads `0x1F84`, `0x1F9C` and `0x1FA8`, then waits for bit 31 of `0x1FB4`. We report that bit set.
- **Packets the CPU path got wrong.** `3D_DRAW_128` carries indices or vertices as its `VAP_VF_CNTL` walk says. `INDX_BUFFER` can stream into any register: the driver uploads vertex programs into `VAP_PVS_VECTOR_DATA_REG` with it.
- **Logical texture ids.** A fragment program's `TEX_ID` names the id in bits 31:28 of `TX_FILTER0`, not the physical map. The driver rotates the physical map every frame and keeps id 0.
- **Texture uploads** through host data written to `0x17E4`, an undocumented port after `HOST_DATA_LAST`, with `DST_TILE` (`0x1700`) set. A `DP_GUI_MASTER_CNTL` write sets `DP_CNTL`'s directions, as on the Rage 128.
- **Vertex lighting.** Fixed-function lighting compiles to vertex programs that use dual-math instructions, which Mesa never emits: a vector instruction with `PVS_DST_DUAL_MATH_OP` (bit 28) set also runs a math opcode (here reciprocal square root), which its third operand describes, as AMD's R5xx Acceleration guide lays out: the opcode in bits 21-24, two scalar sources by the X and Y swizzles of that operand's register, and one component of alternate temporary 0-3 as destination. We first decoded this from programs alone and got the opcode, the source and the destination wrong; 3DMark 2001's leaf shaders then took infinities into their positions and the car chase lost every leaf. LIT takes N.H, N.L and the exponent from the w of its three operands.

Its hottest MMIO polls: `CP_RB_RPTR` (about 900,000 reads while it waited on a fence that never came), then the DDC GPIOs (about 15,000 each).

## Catalyst 10.2 on Windows XP

The installer (`10-2_legacy_xp32-64_dd_ccc.exe`) installs the display driver and Catalyst Control Center. Run it on the ACPI-HAL XP after XP's own ATI driver has taken the card, and restart. After that, XP comes up at 1440x900 in 32 bits on Catalyst's driver: ati2dvag.dll 6.14.10.6925, ati2mtag.sys, DDI 9, with DirectDraw, Direct3D and AGP texture acceleration on. dxdiag's DirectDraw tests and its Direct3D 7, 8 and 9 tests pass, and the OpenGL driver (atioglxx.dll) draws XP's 3D Pipes screen saver. CCC needs .NET 2.0, which our XP image lacks: at each logon it says so and quits.

What it needed:

- **IA32_DEBUGCTL and the branch records.** As ati2mtag.sys starts, it sets `IA32_DEBUGCTL.LBR` and later reads the last branch record stack, through self-modifying code that patches RDMSR and WRMSR into place. On a family 15 model 2 CPU those are `MSR_LASTBRANCH_TOS` (0x1DA) and `MSR_LASTBRANCH_0..3` (0x1DB to 0x1DE). Each raised #GP, which bugchecked XP with 0x8E in ati2mtag.sys. The CPU now keeps DEBUGCTL's bits and reads the branch records as empty.
- **Fast Z clears.** The OpenGL driver clears depth only with `3D_CLEAR_ZMASK`, which zeroes the Z compression mask; with `ZB_BW_CNTL.FAST_FILL` the card then reads `ZB_DEPTHCLEARVALUE` for those tiles and never touches the Z buffer. We keep the mask (2 x 4096 dwords, one per 32x16 pixels on R300's two Z pipes, as Mesa's r300 driver lays it out) and, before the next draw that reads Z with FAST_FILL, write the clear value into the blocks it marks as cleared: with a Metal compute fill queued behind the draws, or on the CPU. Without this the stale Z buffer failed every pixel and 3D Pipes drew nothing. The fill stays inside the scissor: one mask serves whichever Z buffer is bound and knows nothing of its height, so in 3DMark 2001 a mask cleared for the 640x480 Z buffer, resolved with a 256x256 render target's Z buffer bound, filled on past it into the vertex buffers after it and wiped out the car chase's terrain. Each Z buffer reads the mask from `ZB_ZMASK_OFFSET` on: Catalyst gives every Z buffer its own part and clears only that part (3DMark 2001's demo renders into a 512x512 target whose mask starts at 0x5b00), so the mask grows past its first 8,192 dwords when a clear reaches beyond them. Ignoring the offset left that Z buffer uncleared, and every frame z-fought the one before: black streaks on the demo's statue and striped walls behind it.

- **A later silicon revision.** `CONFIG_CNTL`'s `CFG_ATI_REV_ID` used to read as A11, the first R300 silicon. For A11 the Direct3D driver (ati3duag.dll) writes `RB3D_COLORPITCH0` = 0xC00000 (ARGB8888, pitch 0) and a channel mask before every 2D blit, and does not set the colour buffer again for the 3D draws after it. Half-Life uploads its lightmaps with host-data blits in the middle of a frame, three per frame with the flashlight on, so every later draw in that frame went to a buffer with no pitch: surfaces lost their lightmap pass and went bright, the view model and other triangles vanished. How A11 hardware coped is unknown. The card now reports A13, for which Catalyst sends no such write. A save state keeps the driver's choice until XP restarts.

How it drives the card, compared with XP's own driver:

- It reprograms the memory controller at start: `MC_FB_LOCATION` = 0xF7FFF000, so VRAM sits at 0xF0000000 in the card's address space, with `DISPLAY_BASE_ADDR` there too.
- Its ring and indirect buffers are in the PCI GART. Fences are the same `CP_RESYNC_ADDR`/`CP_RESYNC_DATA` pair, followed by a write of a GART address to the undocumented `0x780`, which we ignore.
- Each indirect buffer starts with a one-dword type-3 packet `0x20` (`0x01000002`) after `WAIT_UNTIL`. We skip it.
- Flips are `CRTC_OFFSET` writes in the ring, behind the frame's draws. OpenGL renders to a tiled back buffer and presents it with a full-screen point sprite into one of two front buffers before the flip.
- Colour and depth buffers are macro- and micro-tiled. We keep them linear, as everywhere else. Tiled textures still keep the level layout the tiling implies: each level is padded to whole tiles as `TX_OFFSET`'s MACRO_TILE and MICRO_TILE say, with Mesa's alignments, so a microtiled 32-bit 4x4, 2x2 or 1x1 level has a 16-byte pitch. Reading those levels with the linear 32-byte pitch turned them black, and grazing surfaces that pick small levels went black with them.

Sniper: Path of Vengeance (LithTech, Direct3D) on Catalyst needed:

- **Frustum clipping.** Catalyst sets `DX_CLIP_SPACE_DEF` and leaves clipping on, with a guard band of about 3.4 by 4.2 viewports. We only clipped at w = 0, so vertices just in front of the eye but behind the near plane, or lerped onto w = 0, became huge spikes across the screen and their near depth hid the level. Clipping interpolates from the inside vertex, since t computed from a far outside one rounds to 1.
- **Fog.** Its levels use linear fog by eye distance (`GB_SELECT` 1/(1/W)), with the clear colour as fog colour.

3DMark 2001 (MadOnion) on Catalyst needed, compared against MadOnion's own recording of the demo:

- **Fast Z clears inside the scissor.** Its car chase draws into a 256x256 render target whose Z buffer sits just before the terrain's vertex buffers in VRAM; resolving the Z mask for the whole screen there overwrote them, so the terrain vanished.
- **Colour clears through the Z buffer** (`ZB_CB_CLEAR`). Without them the bottom half of the screen turned yellow in transitions and the credits, and the Lobby's floor showed green stripes.
- **The alpha test's own function order** and **dual-math vertex instructions as AMD documents them.** The trees' leaves are alpha-tested billboards whose vertex program uses dual-math multiplies; misread, they took infinities into their positions.
- **Speed.** In the car chase, from about 9 to 57 frames per second, and in the Lobby from 14 to 40: VGA port writes only for VGA ports, scanout and the display refresh that no longer wait for the command processor or for frames being drawn off screen, each vertex shaded once on several threads from a program decoded once per draw, and texel reads in Metal without per-byte divisions. Then Metal's samplers (Lobby GPU time per second from 1.2 s to 0.4 s) and the vertex stage on the GPU took Nature from 31 to 360 frames per second (command processor thread about 70% idle, GPU 83% busy) and the Lobby from 36 to 47 (from 52 to 65 in a later part), where the GPU is now 25% busy and the guest's own CPU time is the limit.
- **A guest clock that keeps up.** Each test checks its run's length by QueryPerformanceCounter (the PM timer) against GetSystemTime, and records -1, shown as N/A, when they differ by more than half a second. The system time counts RTC ticks, and the RTC used to lose ticks whenever the emulation thread polled it late (see [architecture.md](architecture.md)), so the busier tests (both fill rates, point sprites, Lobby low detail, Nature) fell behind by seconds. Long pauses of the whole process, as `sample` makes, can still lose ticks past that backlog.
- **Score:** 3354 3D marks at 1024x768x32 with compressed textures. Car Chase 29.2 / 13.1 fps (low / high detail), Dragothic 39.4 / 23.2, Lobby 86.9 / 33.6, Nature 20.0; fill rate 1050 / 1365 MTexels/s (single / multi-texturing), high polygon count 3.44 / 2.53 MTriangles/s (1 / 8 lights), environment and DOT3 bump mapping 63.5 / 125.1 fps, vertex shader 18.0 fps, pixel shader 75.7 fps, point sprites 1.63 MSprites/s.

To install it:

1. Boot a fresh `~/xp-acpi.img` clone with `--gpu radeon --vga-rom ROM`, let XP install its ATI and AGP drivers, and click Yes on the restart prompt.
2. At the 1440x900 desktop, `cdrom ISO` on the control socket, run the installer, choose Custom (display driver and CCC), click Continue Anyway on the logo warning, then Finish with "restart now".
3. Take a save state before the restart. Killing the guest during the install can break its AGP driver.

## Windows Vista's WDDM driver

Vista SP2 binds its in-box "RADEON 9700 PRO (Microsoft Corporation - WDDM)" driver (atikmdag.sys). It used to stop with Code 43 after reading `CONFIG_MEMSIZE`; it now starts and draws the Vista Basic desktop through the CP and the 2D engine. What it needed:

- **AGP enabled by firmware.** ATI's ASIC library finds the card's AGP capability and drops a capability the driver requires unless the card's AGP command register has AGP_ENABLE set; the adapter then fails to start. Vista's agp440.sys only programs AGP when the bridge returns to D0 (on resume), so it relies on the BIOS having done it. The board now enables AGP on the 440BX and the card at their fastest common rate (2x, with sideband addressing) at power-on and at every reset, as a 440BX board's firmware does.
- **PAT.** agp440's AddDevice allocates its GATT with `MmAllocateContiguousMemorySpecifyCache(MmWriteCombined)`, which fails when the CPU reports no PAT. The filter then never attaches, dxgkrnl reports no AGP aperture, and atikmdag refuses to start with less than 32 MiB of aperture. CPUID now reports PAT, as a real Northwood does, and IA32_PAT keeps what the guest writes. We give every page the same memory type anyway.
- **An output protection block at `0x1FC0`-`0x1FDC`**, four channels of a control and a data register. The driver flushes a channel (writes 4, waits for the control to read 0), starts it (writes 0, waits for 8) and reads a nonzero random word, without timeouts. It then starts a key exchange; we never answer it (the control stays 1), its bounded wait times out and the driver goes on without the protected session.
- **The GUIDMA interrupt.** Every DMA buffer ends with the fence id in `SCRATCH_REG6` and a GUI DMA descriptor written through the CP at `0x720`-`0x728`: no bytes, EOL set, INTDIS clear. Its only job is to raise `GEN_INT_STATUS` bit 30 (GUIDMA), which the driver enables in `GEN_INT_CNTL`; dxgkrnl's scheduler stalls until it arrives. We latch it once the GPU has caught up, and the driver acknowledges it by writing the bit back.

**Aero.** With the Windows Aero colour scheme, DWM composes through the 3D engine: textured quads from tiled surfaces into one of two front buffers (at 0 and `0x1DB000`), then a flip. Glass with its blur, dragging windows and Flip3D all draw, on the Metal and the CPU path. It needed:

- **Two undocumented type-3 packets** that set the vertex array's address. AMD's R5xx guide lists neither, and Mesa and Linux never send them, so their meaning comes from what the driver sends. `0x24` is `[slot, address]` and stores a base address; `0x25` is `[slot, ?, offset, register / 4]` and writes the slot's base plus the offset to the register. The driver sends `0x24 [0, 0xF8052000]` once per DMA buffer, then one `0x25 [0, offset << 15, offset, 0x832]` before each draw, which sets `VAP_VTX_AOS_ADDR0` (0x20C8) to its place in a vertex buffer in the GART. The second dword is always the offset shifted left by 15, and we ignore it. The driver writes `VAP_VTX_NUM_ARRAYS` and `VAP_VTX_AOS_ATTR01` as plain registers and never sends `3D_LOAD_VBPNTR`. While these packets were skipped, every draw read its vertices from a stale address and DWM drew nothing.
- **A zero colour pitch keeps the bound colour buffer.** Before each 2D blit, and after some draws, the driver flushes the 3D cache and writes `RB3D_COLORPITCH0 = 0xC00000` (ARGB8888, pitch 0) and `RB3D_COLOR_CHANNEL_MASK` with 0xF in its low bits and a changing number above them. A new DMA buffer binds its colour buffer again, but a blit in the middle of one does not: with the Start menu open, the search box's caret blink uploads its pixels that way, and DWM's next draw, the menu's own contents, follows without `COLOROFFSET0` or `COLORPITCH0`. Taking pitch 0 as written dropped that draw, so the menu vanished into its blurred glass at every blink and came back whenever a hover redrew it, which looked like heavy tearing. The register now ignores writes whose pitch is 0. What the card really does with them is unknown; the next draw shows it keeps drawing into the buffer bound before.
- Nothing else. Draws drawing 0 pixels in a trace are normal: DWM sets a scissor around the dirty rectangle and draws whole window parts through it. The output protection handshake can stay unanswered.

The driver also wraps each DMA buffer in `COND_EXEC` (`0x21 [0xF7FFF000, 2, 0x3C]`) and starts it with `PRED_EXEC` (`0x20`); we run what they guard unconditionally.

## 3D engine

`crates/radeon/src/r3d` runs the R300's 3D pipeline. Its CPU path draws while the packet executes and is the reference for the Metal backend, which draws the pixels by default (see [Metal backend](#metal-backend)):

- **Packets:** `3D_DRAW_IMMD(_2)`, `3D_DRAW_VBUF(_2)`, `3D_DRAW_INDX(_2)`, `INDX_BUFFER` (indices, which Catalyst may start two bytes into a dword, or any register), `3D_LOAD_VBPNTR` and `3D_DRAW_128`.
- **Vertex fetch:** a vertex's dwords come from the packet or from each array of structures in turn, and the PSC (`VAP_PROG_STREAM_CNTL*`) splits them into input vectors: floats, bytes, D3DCOLOR, shorts and packed 10/11-bit formats, with SIGNED and NORMALIZE.
- **Vertex shader:** the PVS program in vector memory (code in a ring of 256 instructions at 0, which Catalyst wraps programs around, and constants at 512), interpreted per vertex: every R3xx vector and math opcode, the macro MAD, dual-math instructions, relative addressing through A0, and saturation. `PVS_BYPASS` passes inputs through. Flow control (loops, JSR) is not implemented.
- **Setup:** `VAP_OUT_VTX_FMT_*` lays out position, point size, colours and texture coordinates. Triangles are clipped in clip space as `VAP_CLIP_CNTL` and `VAP_CNTL` say: to the near plane (z = 0 with `DX_CLIP_SPACE_DEF`, z = -w without), the far plane z = w and the guard band in `VAP_GB_*_CLIP_ADJ`; with `CLIP_DISABLE` only to w > 0. Then the VTE divides by w and applies the viewport (whose registers have a second address at 0x1D98). Points become sprites, lines become quads; quads, strips, fans and polygons become triangles. Culling follows `SU_CULL_MODE`; polygon offset `SU_POLY_OFFSET_*`.
- **Rasterizer:** triangles are scan-converted in 1/16 pixel fixed point with a top-left fill rule, inside the scissor and the clip rectangles (offset by 1440 with 1/12 subpixels, 1088 with 1/16, as AMD's R5xx guide says for `GB_TILE_CONFIG`). Attributes are interpolated perspective-correctly; the RS instructions place colours and texture coordinates in the fragment shader's registers.
- **Fragment shader:** the US program, interpreted on 2x2 quads so texture lookups can take their level of detail from neighbouring pixels: nodes, TEX/TXP/TXB/KIL, and every RGB and alpha ALU opcode with pre-subtract, swizzles, modifiers, output modifiers and clamping.
- **Textures:** 8 to 128-bit integer, half and float formats and DXT1/3/5, from VRAM or through the GART, with every wrap mode, point and bilinear filtering and mipmap levels. `SIGNED_COMP0..3` read components as two's complement (the bump map formats V8U8 and Q8W8V8U8). Cube maps (`TEX_COORD_TYPE` 2) pick their face from the direction's major axis, with the six faces of each level one after the other as Mesa lays them out; volume textures (`TEX_COORD_TYPE` 1, `TXDEPTH`) filter between slices.
- **Fog:** the GA's fog value is a vertex colour's alpha, the eye distance w or z (`GB_SELECT`), scaled and offset by `GA_FOG_SCALE`/`GA_FOG_OFFSET` and interpolated; the FG blends towards `FG_FOG_COLOR_*` (10-bit units) by it, linearly, as 2^(-16f) or 2^(-16f²), or by `FG_FOG_FACTOR`, as Mesa's classic r300 driver's fog constants imply.
- **Colour clears through the Z buffer:** with `ZB_BW_CNTL.ZB_CB_CLEAR`, drawn pixels write `ZB_DEPTHCLEARVALUE` into the Z buffer unchanged. Catalyst clears a colour buffer that way, as Mesa's r300 driver does (its CBZB clear): the top half bound as colour, the bottom half as Z, one quad over the top half.
- **Back end:** alpha test, 16-bit and 24/8 depth and stencil (two-sided too), blending with every factor and combine function, separate alpha, ROP2, colour masks, and ARGB8888, RGB565, ARGB1555, ARGB4444, I8 and UV88 colour buffers. `ZB_ZPASS_*` counts passing pixels for occlusion queries.
- **Tiling:** like the 2D engine, it treats every surface as linear, so what it draws stays consistent with the CRTC and the 2D engine.
- **Sync:** on the CPU path, drawing finishes before the next packet, so fences, scratch writebacks and `WAIT_UNTIL` always come after finished pixels.

Still missing: HiZ (`3D_CLEAR_HIZ` does nothing, which only costs speed), Z compression beyond fast clears (see [Catalyst](#catalyst-102-on-windows-xp)), seamless filtering across cube faces, user clip planes, MSAA and the R5xx-only opcodes.

### Metal backend

`--gpu-3d metal` (the default) or `--gpu-3d cpu`, also set by `SLOPPER_R3D`, picks the backend; `status` reports draws, CPU fallbacks, waits, command buffers submitted, kicks throttled by the in-flight limit, pipelines and the milliseconds the GPU spent on finished command buffers (two samples give how busy the GPU is); with the command processor thread it adds the ring kicks, the guest's `CP_RB_WPTR` writes.

- **Vertices on the GPU:** for triangles, quads, strips, fans and polygons, a Metal vertex function does what the CPU path does per vertex (`r3d/vsgen.rs`): it reads the vertex's dwords from the packet or from the arrays of structures, splits them as the PSC says, runs the vertex program translated to MSL (`r3d/pvs_msl.rs`, straight-line code with the interpreter's semantics, `precise::` math, relative addressing and dual math), lays out the outputs as `VAP_OUT_VTX_FMT_*` says, applies the VTE and viewport, snaps window positions to 1/16 pixel, clamps colours, applies `GA_SOLID` colours and fills the varyings the RS instructions build. The CPU only turns the primitive into a triangle index list (keeping the order the CPU path sets triangles up in) and copies what Metal cannot read in place: arrays in VRAM are read straight from the VRAM buffer, arrays in system memory and immediate vertices are copied into the arena, and so are the constants. Metal clips: positions stay in clip space, with the near plane (z = 0, or z = -w without `DX_CLIP_SPACE_DEF`) and the far plane as clip distances and depth clamping instead of Metal's own z clipping, so the depth comes from the rasterized position. Metal culls by `SU_CULL_MODE` and gives the face for two-sided stencil; the fragment shader applies polygon offset from the depth's screen-space slope. Flat shading rotates each triangle so `GA_COLOR_CONTROL`'s provoking vertex comes first, which keeps its winding, and passes the flat colour components in `[[flat]]` varyings. The pipeline key adds the vertex state (program words, PSC, array sizes, output format, VTE, clip mode, fog source, shading, RS) to the fragment program's. Points and lines still go the CPU way below, as do draws whose copies would overflow the arena; `SLOPPER_VS=cpu` sends every draw that way, and `status` counts the draws the GPU's vertex stage took.
- **Split:** for everything else the CPU fetches vertices, runs the vertex program (once per distinct index of an indexed draw, and on up to eight threads for draws of 2,048 vertices or more), clips, culls, applies polygon offset, flat shading and the RS, and turns points and lines into triangles. `Raster::triangle` captures the window-space triangles, with the pixel stack registers the RS fills, instead of scan-converting them. Metal rasterizes them in an attachmentless 4096x4096 pass with the scissor as its scissor rectangle.
- **VRAM** is one `MTLBuffer` over the card's memory (`newBufferWithBytesNoCopy`, page aligned, shared storage), so Metal reads textures and writes pixels where the CRTC, the 2D engine and the guest see them, with no copies.
- **Fragment programs** are translated to MSL (`r3d/msl.rs`) straight-line, with the same instruction semantics as the interpreter. Pipelines are cached by the instruction words the program runs, the RS slots and the interpolation mode, up to 1,024 of them. Keying on every slot, stale words around the program included, built a new pipeline for nearly every draw in 3DMark 2001: thousands of compiled variants, gigabytes within a minute. Constants, texture state, buffer addresses and the reference values (stencil, alpha, constant colour, fog colour, depth scale) go in a uniform buffer. The rest of the back end's state is compiled in (`msl::Backend`: colour formats, depth and stencil modes, the alpha function, blending, the ROP, fog mode, output swizzle and channel mask, with what the state makes irrelevant zeroed), so the shader carries no branches on it; that halved the time of 3DMark 2001 Nature's blended foliage pass (2.3 to 1.05 ms per frame in Metal System Trace) and the Lobby's GPU time (500 to 285 ms per second), for 47 pipelines instead of 39 in the Lobby.
- **Textures** are sampled by Metal where it can match `tex.rs` exactly (`r3d/metal_tex.rs`). The shader still picks the mipmap level, from its own derivatives as the CPU path does, then samples that level's image with a Metal sampler. Every level, and every cube face of it, is its own 2D texture. Uncompressed levels are linear textures over the VRAM buffer (`newTextureWithDescriptor:offset:bytesPerRow:`, offsets and pitches 16-byte aligned), so they never go stale: CPU writes, blits and render-to-texture all show at once. Metal has no linear layout for BC formats, so DXT1/3/5 levels are copies, kept with a shadow of their bytes and compared with VRAM at first use after anything may have written VRAM outside Metal (a new ring run, a 2D operation or other `gpu_sync`, CP or MMIO writes to VRAM, guest writes through the board); a copy that differs is made again. Formats map to R8, RG8, R16, RG16, B5G6R5, ABGR4, BGR5A1, RGBA8, RGB10A2, RGBA16, half and float formats, their snorm variants when every component is signed, and BC1 to BC3, with the swizzle and channel order applied after the sample. Wrap modes map to Metal's address modes; clamp-to-border only when the border colour equals one of Metal's three presets on a four-channel format. Everything else is decoded once on the CPU with `tex.rs` into RGBA8 images (formats whose components RGBA8 holds exactly) or RGBA32Float ones: partly signed formats and formats Metal lacks (Y4X4, Z3Y3X2, Z5Y5X6, the 11/10-bit packed ones). Textures in a native format that cannot be linear textures (unaligned levels, textures in system memory) are copies in that format. Copies and decoded images are checked like DXT copies; textures in system memory are compared with their bytes at every use, since the guest writes them without the card seeing it. Volume textures (one 2D image per slice), mirror-once-to-border and border colours Metal lacks are read texel by texel with `read()`, and the shader applies the wrap modes and bilinear filter as `tex.rs` does; there is no texel decoder in the shaders any more. `SLOPPER_TEX=decode` decodes every texture on the CPU, for comparison; `status` counts the binds sampled from native formats, from decoded images and by the shader's own filter, and the copies and decodes made.
- **Texture sampling is linked, not compiled, into each pipeline.** `msl_tex.metal` (with the shared `msl_common.metal`) is compiled once at start into a library whose `[[visible]] sample_tex` each pipeline links as a private function (`fragmentLinkedFunctions`); the per-pipeline source only declares it `extern`. Naming `texture2d<float>` alone cost every compile about 0.12 s in the MSL front end, and the sampling code as much again; cold pipelines now take about 30 ms in the front end and 40 ms in the backend instead of about 0.27 s and 0.06 s. `status` shows the milliseconds spent building pipelines, and `SLOPPER_MSL_DUMP=DIR` writes each pipeline's source there. To time cold compiles offline, salt the `vs`/`fs` names: Metal caches compiled sources and AIR across processes.
- **Back end in the shader.** Metal's depth attachments cannot alias VRAM, and it has no 24-bit Z, no ROP and no ARGB4444 or ARGB1555 layout matching the card's. So the fragment shader does the alpha test, Z16 and Z24/8 depth and two-sided stencil, blending, the ROP, the channel mask and colour packing itself, reading and writing VRAM, with raster order groups keeping overlapping fragments in submission order. The occlusion counter is an atomic. A draw that samples memory the open encoder writes starts a new encoder.
- **Fallback:** the CPU draws when a pipeline fails to build, a draw fills more than 24 pixel stack registers, or a texture has more than 768 images (levels times faces or volume slices); it waits for Metal first.
- **Sync:** draws collect in a command buffer, which is submitted at the end of each ring run, at a fence, or after a draw once it has been open for 50 µs while fewer than half of the in-flight limit wait. Smaller command buffers cost the GPU less per frame (Metal System Trace: about 20% less fragment time per frame in 3DMark 2001's Nature). Nature went from about 325 to 420 flips per second and Half-Life's tram ride from about 4,900-11,300 to 6,200-12,100 draws per second; the extra submissions cost the Lobby, which the guest's CPU limits, about 4% (111 to 107 flips per second) and its GPU time rose from 0.3 to 0.4 s per second, and the 2,500-frames-per-second scene after Nature lost about 5%. At most eight submitted command buffers, each with its 32 MiB arena, wait for the GPU; a ninth submission waits for the oldest (`throttled` in `status`). Every call into Metal runs in its own autorelease pool, since the command processor thread has no run loop to drain one. Everything that touches VRAM outside the pipeline waits for it first: 2D operations, CPU fallbacks and saves. Scanout waits only when queued or running work writes the buffer on screen (each command buffer's arena lists the VRAM ranges its draws and fills write), so a page-flipping game's display refresh never waits for the frame it is drawing. The refresh, which runs on the emulation thread every 16 ms, also does not wait for the card: while the command processor holds it, or Metal still draws into the visible buffer, it skips the refresh, and only the seventh skip in a row (about 100 ms) waits. So do software interrupts. Occlusion queries behave like fences: draws count passing pixels into a slot of a counter buffer, a `ZB_ZPASS_ADDR` write queues the count so far (the register's value plus the slots since it was last set or read) to be written once its draws finish, and starts a new slot so later draws stay out of it; fences behind a query land only after the query's result. Source games end several queries a frame; waiting for the GPU at each cost Portal about 250-300 full waits a second. Idle polls (`RBBM_STATUS`, cache `CTLSTAT`, `CP_STAT` and the other status registers drivers spin on) read busy until the draws finish. Fences behave as on the card: a scratch register or `CP_RESYNC` write behind queued draws lands, with its writeback, only when those draws finish; the command processor thread applies finished fences. Each command buffer signals its serial number on a shared event (`MTLSharedEvent`) after its work, and fences land once the event reaches their command buffer, about 40 µs before its status reads Completed; the arena still waits for Completed. `WAIT_UNTIL` needs no wait, since Metal runs draws in order and the CPU-side users wait anyway. Guest CPU reads of VRAM do not trap, so they rely on the driver waiting on a fence or idle first, as it must on the card.
- **Measurements** (on this Apple Silicon Mac, before Metal's samplers and the GPU vertex stage, CPU path / Metal): dxdiag's spinning cube at 640x480 (D3D7) 28 / 460, at 640x480 (D3D8) 29 / 541 and at 800x600 (D3D9) 16.5 / 545 frames per second; glxgears 48 / 114, with or without Compiz. Waiting for the GPU at every fence instead held glxgears at 85.

## Command processor thread

The command processor runs on a worker thread (`radeon-cp`), so the vCPU never executes packets. `crates/radeon/src/card.rs` holds it: `Card` is what the board owns, with the register file, the engines, VRAM and the Metal backend behind one lock.

- **Kicks:** a `CP_RB_WPTR` write stores the new pointer in an atomic, counts a submission and wakes the worker; the MMIO exit returns at once. An idle worker spins for 50 µs before it parks, so a stream of writes rarely pays for a wake-up.
- **Slices:** the worker fetches the ring from `CP_RB_RPTR` to the posted write pointer and runs it, with the indirect buffers it calls, 32 packets at a time. Between slices it publishes its progress and lets go of the lock, and it yields at once when the vCPU waits for the lock. `CP_RB_RPTR` and its writeback advance with each slice.
- **Status without the lock:** `CP_RB_RPTR`, `CP_RB_WPTR`, the scratch registers, `RBBM_STATUS` and `CP_STAT` read from mirrors the worker updates. The engines count as busy while a submission waits or runs, while Metal draws are in flight and while fences wait for them: `RBBM_STATUS` then sets bit 31 (GUI_ACTIVE), `CP_STAT` bits 31 and 0, and the cache `CTLSTAT` registers their busy bit. Polls never wait. The undocumented idle registers (`0x790`, `0x7B0`, `0xA10`) and `IDCT_STATUS`, whose busy values we do not know, wait for the worker and the GPU instead.
- **Fences:** the worker runs scratch and `CP_RESYNC` writes in ring order. Behind queued Metal draws they wait as described under [Metal backend](#metal-backend); while fences wait, the worker spins on the shared event, until 500 µs pass without one landing, and writes the fences back; otherwise it looks for finished command buffers every 50 µs while any are in flight. A ring kick wakes it from that wait too.
- **Round trips:** some guests wait on a fence after nearly every draw. 3DMark 2001's demo, in its orange CAM01/CAM02 scene, sends one DMA buffer of one or two small draws, ending in `WAIT_UNTIL` (3D idle clean) and a 64-bit timestamp in `SCRATCH_REG2`/`3`, and waits for it before the next one: about 2,500 round trips a second. On an M4 Pro a command buffer with a render pass takes about 170-180 µs from commit to its shared event even when the pass draws nothing (an empty command buffer: 6 µs; committing it early and releasing it with an event: no faster; a 1024x1024 instead of 4096x4096 target: about 15 µs less), so each round trip costs at least that plus the guest's own work, about 90 µs from fence to its next `CP_RB_WPTR` write. Landing fences on the shared event, spinning while they wait and waking on kicks took that scene from about 156 to 178 flips per second (4,900 to 5,500 draws); the Lobby and the car chase did not change.
- **Interrupts:** a software interrupt raised by a packet updates a mirror of `GEN_INT_STATUS`, `GEN_INT_CNTL` and the acknowledged vertical blank, which the board reads for the card's INTA level without the lock. When the worker changes it, it unparks the emulation thread and kicks the vCPU, and the board's next `tick` raises the line.
- **Ordering of guest MMIO:** a write to an engine or CP register (offsets from `0x400` up, plus `RBBM_SOFT_RESET` and the memory controller's `MC_*`, `AGP_BASE` and `AIC_*` mappings) first waits until the worker has run every submission posted before it, then runs on the vCPU thread under the lock. Display, interrupt, clock, I2C and palette writes take effect at once, as they do on the card. Reads of other registers take the lock but do not wait for the ring.
- **Memory:** the worker reaches guest RAM through the board's own pointer to it, with the AGP aperture and the bus master bit the board publishes before each access to the card. Pages it writes are handed to the board, which passes them to the JIT's self-modifying code watch at its next tick. VRAM is shared with the guest's mapping without synchronisation, as on the card; scanout, VGA accesses and CPU fallbacks take the lock and wait for Metal.
- **Save states and reset:** saving, loading and resetting wait until the worker has run everything posted and Metal has finished. The saved state is the same as without the thread, so older snapshots load.
- **Measured** (same save states, baseline vs thread): Half-Life's tram ride on Catalyst went from about 3,580 to 4,100 3D draws per second, with MMIO handling falling from 27% to 16% of the vCPU thread's host time; Catalyst's last guest hotspot, its `CP_RB_WPTR` write, left the PC samples. dxdiag's cubes (about 620, 750 and 750 flips per second for D3D 7, 8 and 9), 3D Pipes (vblank-paced) and glxgears (about 1,450 draws per second) stayed within a few percent, as their rings are light; glxgears made a third fewer MMIO exits.
- **Debugging:** `--gpu-thread off` (or `SLOPPER_GPU_THREAD=off`) runs the ring on the emulation thread during the `CP_RB_WPTR` exit, as before. Unit tests use that mode, except `card_tests.rs`, which checks the thread's ordering and status bits. With `--gpu-trace`, every access takes the lock so the trace stays complete.

## Open questions

- **CCC.** Catalyst Control Center needs .NET 2.0. With it installed, check that CCC opens and lists the card's settings.
- **Fixed-function Direct3D.** We expect Catalyst to turn fixed-function state into vertex and fragment programs, since R300 runs all transform, lighting and texture combining through its shader units. Logging what the driver sends confirms it.
- **Guest reads of VRAM without a fence.** See the Metal backend's sync. Write-protecting rendered pages through HVF would catch them, at a fault per page.

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

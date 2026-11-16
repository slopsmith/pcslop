# GPU

Plan for 3D acceleration. Nothing here is built yet.

## Target: GeForce FX 5200 PCI (NV34)

We emulate a real Nvidia card so that Windows XP runs Nvidia's official driver. We emulate the card's interface (MMIO registers, the command FIFO, DMA objects) at a low level, and draw everything it is asked to draw with Metal. We do not emulate the GPU's rendering on the CPU.

Why this card:

- **DirectX 9 over PCI.** The FX 5200 was widely sold as a PCI card for PCs without AGP. Our i440FX board has no AGP, so it fits without a new board. It exposes Shader Model 2.0 vertex and fragment programs, DXT textures and hardware shadow maps.
- **Same era as our CPU.** It is from 2003, like the Northwood Pentium 4 we report in CPUID. ForceWare 175.19 is the last XP driver for the FX line.
- **Its speed does not matter.** We only borrow its feature set. Metal does the drawing.
- **It is well documented for an Nvidia card.** See [References](#references).

Rejected:

- **GeForce4 Ti (NV25, Kelvin):** the best-understood option thanks to xemu's NV2A work, but DirectX 8.1 only.
- **GeForce 6200 PCI (NV44):** Shader Model 3.0, but from 2005, and it renders into system memory (TurboCache), which makes it harder to emulate.

NV04 through NV40 share one design for PFIFO, instance memory, DMA objects, the CRTC/RAMDAC and VGA. Only the 3D object class changes between generations. A second card from that range later would reuse most of the work.

## Pieces

In the order we need them:

1. **PCI device.**
   - BAR0: 16 MiB of MMIO registers.
   - BAR1: VRAM, prefetchable.
   - An expansion ROM.
   - No AGP capability in config space. To check: how the driver tells PCI from AGP (config space capabilities, strap bits, or both).
   - Use a device ID that the 175.19 driver's INF lists, and confirm the ID against it.
2. **Video BIOS.** We cannot ship Nvidia's ROM. We build our own: SeaVGABIOS for the VGA and VBE side, plus the tables the XP driver reads (BIT, DCB, PLL limits, init scripts that do nothing). Nouveau's BIOS parser and envytools' `nvbios` describe these tables. A ROM dumped by the user could be an option later.
3. **Core blocks:**
   - PMC: chip ID, interrupt status and enables.
   - PBUS, PEXTDEV: straps.
   - PTIMER: the driver uses it for timing.
   - PFB: memory size and config.
   - PRAMIN: instance memory at the top of VRAM, also reachable through BAR0.
4. **Display:**
   - VGA, PCRTC and PRAMDAC: mode setting, scanout start address and pitch.
   - The hardware cursor.
   - DDC over I2C, so the driver reads an EDID that we generate.
   - TV-out stays unconnected.
   - Scanout is a Metal texture that the frontend shows.
5. **PFIFO.**
   - DMA pushbuffer fetch, the RAMHT object hash table, RAMFC channel state and RAMRO.
   - Channel switching. The driver may open more than one channel.
   - The puller hands methods to the object bound to each subchannel.
6. **DMA objects.** With no AGP GART, the driver gives the card paged DMA objects whose page lists point into system RAM. We follow those page lists to read guest memory. Linear DMA objects point into VRAM.
7. **2D classes.** The XP desktop is drawn through GDI acceleration:
   - surfaces;
   - image blits;
   - ROPs and patterns;
   - GDI rectangles and text;
   - image-from-CPU uploads;
   - scaled image transfers.

   These come before 3D because they give us a usable desktop. They run on the CPU at first and move to Metal later.
8. **Rankine 3D (NV34 class `0x0697`) on Metal.**
   - Decode vertex and fragment programs and translate them to MSL.
   - Map the rest of the 3D state to Metal pipeline, depth-stencil and sampler objects, and cache each of these by its state.
   - Notifiers, reference counters and semaphores, which the driver uses to wait for work.

## Open questions

- **Fixed-function Direct3D.** Does the XP driver turn fixed-function state into fragment programs, or does it still set NV2x-style register combiner state on NV3x? Mesa's nv30 driver only uses programs, so it cannot tell us. Logging the methods the real driver sends answers this. If it uses combiners, xemu's Kelvin code is the reference for turning combiner state into shaders.
- **Sync with the guest.** The driver waits on notifier and reference-counter writes.
  - If we write them only when the Metal command buffer completes, every fence costs a round trip to the GPU.
  - If we write them early, the guest can lock a render target and read stale pixels.

  A middle path: write early, and when the guest locks a surface, wait for pending Metal work and read the surface back.
- **Context switching.** NV3x PGRAPH saves and loads channel context through instance memory. We keep the context in Rust. We still have to know which registers the driver reads back after a switch.

## Apple Silicon and Metal

- **Unified memory:** guest VRAM (BAR1) can be a no-copy `MTLBuffer` over the same host memory. Linear surfaces, vertex buffers and index buffers then need no copy.
- **Dirty tracking:** guest CPU writes to VRAM that has a Metal copy, such as swizzled textures, are found with HVF page protection. The pages are mapped read-only, and the first write faults and marks the copy stale.
- **Formats:**
  - DXT1, DXT3 and DXT5 map to BC1 to BC3, which Apple Silicon samples natively.
  - Nvidia's swizzled texture and surface layouts are converted.
  - Palettized textures, if the driver exposes them, are decoded in the shader.
- **Gaps:**
  - No D24S8 on Apple Silicon, so we use `Depth32Float_Stencil8`. Depth bias and depth readback need conversion.
  - No quads or quad strips: we split them into triangles.
  - Alpha test and fog run in the fragment shader.
- **Precision:** NV3x mixes fp16 and fp32. We run everything as fp32, which matches or beats the hardware.

## References

These are listed with their licenses in [references.md](references.md).

- **envytools:** `rnndb/` has register and method definitions for PMC, PBUS, PTIMER, NV4 PFIFO, the NV1 to NV4 2D classes, PCRTC, PRAMDAC and VGA. `docs/hw/` describes the FIFO, DMA pusher, PGRAPH and the 2D objects. Its Rankine pages are mostly stubs. `nvbios/` parses video BIOS tables.
- **Mesa, `src/gallium/drivers/nouveau/nv30/`:**
  - `nv30-40_3d.xml.h`: the Rankine and Curie 3D methods.
  - `nvfx_vertprog.c`, `nvfx_fragprog.c`: shader encoders, which are our decoder spec.
  - `nv30_format.c`: texture and surface formats.
  - `nv30_state*.c`: how state maps to methods.
- **Nouveau kernel driver:** `drivers/gpu/drm/nouveau/`:
  - `nvkm/subdev/bios/`: BIT, DCB and PLL parsing.
  - `nvkm/engine/fifo/` and `nvkm/engine/gr/` for the NV04 to NV40 generations: channel and PGRAPH setup.
  - `dispnv04/`: pre-NV50 mode setting.
- **OpenGL extension specs:** `NV_vertex_program2` and `NV_fragment_program` define what each shader instruction does, including precision and special cases.
- **xemu, `hw/xbox/nv2a/`:** an NV2A (Kelvin) emulator that draws with OpenGL or Vulkan. It is the closest existing design to ours, for PFIFO, DMA objects, method dispatch, swizzling and surface caching.

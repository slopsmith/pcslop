//! Draws the 3D engine's triangles with Metal. The CPU still runs vertex
//! fetch, the vertex shader, clipping and triangle setup; Metal rasterizes,
//! runs the fragment program translated to MSL, and does the back end in
//! the same shader, reading and writing VRAM through a buffer that wraps
//! the card's memory without a copy. Raster order groups keep overlapping
//! fragments in submission order, so depth, stencil and blending behave as
//! on the CPU path.
//!
//! Draws collect in a command buffer until something needs finished pixels:
//! see `Radeon::gpu_sync`.

use super::msl;
use super::raster::{Captured, Raster};
use super::tex::{TexMem, Texture};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

/// Pixel stack registers a draw may interpolate; more go to the CPU.
const MAX_SLOTS: usize = 24;
const ARENA_SIZE: usize = 32 << 20;
const ALIGN: usize = 256;
/// The render target is attachmentless and covers the whole 4096x4096
/// coordinate space the scissor allows.
const TARGET: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TexDesc {
    enabled: u32,
    stage: u32,
    base: u32,
    limit: u32,
    width: u32,
    height: u32,
    format: u32,
    nlevels: u32,
    filter: u32,
    border: u32,
    sel: [u32; 4],
    off: [u32; 12],
    pitch: [u32; 12],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ColourDesc {
    offset: u32,
    pitch: u32,
    format: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Uniforms {
    consts: [[f32; 4]; 32],
    tex: [TexDesc; 16],
    cb: [ColourDesc; 4],
    ncb: u32,
    has_depth: u32,
    depth_base: u32,
    depth_pitch: u32,
    z16: u32,
    zb_cntl: u32,
    zs_cntl: u32,
    stencil_ref: u32,
    depth_scale: f32,
    depth_bias: f32,
    clips: [i32; 16],
    clip_rule: u32,
    blend: u32,
    ablend: u32,
    channel_mask: u32,
    constant_colour: u32,
    rop: u32,
    alpha_func: u32,
    out_fmt: u32,
    vram_size: u32,
}

type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
pub(crate) type Pipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

struct Arena {
    buf: Buffer,
    used: usize,
}

impl Arena {
    /// Copies `data` in and returns its offset, or None when full.
    fn put(&mut self, data: &[u8]) -> Option<usize> {
        let at = self.used.next_multiple_of(ALIGN);
        if at + data.len() > ARENA_SIZE {
            return None;
        }
        unsafe {
            let dst = self.buf.contents().as_ptr().cast::<u8>().add(at);
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
        }
        self.used = at + data.len();
        Some(at)
    }
}

struct Batch {
    cmd: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
    enc: Option<Retained<ProtocolObject<dyn MTLRenderCommandEncoder>>>,
    arena: Arena,
    /// VRAM ranges the open encoder writes; a draw that samples one of them
    /// starts a new encoder so it sees the finished pixels.
    written: Vec<(usize, usize)>,
}

pub(crate) struct MetalR3d {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    vram: Buffer,
    vram_len: usize,
    counter: Buffer,
    pipelines: HashMap<Vec<u32>, Option<Pipeline>>,
    batch: Option<Batch>,
    inflight: Vec<(Retained<ProtocolObject<dyn MTLCommandBuffer>>, Arena)>,
    free: Vec<Arena>,
    draws: u64,
    fallbacks: u64,
    waits: u64,
}

// The Metal objects are only touched while the card's owner holds it.
unsafe impl Send for MetalR3d {}

fn bytes_of<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

impl MetalR3d {
    /// Wraps `vram`, which must be page aligned and stay put, for drawing.
    pub fn new(vram: &mut [u8]) -> Option<Self> {
        let device = MTLCreateSystemDefaultDevice()?;
        if !device.areRasterOrderGroupsSupported() {
            return None;
        }
        let queue = device.newCommandQueue()?;
        let ptr = NonNull::new(vram.as_mut_ptr().cast::<c_void>())?;
        let vram_buf = unsafe {
            device.newBufferWithBytesNoCopy_length_options_deallocator(ptr, vram.len(), MTLResourceOptions::StorageModeShared, None)
        }?;
        let counter = device.newBufferWithLength_options(16, MTLResourceOptions::StorageModeShared)?;
        Some(MetalR3d {
            device,
            queue,
            vram: vram_buf,
            vram_len: vram.len(),
            counter,
            pipelines: HashMap::new(),
            batch: None,
            inflight: Vec::new(),
            free: Vec::new(),
            draws: 0,
            fallbacks: 0,
            waits: 0,
        })
    }

    /// The pipeline for a fragment program, built from MSL the first time.
    fn pipeline(&mut self, r: &Raster, slots: &[usize]) -> Option<Pipeline> {
        let us = &r.us;
        let mut key = vec![r.perspective as u32, slots.len() as u32];
        key.extend(slots.iter().map(|&s| s as u32));
        for n in &us.nodes {
            let (ts, tl) = n.tex.unwrap_or((usize::MAX, 0));
            key.extend([n.alu.0 as u32, n.alu.1 as u32, ts as u32, tl as u32]);
        }
        key.push(u32::MAX);
        key.extend(&us.tex);
        key.extend(us.alu.iter().flatten());
        if let Some(p) = self.pipelines.get(&key) {
            return p.clone();
        }
        let built = self.build(&msl::shader(us, slots, r.perspective));
        if built.is_none() {
            eprintln!("radeon: Metal pipeline failed; drawing on the CPU instead");
        }
        self.pipelines.insert(key, built.clone());
        built
    }

    fn build(&self, src: &str) -> Option<Pipeline> {
        let lib = match self.device.newLibraryWithSource_options_error(&NSString::from_str(src), None) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("radeon: MSL error: {}", e.localizedDescription());
                return None;
            }
        };
        let vs = lib.newFunctionWithName(&NSString::from_str("vs"))?;
        let fs = lib.newFunctionWithName(&NSString::from_str("fs"))?;
        let desc = MTLRenderPipelineDescriptor::new();
        desc.setVertexFunction(Some(&vs));
        desc.setFragmentFunction(Some(&fs));
        desc.setRasterSampleCount(1);
        match self.device.newRenderPipelineStateWithDescriptor_error(&desc) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("radeon: pipeline error: {}", e.localizedDescription());
                None
            }
        }
    }

    fn arena(&mut self) -> Option<Arena> {
        if let Some(a) = self.free.pop() {
            return Some(a);
        }
        let buf = self.device.newBufferWithLength_options(ARENA_SIZE, MTLResourceOptions::StorageModeShared)?;
        Some(Arena { buf, used: 0 })
    }

    fn open(&mut self) -> Option<&mut Batch> {
        if self.batch.is_none() {
            let cmd = self.queue.commandBuffer()?;
            let arena = self.arena()?;
            self.batch = Some(Batch { cmd, enc: None, arena, written: Vec::new() });
        }
        self.batch.as_mut()
    }

    fn encoder(batch: &mut Batch) -> Option<Retained<ProtocolObject<dyn MTLRenderCommandEncoder>>> {
        if batch.enc.is_none() {
            let pass = MTLRenderPassDescriptor::renderPassDescriptor();
            pass.setRenderTargetWidth(TARGET);
            pass.setRenderTargetHeight(TARGET);
            pass.setDefaultRasterSampleCount(1);
            batch.enc = Some(batch.cmd.renderCommandEncoderWithDescriptor(&pass)?);
            batch.written.clear();
        }
        batch.enc.clone()
    }

    /// The pipeline for a draw, or None if the CPU must draw it.
    pub fn prepare(&mut self, r: &Raster, cap: &Captured) -> Option<Pipeline> {
        let copies: usize = r.textures.iter().map(|t| match t {
            Some(Texture { mem: TexMem::Copy(v), .. }) => v.len() + ALIGN,
            _ => 0,
        }).sum();
        if cap.slots.len() > MAX_SLOTS || copies + size_of::<Uniforms>() + (1 << 20) > ARENA_SIZE {
            self.fallbacks += 1;
            return None;
        }
        let p = self.pipeline(r, &cap.slots);
        if p.is_none() {
            self.fallbacks += 1;
        }
        p
    }

    /// Queues the captured triangles of a draw.
    pub fn draw(&mut self, r: &Raster, cap: &Captured, pipeline: &Pipeline) {
        let [x0, y0, x1, y1] = r.bounds();
        if cap.vertices == 0 || x0 > x1 || y0 > y1 {
            return;
        }
        let stride = cap.stride();
        let per_chunk = ((ARENA_SIZE / 2) / (stride * 4) / 3 * 3).max(3);
        for chunk in cap.floats.chunks(per_chunk * stride) {
            self.draw_chunk(r, chunk, chunk.len() / stride, pipeline, [x0, y0, x1, y1]);
        }
    }

    fn draw_chunk(&mut self, r: &Raster, floats: &[f32], vertices: usize, pipeline: &Pipeline, [x0, y0, x1, y1]: [i32; 4]) {
        let mut u = self.uniforms(r);
        let reads: Vec<(usize, usize)> = r
            .textures
            .iter()
            .filter_map(|t| match (t, t.as_ref().map(|t| &t.mem)) {
                (Some(t), Some(TexMem::Vram(o))) => Some((*o, o + t.span())),
                _ => None,
            })
            .collect();
        let writes: Vec<(usize, usize)> = r
            .colour
            .iter()
            .map(|cb| {
                let bpp = colour_bpp(cb.format);
                (cb.offset + y0 as usize * cb.pitch * bpp, cb.offset + (y1 as usize + 1) * cb.pitch * bpp)
            })
            .chain(r.depth.iter().map(|d| {
                let bpp = if d.z16 { 2 } else { 4 };
                (d.offset + y0 as usize * d.pitch * bpp, d.offset + (y1 as usize + 1) * d.pitch * bpp)
            }))
            .collect();
        let floats: &[u8] = unsafe { std::slice::from_raw_parts(floats.as_ptr().cast::<u8>(), floats.len() * 4) };
        let mut copies: Vec<(usize, &[u8])> = Vec::new();
        for (i, t) in r.textures.iter().enumerate() {
            if let Some(Texture { mem: TexMem::Copy(v), .. }) = t {
                copies.push((i, v));
            }
        }
        let need = floats.len() + copies.iter().map(|c| c.1.len() + ALIGN).sum::<usize>() + size_of::<Uniforms>() + 4 * ALIGN;
        if self.batch.as_ref().is_some_and(|b| b.arena.used + need > ARENA_SIZE) {
            self.kick();
        }
        let counter = self.counter.clone();
        let vram = self.vram.clone();
        let Some(batch) = self.open() else { return };
        for (i, data) in copies {
            let Some(at) = batch.arena.put(data) else { return };
            u.tex[i].stage = 1;
            u.tex[i].base = at as u32;
            u.tex[i].limit = ARENA_SIZE as u32;
        }
        let Some(vb) = batch.arena.put(floats) else { return };
        let Some(ub) = batch.arena.put(bytes_of(&u)) else { return };
        if reads.iter().any(|&(a, b)| batch.written.iter().any(|&(c, d)| a < d && c < b)) {
            if let Some(enc) = batch.enc.take() {
                enc.endEncoding();
            }
        }
        let Some(enc) = Self::encoder(batch) else { return };
        batch.written.extend(writes);
        let arena = batch.arena.buf.clone();
        enc.setRenderPipelineState(pipeline);
        unsafe {
            enc.setVertexBuffer_offset_atIndex(Some(&arena), vb, 0);
            enc.setFragmentBuffer_offset_atIndex(Some(&vram), 0, 0);
            enc.setFragmentBuffer_offset_atIndex(Some(&vram), 0, 1);
            enc.setFragmentBuffer_offset_atIndex(Some(&arena), 0, 2);
            enc.setFragmentBuffer_offset_atIndex(Some(&arena), ub, 3);
            enc.setFragmentBuffer_offset_atIndex(Some(&counter), 0, 4);
        }
        enc.setViewport(MTLViewport { originX: 0.0, originY: 0.0, width: TARGET as f64, height: TARGET as f64, znear: 0.0, zfar: 1.0 });
        enc.setScissorRect(MTLScissorRect { x: x0 as usize, y: y0 as usize, width: (x1 - x0 + 1) as usize, height: (y1 - y0 + 1) as usize });
        unsafe { enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, vertices) };
        self.draws += 1;
    }

    fn uniforms(&self, r: &Raster) -> Uniforms {
        let mut u = Uniforms {
            consts: [[0.0; 4]; 32],
            tex: [TexDesc::default(); 16],
            cb: [ColourDesc::default(); 4],
            ncb: r.colour.len().min(4) as u32,
            has_depth: r.depth.is_some() as u32,
            depth_base: r.depth.map_or(0, |d| d.offset as u32),
            depth_pitch: r.depth.map_or(0, |d| d.pitch as u32),
            z16: r.depth.is_some_and(|d| d.z16) as u32,
            zb_cntl: r.zb_cntl,
            zs_cntl: r.zs_cntl,
            stencil_ref: r.stencil_ref,
            depth_scale: r.depth_scale,
            depth_bias: r.depth_offset,
            clips: [0; 16],
            clip_rule: r.clip_rule,
            blend: r.blend,
            ablend: r.ablend,
            channel_mask: r.channel_mask,
            constant_colour: r.constant,
            rop: r.rop,
            alpha_func: r.alpha_func,
            out_fmt: r.out_fmt,
            vram_size: self.vram_len as u32,
        };
        for (i, c) in r.us.consts.iter().take(32).enumerate() {
            u.consts[i] = *c;
        }
        for (k, c) in r.clips.iter().enumerate() {
            u.clips[4 * k..4 * k + 4].copy_from_slice(c);
        }
        for (i, cb) in r.colour.iter().take(4).enumerate() {
            u.cb[i] = ColourDesc { offset: cb.offset as u32, pitch: cb.pitch as u32, format: cb.format, pad: 0 };
        }
        for (i, t) in r.textures.iter().enumerate() {
            let Some(t) = t else { continue };
            let d = &mut u.tex[i];
            d.enabled = 1;
            d.base = match t.mem {
                TexMem::Vram(o) => o as u32,
                TexMem::Copy(_) => 0,
            };
            d.limit = self.vram_len as u32;
            d.width = t.width;
            d.height = t.height;
            d.format = t.format;
            d.nlevels = t.levels.len().min(12) as u32;
            d.filter = t.filter;
            d.border = t.border;
            d.sel = t.sel;
            for (l, &(off, pitch)) in t.levels.iter().take(12).enumerate() {
                d.off[l] = off as u32;
                d.pitch[l] = pitch as u32;
            }
        }
        u
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (self.draws, self.fallbacks, self.waits)
    }

    /// Submits the queued draws without waiting for them.
    pub fn kick(&mut self) {
        let Some(mut b) = self.batch.take() else { return };
        if let Some(enc) = b.enc.take() {
            enc.endEncoding();
        }
        b.cmd.commit();
        self.inflight.push((b.cmd, b.arena));
    }

    /// Submits the queued draws, waits for every draw to finish, and returns
    /// how many pixels passed the depth and stencil tests since the last
    /// call.
    pub fn sync(&mut self) -> u32 {
        self.kick();
        if self.inflight.is_empty() {
            return 0;
        }
        self.waits += 1;
        for (cmd, mut arena) in self.inflight.drain(..) {
            cmd.waitUntilCompleted();
            if let Some(e) = cmd.error() {
                eprintln!("radeon: Metal command buffer failed: {}", e.localizedDescription());
            }
            arena.used = 0;
            self.free.push(arena);
        }
        unsafe {
            let p = self.counter.contents().as_ptr().cast::<u32>();
            let n = p.read_volatile();
            p.write_volatile(0);
            n
        }
    }

}

fn colour_bpp(format: u32) -> usize {
    match format {
        9 => 1,
        3 | 4 | 13 | 15 => 2,
        _ => 4,
    }
}

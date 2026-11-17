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

use super::{msl, us};
use super::raster::{Captured, Raster};
use super::zmask::Fill;
use super::tex::{TexMem, Texture};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

/// Pixel stack registers a draw may interpolate; more go to the CPU.
const MAX_SLOTS: usize = 24;
const ARENA_SIZE: usize = 32 << 20;
/// Command buffers in flight at most. Each holds an arena, so this bounds
/// the memory a GPU that falls behind the guest can tie up; past it, a kick
/// waits for the oldest one, as a driver's ring would fill up on the card.
const MAX_INFLIGHT: usize = 4;
/// Pipelines kept at most. Each compiled variant costs the driver about a
/// megabyte; past this the cache starts over.
const MAX_PIPELINES: usize = 1024;
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
    signed: u32,
    kind: u32,
    depth: u32,
    off: [u32; 12],
    pitch: [u32; 12],
    slice: [u32; 12],
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
    cb_clear: u32,
    cb_clear_value: u32,
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
    fog: u32,
    fog_factor: f32,
    fog_colour: [f32; 3],
}

type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
pub(crate) type Pipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
type ComputePipeline = Retained<ProtocolObject<dyn MTLComputePipelineState>>;

const FILL_MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct FillArgs { uint offset; uint stride; uint bpp; uint value; };
kernel void fill(device uchar *vram [[buffer(0)]], constant FillArgs &a [[buffer(1)]],
                 uint2 id [[thread_position_in_grid]]) {
    uint at = a.offset + id.y * a.stride + id.x * a.bpp;
    if (a.bpp == 2) {
        *(device ushort *)(vram + at) = ushort(a.value);
    } else {
        *(device uint *)(vram + at) = a.value;
    }
}
"#;

#[repr(C)]
struct FillArgs {
    offset: u32,
    stride: u32,
    bpp: u32,
    value: u32,
}

struct Arena {
    buf: Buffer,
    used: usize,
    /// VRAM ranges the command buffer using this arena writes.
    dirty: Vec<(usize, usize)>,
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
    /// Built on first use; None inside if it failed.
    fill_pipeline: Option<Option<ComputePipeline>>,
    batch: Option<Batch>,
    /// Committed command buffers, oldest first, with their serial numbers.
    inflight: std::collections::VecDeque<(Retained<ProtocolObject<dyn MTLCommandBuffer>>, Arena, u64)>,
    committed: u64,
    completed: u64,
    free: Vec<Arena>,
    draws: u64,
    fallbacks: u64,
    waits: u64,
    /// Kicks that waited because MAX_INFLIGHT command buffers were queued.
    throttles: u64,
}

// The Metal objects are only touched while the card's owner holds it.
unsafe impl Send for MetalR3d {}

impl Drop for MetalR3d {
    fn drop(&mut self) {
        if let Some(enc) = self.batch.as_mut().and_then(|b| b.enc.take()) {
            enc.endEncoding();
        }
    }
}

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
            fill_pipeline: None,
            batch: None,
            inflight: Default::default(),
            committed: 0,
            completed: 0,
            free: Vec::new(),
            draws: 0,
            fallbacks: 0,
            waits: 0,
            throttles: 0,
        })
    }

    /// The pipeline for a fragment program, built from MSL the first time.
    /// The key holds only the instructions the program runs, as the MSL
    /// does: drivers leave stale words in the slots around a program, and
    /// keying on them built a new pipeline for every combination.
    fn pipeline(&mut self, r: &Raster, slots: &[usize]) -> Option<Pipeline> {
        let key = pipeline_key(&r.us, slots, r.perspective);
        if let Some(p) = self.pipelines.get(&key) {
            return p.clone();
        }
        if self.pipelines.len() >= MAX_PIPELINES {
            self.pipelines.clear();
        }
        let built = self.build(&msl::shader(&r.us, slots, r.perspective));
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
        Some(Arena { buf, used: 0, dirty: Vec::new() })
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
    fn prepare_inner(&mut self, r: &Raster, cap: &Captured) -> Option<Pipeline> {
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
    fn draw_inner(&mut self, r: &Raster, cap: &Captured, pipeline: &Pipeline) {
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
        for &w in &writes {
            if batch.arena.dirty.last() != Some(&w) {
                batch.arena.dirty.push(w);
            }
        }
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
            cb_clear: r.cb_clear.is_some() as u32,
            cb_clear_value: r.cb_clear.unwrap_or(0),
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
            fog: r.fog,
            fog_factor: r.fog_factor,
            fog_colour: r.fog_colour,
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
            d.signed = t.signed;
            d.kind = t.kind;
            d.depth = t.depth;
            for (l, &n) in t.slices.iter().take(12).enumerate() {
                d.slice[l] = n as u32;
            }
            for (l, &(off, pitch)) in t.levels.iter().take(12).enumerate() {
                d.off[l] = off as u32;
                d.pitch[l] = pitch as u32;
            }
        }
        u
    }

    fn fill_pipeline(&mut self) -> Option<ComputePipeline> {
        if self.fill_pipeline.is_none() {
            let built = (|| {
                let lib = self.device.newLibraryWithSource_options_error(&NSString::from_str(FILL_MSL), None).ok()?;
                let f = lib.newFunctionWithName(&NSString::from_str("fill"))?;
                self.device.newComputePipelineStateWithFunction_error(&f).ok()
            })();
            if built.is_none() {
                eprintln!("radeon: Metal fill pipeline failed; filling on the CPU instead");
            }
            self.fill_pipeline = Some(built);
        }
        self.fill_pipeline.clone().flatten()
    }

    /// Queues a write of `value` into each `bpp`-byte pixel of `f`, after
    /// the draws queued so far. False if the CPU must do it.
    fn fill_inner(&mut self, f: Fill, bpp: usize, value: u32) -> bool {
        let Some(pipeline) = self.fill_pipeline() else { return false };
        let vram = self.vram.clone();
        let Some(batch) = self.open() else { return false };
        if let Some(enc) = batch.enc.take() {
            enc.endEncoding();
        }
        let Some(enc) = batch.cmd.computeCommandEncoder() else { return false };
        batch.arena.dirty.push((f.offset, f.offset + (f.rows.max(1) - 1) * f.stride + f.width));
        let args = FillArgs { offset: f.offset as u32, stride: f.stride as u32, bpp: bpp as u32, value };
        enc.setComputePipelineState(&pipeline);
        unsafe {
            enc.setBuffer_offset_atIndex(Some(&vram), 0, 0);
            enc.setBytes_length_atIndex(NonNull::from(&args).cast(), size_of::<FillArgs>(), 1);
        }
        let grid = MTLSize { width: f.width / bpp, height: f.rows, depth: 1 };
        enc.dispatchThreads_threadsPerThreadgroup(grid, MTLSize { width: 64, height: 1, depth: 1 });
        enc.endEncoding();
        true
    }

    /// True if queued or running work writes VRAM between `start` and `end`.
    pub fn writes_pending(&mut self, start: usize, end: usize) -> bool {
        self.completed();
        let hit = |a: &Arena| a.dirty.iter().any(|&(s, e)| s < end && start < e);
        self.batch.as_ref().is_some_and(|b| hit(&b.arena)) || self.inflight.iter().any(|(_, a, _)| hit(a))
    }

    /// True while draws are queued or running.
    pub fn busy(&self) -> bool {
        self.batch.is_some() || !self.inflight.is_empty()
    }

    pub fn pipelines(&self) -> usize {
        self.pipelines.len()
    }

    pub fn throttles(&self) -> u64 {
        self.throttles
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (self.draws, self.fallbacks, self.waits)
    }

    /// Submits the queued draws without waiting for them and returns the
    /// serial number of the last command buffer submitted.
    fn kick_inner(&mut self) -> u64 {
        if let Some(mut b) = self.batch.take() {
            if let Some(enc) = b.enc.take() {
                enc.endEncoding();
            }
            b.cmd.commit();
            self.committed += 1;
            self.inflight.push_back((b.cmd, b.arena, self.committed));
        }
        while self.inflight.len() > MAX_INFLIGHT {
            let (cmd, arena, serial) = self.inflight.pop_front().unwrap();
            self.throttles += 1;
            cmd.waitUntilCompleted();
            self.retire(&cmd, arena, serial);
        }
        self.committed
    }

    fn retire(&mut self, cmd: &ProtocolObject<dyn MTLCommandBuffer>, mut arena: Arena, serial: u64) {
        if let Some(e) = cmd.error() {
            eprintln!("radeon: Metal command buffer failed: {}", e.localizedDescription());
        }
        arena.used = 0;
        arena.dirty.clear();
        if self.free.len() <= MAX_INFLIGHT {
            self.free.push(arena);
        }
        self.completed = serial;
    }

    /// The serial number of the last command buffer that has finished,
    /// without waiting.
    fn completed_inner(&mut self) -> u64 {
        while let Some((cmd, _, _)) = self.inflight.front() {
            if !matches!(cmd.status(), MTLCommandBufferStatus::Completed | MTLCommandBufferStatus::Error) {
                break;
            }
            let (cmd, arena, serial) = self.inflight.pop_front().unwrap();
            self.retire(&cmd, arena, serial);
        }
        self.completed
    }

    /// Submits the queued draws, waits for every draw to finish, and returns
    /// how many pixels passed the depth and stencil tests since the last
    /// call.
    fn sync_inner(&mut self) -> u32 {
        self.kick();
        if self.inflight.is_empty() {
            return 0;
        }
        self.waits += 1;
        while let Some((cmd, arena, serial)) = self.inflight.pop_front() {
            cmd.waitUntilCompleted();
            self.retire(&cmd, arena, serial);
        }
        unsafe {
            let p = self.counter.contents().as_ptr().cast::<u32>();
            let n = p.read_volatile();
            p.write_volatile(0);
            n
        }
    }

}


/// Metal returns autoreleased objects (command buffers, encoders, pass
/// descriptors). The command processor thread has no run loop to drain them,
/// so every entry point runs in its own pool; without one, each command
/// buffer and the commands encoded into it lived until the thread exited.
impl MetalR3d {
    pub fn prepare(&mut self, r: &Raster, cap: &Captured) -> Option<Pipeline> {
        autoreleasepool(|_| self.prepare_inner(r, cap))
    }

    pub fn draw(&mut self, r: &Raster, cap: &Captured, pipeline: &Pipeline) {
        autoreleasepool(|_| self.draw_inner(r, cap, pipeline))
    }

    pub fn fill(&mut self, f: Fill, bpp: usize, value: u32) -> bool {
        autoreleasepool(|_| self.fill_inner(f, bpp, value))
    }

    pub fn kick(&mut self) -> u64 {
        autoreleasepool(|_| self.kick_inner())
    }

    pub fn completed(&mut self) -> u64 {
        autoreleasepool(|_| self.completed_inner())
    }

    pub fn sync(&mut self) -> u32 {
        autoreleasepool(|_| self.sync_inner())
    }
}

pub(crate) fn pipeline_key(us: &us::Program, slots: &[usize], perspective: bool) -> Vec<u32> {
    let mut key = vec![perspective as u32, slots.len() as u32];
    key.extend(slots.iter().map(|&s| s as u32));
    for n in &us.nodes {
        if let Some((start, len)) = n.tex {
            let tex = us.tex.get(start..(start + len).min(us.tex.len())).unwrap_or(&[]);
            key.push(tex.len() as u32);
            key.extend(tex);
        } else {
            key.push(0);
        }
        let (start, len) = n.alu;
        let alu = us.alu.get(start..(start + len).min(us.alu.len())).unwrap_or(&[]);
        key.push(alu.len() as u32);
        key.extend(alu.iter().flatten());
    }
    key
}

fn colour_bpp(format: u32) -> usize {
    match format {
        9 => 1,
        3 | 4 | 13 | 15 => 2,
        _ => 4,
    }
}

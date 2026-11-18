//! The R300 3D engine, run on the CPU: the 3D_DRAW packets, vertex fetch
//! through the programmable stream control (PSC), the vertex shader, the
//! viewport transform, primitive assembly and the rasterizer and back end.
//!
//! Drawing happens while the packet executes, so fences, scratch writebacks
//! and WAIT_UNTIL that follow it always see finished pixels. Like the 2D
//! engine, the 3D engine treats every surface as linear and ignores the
//! tiling bits, which keeps what it draws consistent with the CRTC and the
//! 2D engine.
//!
//! Packet layouts follow AMD's R5xx Acceleration guide (v1.5, section 6.2.3)
//! and registers its R3xx 3D register reference.

#[cfg(target_os = "macos")]
pub(crate) mod metal;
#[cfg(target_os = "macos")]
mod metal_tex;
#[cfg(not(target_os = "macos"))]
#[path = "metal_none.rs"]
pub(crate) mod metal;
mod msl;
mod pvs;
mod pvs_msl;
#[cfg(all(test, target_os = "macos"))]
mod pvs_tests;
mod raster;
pub(crate) mod regs;
mod tex;
mod us;
mod vsgen;
mod zmask;

#[cfg(test)]
mod tests;

use crate::Radeon;
use crate::mem::{BusMemory, Place};
use raster::{Captured, ColourBuffer, DepthBuffer, Raster, Vertex};
use regs::*;
use tex::{TexMem, Texture};

pub(crate) type V4 = [f32; 4];

/// The smallest w the setup engine divides by; anything nearer is clipped.
const NEAR_W: f32 = 1e-6;

/// VAP_VTE_CNTL and the viewport transform it enables.
#[derive(Clone, Copy)]
struct Viewport {
    vte: u32,
    scale: [f32; 3],
    offset: [f32; 3],
    ga: (f32, f32),
    /// GB_SELECT's fog source, then GA_FOG_SCALE and GA_FOG_OFFSET.
    fog: (u32, f32, f32),
}

impl Viewport {
    /// True if positions arrive in clip space and need the perspective divide.
    fn clips(&self) -> bool {
        self.vte & (1 << 8) == 0 && self.vte & (1 << 10) != 0
    }

    /// Window coordinates and 1/w of a vertex, and its fog value.
    fn apply(&self, mut v: Vertex) -> Vertex {
        let vte = self.vte;
        let w = v.pos[3];
        let rcp = if vte & (1 << 10) != 0 { if w != 0.0 { 1.0 / w } else { 0.0 } } else { w };
        let [mut x, mut y, mut z, _] = v.pos;
        if vte & (1 << 8) == 0 {
            x *= rcp;
            y *= rcp;
        }
        if vte & (1 << 9) == 0 {
            z *= rcp;
        }
        let mut c = [x, y, z];
        for (i, c) in c.iter_mut().enumerate() {
            if vte & (1 << (2 * i)) != 0 {
                *c *= self.scale[i];
            }
            if vte & (2 << (2 * i)) != 0 {
                *c += self.offset[i];
            }
        }
        let (select, scale, offset) = self.fog;
        let source = match select {
            k @ 0..=3 => v.colours[k as usize][3],
            4 if vte & (1 << 10) != 0 => w,
            4 => 1.0 / w,
            _ => c[2],
        };
        v.fog = source * scale + offset;
        v.pos = [c[0] + self.ga.0, c[1] + self.ga.1, c[2], rcp];
        v
    }
}

fn lerp(a: &Vertex, b: &Vertex, t: f32) -> Vertex {
    let mix = |x: V4, y: V4| [0, 1, 2, 3].map(|i| x[i] + (y[i] - x[i]) * t);
    Vertex {
        pos: mix(a.pos, b.pos),
        colours: [0, 1, 2, 3].map(|i| mix(a.colours[i], b.colours[i])),
        tex: std::array::from_fn(|i| mix(a.tex[i], b.tex[i])),
        psize: a.psize,
        fog: a.fog + (b.fog - a.fog) * t,
    }
}

/// A clip plane: a vertex is inside where `dot(n, pos) >= bias`.
type Plane = ([f32; 4], f32);

/// The planes the VAP clips triangles against, in clip space. With
/// VAP_CLIP_CNTL's CLIP_DISABLE only w > 0 is kept. Otherwise the near
/// plane is z = 0 under VAP_CNTL's DX_CLIP_SPACE_DEF and z = -w without,
/// the far plane z = w, and x and y are clipped to the guard band the
/// VAP_GB_*_CLIP_ADJ registers give in multiples of w. Register meanings
/// follow Mesa's r300 driver (r300_reg.h, r300_emit.c's guard band and
/// VAP_CNTL state; MIT license, Jerome Glisse, Corbin Simpson, Marek Olšák
/// and others).
fn clip_planes(enabled: bool, dx: bool, horz: f32, vert: f32) -> Vec<Plane> {
    let mut planes = vec![([0.0, 0.0, 0.0, 1.0], NEAR_W)];
    if enabled {
        planes.push(([0.0, 0.0, 1.0, if dx { 0.0 } else { 1.0 }], 0.0));
        planes.push(([0.0, 0.0, -1.0, 1.0], 0.0));
        for (axis, adj) in [(0, horz), (1, vert)] {
            if adj > 0.0 && adj.is_finite() {
                for sign in [1.0, -1.0] {
                    let mut n = [0.0, 0.0, 0.0, adj];
                    n[axis] = sign;
                    planes.push((n, 0.0));
                }
            }
        }
    }
    planes
}

fn distance((n, bias): &Plane, v: &Vertex) -> f32 {
    n[0] * v.pos[0] + n[1] * v.pos[1] + n[2] * v.pos[2] + n[3] * v.pos[3] - bias
}

/// The part of a clip-space triangle inside every plane, as a polygon.
fn clip(t: &[Vertex; 3], planes: &[Plane]) -> Vec<Vertex> {
    let mut poly = t.to_vec();
    for plane in planes {
        if poly.iter().all(|v| distance(plane, v) >= 0.0) {
            continue;
        }
        let mut out = Vec::with_capacity(poly.len() + 1);
        for i in 0..poly.len() {
            let (a, b) = (&poly[i], &poly[(i + 1) % poly.len()]);
            let (da, db) = (distance(plane, a), distance(plane, b));
            if da >= 0.0 {
                out.push(*a);
            }
            if da >= 0.0 && db < 0.0 {
                out.push(lerp(a, b, da / (da - db)));
            } else if da < 0.0 && db >= 0.0 {
                out.push(lerp(b, a, db / (db - da)));
            }
        }
        if out.len() < 3 {
            return Vec::new();
        }
        poly = out;
    }
    poly
}

pub(crate) mod op {
    pub const DRAW_VBUF: u8 = 0x28;
    pub const DRAW_IMMD: u8 = 0x29;
    pub const DRAW_INDX: u8 = 0x2a;
    pub const LOAD_VBPNTR: u8 = 0x2f;
    pub const INDX_BUFFER: u8 = 0x33;
    pub const DRAW_VBUF_2: u8 = 0x34;
    pub const DRAW_IMMD_2: u8 = 0x35;
    pub const DRAW_INDX_2: u8 = 0x36;
    pub const CLEAR_ZMASK: u8 = 0x32;
    pub const CLEAR_HIZ: u8 = 0x37;
    pub const DRAW_128: u8 = 0x39;
}

const VECMEM_DWORDS: usize = 2048 * 4;
const MAX_VERTICES: usize = 1 << 20;

const FENCE_POLL: std::time::Duration = std::time::Duration::from_micros(20);

/// Scratch register writes waiting for Metal: (command buffer serial,
/// register, value), and registers whose writeback is due.
#[derive(Default)]
pub(crate) struct Fences {
    queue: std::collections::VecDeque<(u64, u32, u32)>,
    writebacks: Vec<u32>,
    polled: Option<std::time::Instant>,
}

impl Fences {
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[derive(snapshot::Snap)]
pub(crate) struct ThreeD {
    /// The VAP's vector memory: vertex program code and constants.
    vecmem: Vec<u32>,
    /// Next dword VAP_PVS_VECTOR_DATA_REG writes.
    vec_ptr: u32,
    /// VAP_VF_CNTL of a 3D_DRAW_INDX that waits for INDX_BUFFER's indices.
    pending_indx: Option<u32>,
    zmask: zmask::ZMask,
    pub draws: u64,
    /// CRTC_OFFSET writes, which is how drivers flip buffers.
    #[snap(skip)]
    pub flips: u64,
}

impl Default for ThreeD {
    fn default() -> Self {
        ThreeD { vecmem: vec![0; VECMEM_DWORDS], vec_ptr: 0, pending_indx: None, zmask: zmask::ZMask::default(), draws: 0, flips: 0 }
    }
}

/// VAP_PROG_STREAM_CNTL_0..7 and their EXT registers.
#[derive(Clone, Copy)]
struct Psc {
    cntl: [u32; 8],
    ext: [u32; 8],
}

impl Psc {
    /// Splits a vertex's dwords into input vectors.
    fn inputs(&self, dwords: &[u32]) -> [V4; pvs::INPUTS] {
        let mut inputs = [[0.0, 0.0, 0.0, 1.0]; pvs::INPUTS];
        let mut at = 0;
        for e in 0..16u32 {
            let cntl = self.cntl[e as usize / 2] >> (16 * (e % 2));
            let ext = self.ext[e as usize / 2] >> (16 * (e % 2));
            let kind = cntl & 15;
            let signed = cntl & (1 << 14) != 0;
            let normalize = cntl & (1 << 15) != 0;
            let d = |i: usize| dwords.get(at + i).copied().unwrap_or(0);
            let (raw, used): (V4, usize) = match kind {
                0..=3 => {
                    let n = kind as usize + 1;
                    let mut v = [0.0, 0.0, 0.0, 1.0];
                    for (c, x) in v.iter_mut().enumerate().take(n) {
                        *x = f32::from_bits(d(c));
                    }
                    (v, n)
                }
                4 | 5 => {
                    let w = d(0);
                    let mut v = [0, 8, 16, 24].map(|s| fixed(w >> s, 8, signed, normalize));
                    if kind == 5 {
                        v.swap(0, 2);
                    }
                    (v, 1)
                }
                6 => {
                    let w = d(0);
                    ([fixed(w, 16, signed, normalize), fixed(w >> 16, 16, signed, normalize), 0.0, 1.0], 1)
                }
                7 => {
                    let (w0, w1) = (d(0), d(1));
                    ([w0, w0 >> 16, w1, w1 >> 16].map(|x| fixed(x, 16, signed, normalize)), 2)
                }
                8 => {
                    let w = d(0);
                    ([fixed(w, 10, signed, normalize), fixed(w >> 10, 10, signed, normalize), fixed(w >> 20, 10, signed, normalize), 1.0], 1)
                }
                _ => {
                    let w = d(0);
                    ([fixed(w, 11, signed, normalize), fixed(w >> 11, 11, signed, normalize), fixed(w >> 22, 10, signed, normalize), 1.0], 1)
                }
            };
            let dst = bits(cntl, 8, 5) as usize;
            let mask = bits(ext, 12, 4);
            for c in 0..4 {
                if mask & (1 << c) != 0 {
                    inputs[dst][c] = match bits(ext, 3 * c as u32, 3) {
                        s @ 0..=3 => raw[s as usize],
                        5 => 1.0,
                        _ => 0.0,
                    };
                }
            }
            at += used + bits(cntl, 4, 4) as usize;
            if cntl & (1 << 13) != 0 {
                break;
            }
        }
        inputs
    }
}

/// The distinct indices in `idx`, in first-use order, and where each entry
/// of `idx` is among them.
fn unique_indices(idx: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let mut slot = std::collections::HashMap::with_capacity(idx.len() / 2);
    let mut unique = Vec::new();
    let remap = idx
        .iter()
        .map(|&i| {
            *slot.entry(i).or_insert_with(|| {
                unique.push(i);
                unique.len() as u32 - 1
            })
        })
        .collect();
    (unique, remap)
}

/// Draws with fewer vertices run the vertex program on the calling thread.
const PARALLEL_VERTICES: usize = 2048;

enum Verts<'a> {
    Immediate(&'a [u32]),
    List,
    Indices(Vec<u32>),
}

fn bits(v: u32, shift: u32, n: u32) -> u32 {
    (v >> shift) & ((1 << n) - 1)
}

/// Converts a fixed-point vertex component as the PSC's SIGNED and
/// NORMALIZE flags say.
fn fixed(raw: u32, n: u32, signed: bool, normalize: bool) -> f32 {
    let max = ((1u64 << n) - 1) as f32;
    if signed {
        let v = ((raw << (32 - n)) as i32 >> (32 - n)) as f32;
        if normalize { (v / ((1u64 << (n - 1)) - 1) as f32).max(-1.0) } else { v }
    } else {
        let v = (raw & ((1u64 << n) - 1) as u32) as f32;
        if normalize { v / max } else { v }
    }
}

impl Radeon {
    /// Draws the 3D engine's pixels with Metal from now on, if this Mac can.
    pub fn use_metal(&mut self) -> bool {
        self.metal = metal::MetalR3d::new(self.vga.vram_mut()).map(Box::new);
        self.metal.is_some()
    }

    /// Waits until every queued Metal draw has reached VRAM. Everything that
    /// reads or writes VRAM outside the Metal pipeline, or lets the guest
    /// see that drawing finished, calls this first.
    pub fn gpu_sync(&mut self) {
        if let Some(m) = self.metal.as_mut() {
            m.vram_written();
            let passed = m.sync();
            let r = &mut self.regs[ZB_ZPASS_DATA as usize / 4];
            *r = r.wrapping_add(passed);
            self.retire_fences(u64::MAX);
        }
    }

    /// Like `gpu_sync`, and also writes back the scratch registers.
    pub fn gpu_finish(&mut self, mem: &mut dyn BusMemory) {
        self.gpu_sync();
        self.flush_writebacks(mem);
    }

    /// Holds back scratch register `n`'s new value `v`, and its writeback,
    /// until the draws queued so far have finished, as the card's fences
    /// arrive only once the pipeline drains. False if nothing is queued and
    /// the write can happen now.
    pub(crate) fn defer_fence(&mut self, n: u32, v: u32) -> bool {
        let Some(m) = self.metal.as_mut() else { return false };
        if !m.busy() && self.fences.queue.is_empty() {
            return false;
        }
        let serial = m.kick();
        self.fences.queue.push_back((serial, n, v));
        true
    }

    /// True while fences wait for Metal or for their writeback.
    pub fn fences_pending(&self) -> bool {
        !self.fences.queue.is_empty() || !self.fences.writebacks.is_empty()
    }

    /// Applies the fences whose draws have finished by now, and writes back
    /// the scratch registers they changed.
    pub fn poll_fences(&mut self, mem: &mut dyn BusMemory) {
        if self.fences.queue.is_empty() && self.fences.writebacks.is_empty() {
            return;
        }
        let done = self.metal.as_mut().map_or(u64::MAX, |m| m.completed());
        self.retire_fences(done);
        self.flush_writebacks(mem);
    }

    /// As `poll_fences`, but at most every `FENCE_POLL`, for register writes:
    /// asking Metal on every one took a quarter of the command processor's
    /// time in 3DMark 2001.
    pub(crate) fn poll_fences_sometimes(&mut self, mem: &mut dyn BusMemory) {
        if !self.fences_pending() {
            return;
        }
        let now = std::time::Instant::now();
        if self.fences.polled.is_some_and(|t| now.duration_since(t) < FENCE_POLL) {
            return;
        }
        self.fences.polled = Some(now);
        self.poll_fences(mem);
    }

    pub(crate) fn retire_fences(&mut self, done: u64) {
        while let Some(&(serial, n, v)) = self.fences.queue.front() {
            if serial > done {
                break;
            }
            self.fences.queue.pop_front();
            self.regs[(crate::regs::SCRATCH_REG0 / 4 + n) as usize] = v;
            if !self.fences.writebacks.contains(&n) {
                self.fences.writebacks.push(n);
            }
        }
    }

    fn flush_writebacks(&mut self, mem: &mut dyn BusMemory) {
        for n in std::mem::take(&mut self.fences.writebacks) {
            self.scratch_written(n, mem);
        }
    }

    /// The 3D engine's counters for the status report.
    pub fn describe(&self) -> String {
        let mut s = format!("radeon: 3d draws {} flips {}", self.r3d.draws, self.r3d.flips);
        match self.metal_stats() {
            Some((d, f, w)) => {
                let t = self.metal.as_ref().map_or(0, |m| m.throttles());
                let c = self.metal.as_ref().map_or(0, |m| m.committed());
                let p = self.metal.as_ref().map_or(0, |m| m.pipelines());
                let ms = self.metal.as_ref().map_or(0, |m| m.gpu_ms());
                let (tn, tu) = self.metal.as_ref().map_or((0, 0), |m| m.texture_stats());
                let vd = self.metal.as_ref().map_or(0, |m| m.vertex_draws());
                s += &format!(" | metal draws {d} gpu vertices {vd} cpu fallbacks {f} waits {w} command buffers {c} throttled {t} pipelines {p} gpu ms {ms} metal textures {tn} dxt copies {tu}")
            }
            None => s += " | cpu",
        }
        s
    }

    /// Draws Metal took, draws it left to the CPU, and how often the CPU
    /// waited for it.
    pub fn metal_stats(&self) -> Option<(u64, u64, u64)> {
        self.metal.as_ref().map(|m| m.stats())
    }

    /// Notes that the guest or the CP may have written VRAM, so Metal's
    /// texture copies are checked against it again.
    pub(crate) fn vram_written(&mut self) {
        if let Some(m) = self.metal.as_mut() {
            m.vram_written();
        }
    }

    /// Starts the queued Metal draws without waiting for them.
    pub(crate) fn gpu_kick(&mut self) {
        if let Some(m) = self.metal.as_mut() {
            m.kick();
        }
    }

    pub(crate) fn r3d_register_written(&mut self, reg: u32, v: u32) -> bool {
        match reg {
            VAP_VPORT_ALIAS..=VAP_VPORT_ALIAS_END => {
                self.regs[(reg - VAP_VPORT_ALIAS + VAP_VPORT_XSCALE) as usize / 4] = v;
            }
            VAP_VPORT_XSCALE..=VAP_VPORT_ZOFFSET => {
                self.regs[(reg - VAP_VPORT_XSCALE + VAP_VPORT_ALIAS) as usize / 4] = v;
            }
            VAP_PVS_VECTOR_INDX_REG => self.r3d.vec_ptr = (v & 0x7ff) * 4,
            crate::regs::CRTC_OFFSET => {
                self.r3d.flips += 1;
                return false;
            }
            VAP_PVS_VECTOR_DATA_REG | VAP_PVS_VECTOR_DATA_REG_128 => {
                let p = self.r3d.vec_ptr as usize % VECMEM_DWORDS;
                self.r3d.vecmem[p] = v;
                self.r3d.vec_ptr = (p as u32 + 1) % VECMEM_DWORDS as u32;
            }
            _ => return false,
        }
        true
    }

    /// Writes the occlusion counter to memory when ZB_ZPASS_ADDR is written.
    pub(crate) fn zpass_written(&mut self, reg: u32, v: u32, mem: &mut dyn BusMemory) -> bool {
        if reg != ZB_ZPASS_ADDR {
            return false;
        }
        let count = self.reg(ZB_ZPASS_DATA);
        self.mc_write_dword(mem, v & !3, count);
        true
    }

    /// Runs a 3D packet; false if `opcode` is not one.
    pub(crate) fn packet_3d(&mut self, mem: &mut dyn BusMemory, opcode: u8, body: &[u32]) -> bool {
        let get = |i: usize| body.get(i).copied().unwrap_or(0);
        match opcode {
            op::DRAW_IMMD | op::DRAW_VBUF | op::DRAW_INDX => self.draw_packet(mem, get(1), body.get(2..).unwrap_or(&[])),
            op::DRAW_IMMD_2 | op::DRAW_VBUF_2 | op::DRAW_INDX_2 | op::DRAW_128 => {
                self.draw_packet(mem, get(0), body.get(1..).unwrap_or(&[]))
            }
            op::INDX_BUFFER => {
                let skip = bits(get(0), 16, 3) as usize;
                let size = (get(2) & 0x7f_ffff) as usize;
                let dest = (get(0) & 0x1fff) << 2;
                // Catalyst starts 16-bit index lists at odd indices by
                // pointing two bytes into a dword, and the MIN/MAX clamp it
                // programs only fits the indices read from there.
                let pending = self.r3d.pending_indx.take();
                let addr = if pending.is_some() { get(1) } else { get(1) & !3 };
                let words = self.mc_read_dwords(mem, addr, size.min(1 << 22));
                let words = words.get(skip..).unwrap_or(&[]);
                if let Some(vf) = pending {
                    let idx = self.indices(vf, words);
                    self.draw_3d(mem, vf, Verts::Indices(idx));
                } else {
                    let one = get(0) & (1 << 31) != 0;
                    for (k, &v) in words.iter().enumerate() {
                        let reg = if one { dest } else { dest + 4 * k as u32 };
                        self.cp_reg_write(mem, reg, v);
                    }
                }
            }
            op::LOAD_VBPNTR => {
                let n = (get(0) & 31) as usize;
                self.regs[VAP_VTX_NUM_ARRAYS as usize / 4] = get(0);
                let mut i = 1;
                for pair in 0..n.div_ceil(2) {
                    let base = (VAP_VTX_AOS_ATTR01 + 12 * pair as u32) as usize / 4;
                    self.regs[base] = get(i);
                    self.regs[base + 1] = get(i + 1);
                    let second = 2 * pair + 1 < n;
                    if second {
                        self.regs[base + 2] = get(i + 2);
                    }
                    i += if second { 3 } else { 2 };
                }
            }
            op::CLEAR_ZMASK => self.r3d.zmask.clear(get(0), get(1), get(2)),
            op::CLEAR_HIZ => {}
            _ => return false,
        }
        true
    }

    /// Runs a draw packet's VAP_VF_CNTL and the dwords after it, which hold
    /// indices, nothing or vertices as its PRIM_WALK field says.
    fn draw_packet(&mut self, mem: &mut dyn BusMemory, vf: u32, words: &[u32]) {
        match bits(vf, 4, 2) {
            1 if words.is_empty() => self.r3d.pending_indx = Some(vf),
            1 => {
                let idx = self.indices(vf, words);
                self.draw_3d(mem, vf, Verts::Indices(idx));
            }
            2 => self.draw_3d(mem, vf, Verts::List),
            _ => self.draw_3d(mem, vf, Verts::Immediate(words)),
        }
    }

    fn indices(&self, vf: u32, words: &[u32]) -> Vec<u32> {
        let count = (vf >> 16) as usize;
        let wide = vf & (1 << 11) != 0;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let v = if wide {
                words.get(i).copied()
            } else {
                words.get(i / 2).map(|w| if i % 2 == 0 { w & 0xffff } else { w >> 16 })
            };
            match v {
                Some(v) => out.push(v),
                None => break,
            }
        }
        out
    }

    /// The dwords of each vertex, as the PSC sees them: in immediate mode
    /// straight from the packet, otherwise each array of structures in turn.
    fn vertex_dwords(&mut self, mem: &mut dyn BusMemory, vf: u32, verts: &Verts<'_>) -> Vec<Vec<u32>> {
        let count = ((vf >> 16) as usize).min(MAX_VERTICES);
        match verts {
            Verts::Immediate(data) => {
                let size = (self.reg(VAP_VTX_SIZE) & 0x7f).max(1) as usize;
                data.chunks(size).take(count).map(|c| c.to_vec()).collect()
            }
            Verts::List | Verts::Indices(_) => {
                let index: Vec<u32> = match verts {
                    Verts::Indices(i) => {
                        let (lo, hi) = (self.reg(VAP_VF_MIN_VTX_INDX) & 0xff_ffff, self.reg(VAP_VF_MAX_VTX_INDX) & 0xff_ffff);
                        let hi = if hi == 0 { 0xff_ffff } else { hi };
                        i.iter().map(|&x| x.clamp(lo, hi.max(lo))).collect()
                    }
                    _ => (0..count as u32).collect(),
                };
                let arrays = (self.reg(VAP_VTX_NUM_ARRAYS) & 31) as usize;
                let max = index.iter().copied().max().unwrap_or(0) as usize;
                let mut fetched = Vec::with_capacity(arrays);
                for a in 0..arrays.min(16) {
                    let pair = a / 2;
                    let attr_reg = VAP_VTX_AOS_ATTR01 + 12 * pair as u32;
                    let attr = self.reg(attr_reg) >> (16 * (a % 2) as u32);
                    let addr = self.reg(attr_reg + 4 + 4 * (a % 2) as u32) & !3;
                    let (n, stride) = (bits(attr, 0, 7) as usize, bits(attr, 8, 7) as usize);
                    let total = (max * stride + n).min(1 << 22);
                    fetched.push((self.mc_read_dwords(mem, addr, total), n, stride));
                }
                index
                    .iter()
                    .map(|&i| {
                        let mut v = Vec::new();
                        for (data, n, stride) in &fetched {
                            let at = i as usize * stride;
                            v.extend((0..*n).map(|k| data.get(at + k).copied().unwrap_or(0)));
                        }
                        v
                    })
                    .collect()
            }
        }
    }

    /// The PSC's registers, which split a vertex's dwords into input vectors.
    fn psc(&self) -> Psc {
        Psc {
            cntl: std::array::from_fn(|i| self.reg(VAP_PROG_STREAM_CNTL_0 + 4 * i as u32)),
            ext: std::array::from_fn(|i| self.reg(VAP_PROG_STREAM_CNTL_EXT_0 + 4 * i as u32)),
        }
    }

    /// The VAP's output vector for each vertex attribute the GA receives.
    fn setup_vertex(&self, out: &[V4; pvs::OUTPUTS]) -> Vertex {
        let fmt0 = self.reg(VAP_OUT_VTX_FMT_0);
        let fmt1 = self.reg(VAP_OUT_VTX_FMT_1);
        let mut slot = 0;
        let mut next = || {
            let v = out.get(slot).copied().unwrap_or([0.0; 4]);
            slot += 1;
            v
        };
        let mut v = Vertex { pos: next(), ..Default::default() };
        if fmt0 & (1 << 16) != 0 {
            v.psize = Some(next()[0]);
        }
        for c in 0..4 {
            if fmt0 & (2 << c) != 0 {
                v.colours[c] = next();
            }
        }
        for t in 0..8 {
            if bits(fmt1, 3 * t, 3) != 0 {
                v.tex[t as usize] = next();
            }
        }
        v
    }

    fn viewport(&self) -> Viewport {
        let f = |r: u32| f32::from_bits(self.reg(r));
        let ga = self.reg(GA_OFFSET);
        let sub = self.subpixels();
        Viewport {
            vte: self.reg(VAP_VTE_CNTL),
            scale: [f(VAP_VPORT_XSCALE), f(VAP_VPORT_YSCALE), f(VAP_VPORT_ZSCALE)],
            offset: [f(VAP_VPORT_XOFFSET), f(VAP_VPORT_YOFFSET), f(VAP_VPORT_ZOFFSET)],
            ga: (((ga & 0xffff) as i16) as f32 / sub, ((ga >> 16) as i16) as f32 / sub),
            fog: (self.reg(GB_SELECT) & 7, f(GA_FOG_SCALE), f(GA_FOG_OFFSET)),
        }
    }

    fn subpixels(&self) -> f32 {
        if self.reg(GB_TILE_CONFIG) & (1 << 16) != 0 { 16.0 } else { 12.0 }
    }

    fn texture(&mut self, mem: &mut dyn BusMemory, unit: u32) -> Option<Texture> {
        if self.reg(TX_ENABLE) & (1 << unit) == 0 {
            return None;
        }
        let r = |base: u32| self.reg(base + 4 * unit);
        let (f0, f1, f2, off) = (r(TX_FORMAT0_0), r(TX_FORMAT1_0), r(TX_FORMAT2_0), r(TX_OFFSET_0));
        let (filter, border) = (r(TX_FILTER0_0), r(TX_BORDER_COLOR_0));
        let width = bits(f0, 0, 11) + 1;
        let height = bits(f0, 11, 11) + 1;
        let levels = bits(f0, 26, 4);
        let pitch = (f0 & (1 << 31) != 0).then(|| bits(f2, 0, 14) + 1);
        let format = f1 & 31;
        let base = off & !31;
        let kind = bits(f1, 25, 2);
        let depth = 1 << bits(f0, 22, 4);
        let mut t = Texture::new(TexMem::Vram(0), width, height, format, levels, pitch, kind, depth, off & 0x1c);
        t.mem = match self.surface_vram(base) {
            Some(o) => TexMem::Vram(o),
            None => {
                let mut buf = vec![0; t.span().min(64 << 20)];
                self.mc_read(mem, base, &mut buf);
                TexMem::Copy(buf)
            }
        };
        t.sel = [bits(f1, 9, 3), bits(f1, 12, 3), bits(f1, 15, 3), bits(f1, 18, 3)];
        t.signed = bits(f1, 5, 4);
        t.filter = filter;
        t.border = border;
        Some(t)
    }

    /// The enabled texture maps by the logical id in their TX_FILTER0, which
    /// is what the fragment program's TEX_ID names. Mesa gives every map its
    /// own unit number as id; XP's ATI driver rotates the physical unit and
    /// keeps id 0.
    fn textures(&mut self, mem: &mut dyn BusMemory) -> [Option<Texture>; 16] {
        let mut out: [Option<Texture>; 16] = Default::default();
        for unit in (0..16).rev() {
            if let Some(t) = self.texture(mem, unit) {
                out[(self.reg(TX_FILTER0_0 + 4 * unit) >> 28) as usize] = Some(t);
            }
        }
        out
    }

    fn fragment_program(&self) -> us::Program {
        let config = self.reg(US_CONFIG);
        let levels = (config & 7) as usize + 1;
        let offset = self.reg(US_CODE_OFFSET);
        let (alu_off, tex_off) = (bits(offset, 0, 6) as usize, bits(offset, 13, 5) as usize);
        let mut nodes = Vec::new();
        for (k, n) in (4 - levels.min(4)..4).enumerate() {
            let a = self.reg(US_CODE_ADDR_0 + 4 * n as u32);
            let alu = (alu_off + bits(a, 0, 6) as usize, bits(a, 6, 6) as usize + 1);
            let has_tex = k > 0 || config & (1 << 3) != 0;
            let tex = has_tex.then(|| (tex_off + bits(a, 12, 5) as usize, bits(a, 17, 5) as usize + 1));
            nodes.push(us::Node { alu, tex });
        }
        us::Program {
            nodes,
            tex: (0..32).map(|i| self.reg(US_TEX_INST_0 + 4 * i)).collect(),
            alu: (0..64)
                .map(|i| {
                    [
                        self.reg(US_ALU_RGB_ADDR_0 + 4 * i),
                        self.reg(US_ALU_RGB_INST_0 + 4 * i),
                        self.reg(US_ALU_ALPHA_ADDR_0 + 4 * i),
                        self.reg(US_ALU_ALPHA_INST_0 + 4 * i),
                    ]
                })
                .collect(),
            consts: (0..32)
                .map(|i| [0, 1, 2, 3].map(|c| us::s16e7(self.reg(US_ALU_CONST_R_0 + 16 * i + 4 * c))))
                .collect(),
        }
    }

    fn rect(&self, a: u32, b: u32, offset: i32) -> [i32; 4] {
        let (va, vb) = (self.reg(a), self.reg(b));
        [
            bits(va, 0, 13) as i32 - offset,
            bits(va, 13, 13) as i32 - offset,
            bits(vb, 0, 13) as i32 - offset,
            bits(vb, 13, 13) as i32 - offset,
        ]
    }

    fn raster_state(&mut self, mem: &mut dyn BusMemory) -> Raster {
        let textures = self.textures(mem);
        let offset = if self.reg(GB_TILE_CONFIG) & (1 << 16) != 0 { 1088 } else { 1440 };
        let f = |r: u32| f32::from_bits(self.reg(r));
        let multi = bits(self.reg(RB3D_CCTL), 5, 2) as usize + 1;
        let colour = (0..multi as u32)
            .filter_map(|i| {
                let p = self.reg(RB3D_COLORPITCH0 + 4 * i);
                let pitch = (p & 0x3ffe) as usize;
                let format = bits(p, 21, 4);
                let offset = self.surface_vram(self.reg(RB3D_COLOROFFSET0 + 4 * i) & !31)?;
                (pitch > 0).then_some(ColourBuffer { offset, pitch, format })
            })
            .collect();
        let zb_cntl = self.reg(ZB_CNTL);
        let depth = (zb_cntl & 3 != 0)
            .then(|| {
                let offset = self.surface_vram(self.reg(ZB_DEPTHOFFSET) & !31)?;
                let pitch = (self.reg(ZB_DEPTHPITCH) & 0x3ffc) as usize;
                Some(DepthBuffer { offset, pitch, z16: self.reg(ZB_FORMAT) & 15 < 2 })
            })
            .flatten();
        let fmt1 = self.reg(VAP_OUT_VTX_FMT_1);
        let rs_count = (self.reg(RS_INST_COUNT) & 15) as u32 + 1;
        let solid = {
            let (rg, ba) = (self.reg(GA_SOLID_RG), self.reg(GA_SOLID_BA));
            let c = |v: u32| (v as u16 as i16) as f32 / 4096.0;
            [c(rg >> 16), c(rg), c(ba >> 16), c(ba)]
        };
        Raster {
            colour,
            depth,
            zb_cntl,
            cb_clear: (self.reg(ZB_BW_CNTL) & ZB_CB_CLEAR != 0).then(|| self.reg(ZB_DEPTHCLEARVALUE)),
            zs_cntl: self.reg(ZB_ZSTENCILCNTL),
            stencil_ref: self.reg(ZB_STENCILREFMASK),
            depth_scale: f(SU_DEPTH_SCALE),
            depth_offset: f(SU_DEPTH_OFFSET),
            poly_offset: (
                self.reg(SU_POLY_OFFSET_ENABLE),
                [f(SU_POLY_OFFSET_FRONT_SCALE), f(SU_POLY_OFFSET_FRONT_OFFSET), f(SU_POLY_OFFSET_BACK_SCALE), f(SU_POLY_OFFSET_BACK_OFFSET)],
            ),
            scissor: self.rect(SC_SCISSOR0, SC_SCISSOR1, offset),
            clips: [0, 1, 2, 3].map(|k| self.rect(SC_CLIP_0_A + 8 * k, SC_CLIP_0_A + 8 * k + 4, offset)),
            clip_rule: self.reg(SC_CLIP_RULE) & 0xffff,
            blend: self.reg(RB3D_BLENDCNTL),
            ablend: self.reg(RB3D_ABLENDCNTL),
            channel_mask: self.reg(RB3D_COLOR_CHANNEL_MASK),
            constant: self.reg(RB3D_CONSTANT_COLOR),
            rop: self.reg(RB3D_ROPCNTL),
            alpha_func: self.reg(FG_ALPHA_FUNC),
            fog: self.reg(FG_FOG_BLEND),
            fog_colour: [0, 1, 2].map(|c| bits(self.reg(FG_FOG_COLOR_R + 4 * c), 0, 10) as f32 / 1023.0),
            fog_factor: bits(self.reg(FG_FOG_FACTOR), 0, 10) as f32 / 1023.0,
            cull: self.reg(SU_CULL_MODE),
            colour_control: self.reg(GA_COLOR_CONTROL),
            clamp_colours: self.reg(GA_ROUND_MODE) & (1 << 4) == 0,
            solid,
            out_fmt: self.reg(US_OUT_FMT_0),
            rs_inst: (0..rs_count).map(|i| self.reg(RS_INST_0 + 4 * i)).collect(),
            rs_ip: std::array::from_fn(|i| self.reg(RS_IP_0 + 4 * i as u32)),
            tex_counts: std::array::from_fn(|t| bits(fmt1, 3 * t as u32, 3)),
            perspective: self.reg(GB_SELECT) & (1 << 4) == 0,
            us: self.fragment_program(),
            textures,
            zpass: self.reg(ZB_ZPASS_DATA),
            pixels: 0,
            capture: None,
        }
    }

    /// Hands a draw of triangles to the GPU's vertex stage; false if the CPU
    /// must run its vertices: no Metal, or points and lines.
    fn gpu_vertices(&mut self, mem: &mut dyn BusMemory, vf: u32, verts: &Verts<'_>) -> bool {
        if !self.metal.as_ref().is_some_and(|m| m.gpu_vertices()) || self.trace.is_some() {
            return false;
        }
        let prim = vf & 15;
        let colour_control = self.reg(GA_COLOR_CONTROL);
        if !matches!(prim, 4 | 5 | 6 | 13 | 14 | 15) {
            return false;
        }
        let count = ((vf >> 16) as usize).min(MAX_VERTICES);
        let mut job = vsgen::VertexJob {
            indices: Vec::new(),
            immediate: Vec::new(),
            arrays: Vec::new(),
            consts: Vec::new(),
            uniforms: vsgen::VsUniforms::default(),
            cull: self.reg(SU_CULL_MODE),
            reads: Vec::new(),
        };
        let (ids, fetch): (Vec<u32>, vsgen::Fetch) = match verts {
            Verts::Immediate(data) => {
                let size = (self.reg(VAP_VTX_SIZE) & 0x7f).max(1) as usize;
                let n = data.len().div_ceil(size).min(count);
                job.immediate = data[..data.len().min(n * size)].to_vec();
                job.immediate.resize(n * size, 0);
                ((0..n as u32).collect(), vsgen::Fetch::Immediate(size as u32))
            }
            Verts::List | Verts::Indices(_) => {
                let ids: Vec<u32> = match verts {
                    Verts::Indices(i) => {
                        let (lo, hi) = (self.reg(VAP_VF_MIN_VTX_INDX) & 0xff_ffff, self.reg(VAP_VF_MAX_VTX_INDX) & 0xff_ffff);
                        let hi = if hi == 0 { 0xff_ffff } else { hi };
                        i.iter().map(|&x| x.clamp(lo, hi.max(lo))).collect()
                    }
                    _ => (0..count as u32).collect(),
                };
                let (lo, hi) = (ids.iter().copied().min().unwrap_or(0) as usize, ids.iter().copied().max().unwrap_or(0) as usize);
                let mut sizes = Vec::new();
                for a in 0..((self.reg(VAP_VTX_NUM_ARRAYS) & 31) as usize).min(16) {
                    let attr_reg = VAP_VTX_AOS_ATTR01 + 12 * (a / 2) as u32;
                    let attr = self.reg(attr_reg) >> (16 * (a % 2) as u32);
                    let addr = self.reg(attr_reg + 4 + 4 * (a % 2) as u32) & !3;
                    let (n, stride) = (bits(attr, 0, 7) as usize, bits(attr, 8, 7) as usize);
                    let end = hi * stride + n;
                    if end > 1 << 22 {
                        return false;
                    }
                    let last = addr.wrapping_add(4 * end.max(1) as u32 - 4);
                    let src = match (self.mc_place(mem, addr), self.mc_place(mem, last)) {
                        (Place::Vram(off), Place::Vram(o)) if o == off + 4 * end.max(1) - 4 => {
                            job.reads.push((off, off + 4 * end));
                            vsgen::ArraySrc::Vram(off / 4)
                        }
                        _ => {
                            let first = lo * stride;
                            let data = self.mc_read_dwords(mem, addr.wrapping_add(4 * first as u32), end.saturating_sub(first));
                            vsgen::ArraySrc::Copy { data, first: lo as u32 }
                        }
                    };
                    job.arrays.push((src, stride as u32));
                    sizes.push(n as u32);
                }
                (ids, vsgen::Fetch::Arrays(sizes))
            }
        };
        let provoking = ((colour_control >> 16) & 3).min(2) as usize;
        job.indices = vsgen::triangles(prim, ids.len(), provoking).into_iter().map(|k| ids[k as usize]).collect();
        let bypass = self.reg(VAP_CNTL_STATUS) & PVS_BYPASS != 0;
        let code = self.reg(VAP_PVS_CODE_CNTL_0);
        let const_base = bits(self.reg(VAP_PVS_CONST_CNTL), 0, 8) as usize;
        let start = ((pvs::CONST_START + const_base) * 4).min(self.r3d.vecmem.len());
        let program = if bypass { Vec::new() } else { vsgen::code(&self.r3d.vecmem, bits(code, 0, 10) as usize, bits(code, 20, 10) as usize) };
        let used = vsgen::consts_used(&program).saturating_mul(4);
        job.consts = self.r3d.vecmem[start..(start.saturating_add(used)).min(self.r3d.vecmem.len())].to_vec();
        job.uniforms.nconsts = (job.consts.len() / 4) as u32;
        let vp = self.viewport();
        job.uniforms.scale = [vp.scale[0], vp.scale[1], vp.scale[2], 0.0];
        job.uniforms.offset = [vp.offset[0], vp.offset[1], vp.offset[2], 0.0];
        job.uniforms.ga_fog = [vp.ga.0, vp.ga.1, vp.fog.1, vp.fog.2];
        let psc = self.psc();
        let mut r = self.raster_state(mem);
        job.uniforms.solid = r.solid;
        let desc = vsgen::VsDesc {
            code: program,
            psc: std::array::from_fn(|i| if i < 8 { psc.cntl[i] } else { psc.ext[i - 8] }),
            fetch,
            fmt: [self.reg(VAP_OUT_VTX_FMT_0), self.reg(VAP_OUT_VTX_FMT_1)],
            vte: vp.vte,
            clip: self.reg(VAP_CLIP_CNTL) & CLIP_DISABLE == 0,
            dx: self.reg(VAP_CNTL) & DX_CLIP_SPACE_DEF != 0,
            fog_select: vp.fog.0,
            colour_control,
            clamp_colours: r.clamp_colours,
            rs_inst: r.rs_inst.clone(),
            rs_ip: r.rs_ip,
        };
        let slots = Captured::new(&r.rs_inst).slots;
        r.capture = None;
        let Some(m) = self.metal.as_mut() else { return false };
        if !m.draw_vertices(&r, &slots, &desc, &job) {
            return false;
        }
        self.vga.touch();
        true
    }

    fn draw_3d(&mut self, mem: &mut dyn BusMemory, vf: u32, verts: Verts<'_>) {
        self.r3d.draws += 1;
        self.resolve_zmask();
        if self.gpu_vertices(mem, vf, &verts) {
            return;
        }
        let prim = vf & 15;
        let (verts, remap) = match verts {
            Verts::Indices(idx) => {
                let (unique, remap) = unique_indices(&idx);
                (Verts::Indices(unique), Some(remap))
            }
            v => (v, None),
        };
        let dwords = self.vertex_dwords(mem, vf, &verts);
        if self.trace.is_some() {
            let first: Vec<String> = dwords.iter().take(4).map(|d| format!("{:?}", d.iter().map(|w| f32::from_bits(*w)).collect::<Vec<_>>())).collect();
            self.note(&format!("3d: draw vf {vf:#x}, {} vertices {}", dwords.len(), first.join(" ")));
        }
        let bypass = self.reg(VAP_CNTL_STATUS) & PVS_BYPASS != 0;
        let code = self.reg(VAP_PVS_CODE_CNTL_0);
        let program = pvs::Program {
            mem: &self.r3d.vecmem,
            first: bits(code, 0, 10) as usize,
            last: bits(code, 20, 10) as usize,
            const_base: bits(self.reg(VAP_PVS_CONST_CNTL), 0, 8) as usize,
        };
        let psc = self.psc();
        let program = program.decode();
        let shade = |d: &Vec<u32>| {
            let inputs = psc.inputs(d);
            if bypass { std::array::from_fn(|i| inputs[i]) } else { program.run(&inputs) }
        };
        let outputs: Vec<[V4; pvs::OUTPUTS]> = if dwords.len() < PARALLEL_VERTICES {
            dwords.iter().map(shade).collect()
        } else {
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
            let chunk = dwords.len().div_ceil(threads);
            std::thread::scope(|s| {
                let parts: Vec<_> = dwords.chunks(chunk).map(|c| s.spawn(move || c.iter().map(shade).collect::<Vec<_>>())).collect();
                parts.into_iter().flat_map(|h| h.join().unwrap()).collect()
            })
        };
        let shaded: Vec<Vertex> = outputs.iter().map(|o| self.setup_vertex(o)).collect();
        let vertices: Vec<Vertex> = match &remap {
            Some(r) => r.iter().map(|&k| shaded[k as usize]).collect(),
            None => shaded,
        };
        let mut r = self.raster_state(mem);
        let pipeline = self.metal.as_mut().and_then(|m| {
            let cap = Captured::new(&r.rs_inst);
            let p = m.prepare(&r, &cap)?;
            r.capture = Some(cap);
            Some(p)
        });
        if pipeline.is_none() {
            self.gpu_sync();
        }
        let counts = r.tex_counts;
        let gb = self.reg(GB_ENABLE);
        let vp = self.viewport();
        let behind = |v: &Vertex| vp.clips() && v.pos[3] < NEAR_W;
        let planes = clip_planes(
            self.reg(VAP_CLIP_CNTL) & CLIP_DISABLE == 0,
            self.reg(VAP_CNTL) & DX_CLIP_SPACE_DEF != 0,
            f32::from_bits(self.reg(VAP_GB_HORZ_CLIP_ADJ)),
            f32::from_bits(self.reg(VAP_GB_VERT_CLIP_ADJ)),
        );
        let outside = |v: &Vertex| planes.iter().any(|p| distance(p, v) < 0.0);
        let vram = self.vga.vram_mut();
        let n = vertices.len();
        let tri = |r: &mut Raster, vram: &mut [u8], a: usize, b: usize, c: usize| {
            let t = [vertices[a], vertices[b], vertices[c]];
            if vp.clips() && t.iter().any(outside) {
                let poly = clip(&t, &planes);
                for i in 2..poly.len() {
                    r.triangle(vram, [poly[0], poly[i - 1], poly[i]].map(|v| vp.apply(v)), &counts, true);
                }
            } else {
                r.triangle(vram, t.map(|v| vp.apply(v)), &counts, true);
            }
        };
        match prim {
            1 => {
                let size = self.regs[GA_POINT_SIZE as usize / 4];
                let sub = if self.regs[GB_TILE_CONFIG as usize / 4] & (1 << 16) != 0 { 16.0 } else { 12.0 };
                let half = ((size >> 16) as f32 / sub, (size & 0xffff) as f32 / sub);
                let st = [GA_POINT_S0, GA_POINT_T0, GA_POINT_S1, GA_POINT_T1].map(|reg| f32::from_bits(self.regs[reg as usize / 4]));
                let mut stuff = Vec::new();
                let mut pcounts = counts;
                if gb & 1 != 0 {
                    for t in 0..8 {
                        if bits(gb, 16 + 2 * t, 2) != 0 {
                            stuff.push((t as usize, st));
                            pcounts[t as usize] = 2;
                        }
                    }
                }
                for v in vertices.iter().filter(|v| !behind(v)) {
                    let v = vp.apply(*v);
                    let half = match v.psize {
                        Some(s) if s > 0.0 => (s / 2.0, s / 2.0),
                        _ => half,
                    };
                    r.point(vram, v, half, &stuff, &pcounts);
                }
            }
            2 | 3 | 12 => {
                let width = (self.regs[GA_LINE_CNTL as usize / 4] & 0xffff) as f32 / 16.0;
                let half = width.max(0.5);
                let pairs: Vec<(usize, usize)> = match prim {
                    2 => (0..n / 2).map(|i| (2 * i, 2 * i + 1)).collect(),
                    3 => (1..n).map(|i| (i - 1, i)).collect(),
                    _ => (0..n).map(|i| (i, (i + 1) % n)).collect(),
                };
                for (a, b) in pairs {
                    let (a, b) = (vertices[a], vertices[b]);
                    if !behind(&a) && !behind(&b) {
                        r.line(vram, vp.apply(a), vp.apply(b), half, &counts);
                    }
                }
            }
            4 => (0..n / 3).for_each(|i| tri(&mut r, vram, 3 * i, 3 * i + 1, 3 * i + 2)),
            5 | 15 => (2..n).for_each(|i| tri(&mut r, vram, 0, i - 1, i)),
            6 => (2..n).for_each(|i| {
                if i % 2 == 0 {
                    tri(&mut r, vram, i - 2, i - 1, i)
                } else {
                    tri(&mut r, vram, i - 1, i - 2, i)
                }
            }),
            13 => (0..n / 4).for_each(|i| {
                tri(&mut r, vram, 4 * i, 4 * i + 1, 4 * i + 2);
                tri(&mut r, vram, 4 * i, 4 * i + 2, 4 * i + 3);
            }),
            14 => (1..n / 2).for_each(|i| {
                let (a, b, c, d) = (2 * i - 2, 2 * i - 1, 2 * i + 1, 2 * i);
                tri(&mut r, vram, a, b, c);
                tri(&mut r, vram, a, c, d);
            }),
            _ => {}
        }
        if let (Some(cap), Some(p), Some(m)) = (r.capture.take(), pipeline, self.metal.as_mut()) {
            m.draw(&r, &cap, &p);
            if cap.vertices > 0 {
                self.vga.touch();
            }
            return;
        }
        self.regs[ZB_ZPASS_DATA as usize / 4] = r.zpass;
        if self.trace.is_some() {
            self.note(&format!("3d: {} pixels", r.pixels));
        }
        if r.pixels > 0 {
            self.vga.touch();
        }
    }
}

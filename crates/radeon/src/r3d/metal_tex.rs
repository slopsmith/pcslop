//! Hardware texture sampling for the Metal backend. Each mipmap level (and
//! cube face) of a guest texture becomes a 2D `MTLTexture` that the fragment
//! shader samples with a Metal sampler, after picking the level as tex.rs
//! does. Uncompressed levels are linear textures over the VRAM buffer, so
//! they always show what VRAM holds. Metal has no linear BC layout, so DXT
//! levels are copies, compared with a shadow of their bytes once per epoch.
//!
//! The shader samples in software what Metal cannot match exactly: textures
//! in system memory, volume textures, partly signed formats, formats Metal
//! lacks, mirror-once-to-border wrapping, and other border colours than
//! Metal's three presets.

use super::tex::{TexMem, Texture};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;
use std::collections::HashMap;

pub(crate) type MetalTexture = Retained<ProtocolObject<dyn MTLTexture>>;
pub(crate) type Sampler = Retained<ProtocolObject<dyn MTLSamplerState>>;

/// Entries and bytes of DXT copies kept at most; past either the cache
/// starts over. Command buffers keep the textures they use alive.
const MAX_ENTRIES: usize = 4096;
const MAX_COPY_BYTES: usize = 256 << 20;
/// Apple GPUs' `minimumLinearTextureAlignmentForPixelFormat`.
const LINEAR_ALIGN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NativeFormat {
    pub pixel: MTLPixelFormat,
    /// The Metal channel holding each of the texel's components C0..C3.
    pub perm: [u32; 4],
    pub compressed: bool,
}

/// The Metal pixel format that samples `format` as tex.rs decodes it. The
/// packed 16-bit formats' channel orders were measured on an M4 Pro.
pub(crate) fn native_format(format: u32, signed: u32) -> Option<NativeFormat> {
    use MTLPixelFormat as P;
    const ID: [u32; 4] = [0, 1, 2, 3];
    const BGRA: [u32; 4] = [2, 1, 0, 3];
    let (unorm, snorm, comps, perm, compressed) = match format {
        0 => (P::R8Unorm, Some(P::R8Snorm), 1, ID, false),
        1 => (P::R16Unorm, Some(P::R16Snorm), 1, ID, false),
        3 => (P::RG8Unorm, Some(P::RG8Snorm), 2, ID, false),
        4 => (P::RG16Unorm, Some(P::RG16Snorm), 2, ID, false),
        6 => (P::B5G6R5Unorm, None, 3, BGRA, false),
        10 => (P::ABGR4Unorm, None, 4, [3, 2, 1, 0], false),
        11 => (P::BGR5A1Unorm, None, 4, BGRA, false),
        12 => (P::RGBA8Unorm, Some(P::RGBA8Snorm), 4, ID, false),
        13 => (P::RGB10A2Unorm, None, 4, ID, false),
        14 => (P::RGBA16Unorm, Some(P::RGBA16Snorm), 4, ID, false),
        0xf => (P::BC1_RGBA, None, 0, BGRA, true),
        0x10 => (P::BC2_RGBA, None, 0, BGRA, true),
        0x11 => (P::BC3_RGBA, None, 0, BGRA, true),
        0x18 => (P::R16Float, None, 0, ID, false),
        0x19 => (P::RG16Float, None, 0, ID, false),
        0x1a => (P::RGBA16Float, None, 0, ID, false),
        0x1b => (P::R32Float, None, 0, ID, false),
        0x1c => (P::RG32Float, None, 0, ID, false),
        0x1d => (P::RGBA32Float, None, 0, ID, false),
        _ => return None,
    };
    let all = (1 << comps) - 1;
    let pixel = match signed & all {
        0 => unorm,
        m if m == all => snorm?,
        _ => return None,
    };
    Some(NativeFormat { pixel, perm, compressed })
}

/// For each output channel (R, G, B, A) after TX_FORMAT1's swizzle, three
/// bits naming the Metal channel it reads (0 to 3), or 4 for zero and 5 for
/// one.
pub(crate) fn channel_map(sel: [u32; 4], perm: [u32; 4]) -> u32 {
    let pick = |s: u32| match s {
        0..=3 => perm[s as usize],
        5 => 5,
        _ => 4,
    };
    [sel[1], sel[2], sel[3], sel[0]].iter().enumerate().fold(0, |m, (k, &s)| m | pick(s) << (3 * k))
}

fn address_mode(mode: u32) -> Option<MTLSamplerAddressMode> {
    use MTLSamplerAddressMode as A;
    Some(match mode {
        0 => A::Repeat,
        1 => A::MirrorRepeat,
        2 | 4 => A::ClampToEdge,
        3 | 5 => A::MirrorClampToEdge,
        6 => A::ClampToBorderColor,
        _ => return None,
    })
}

// Metal converts the preset to the texture's format, which garbles it for
// packed formats, so only four-channel formats qualify.
fn border_preset(format: u32, map: u32, border: u32) -> Option<u32> {
    if !matches!(format, 12 | 13 | 14 | 0x1a | 0x1d) {
        return None;
    }
    let want = [(border >> 16) & 0xff, (border >> 8) & 0xff, border & 0xff, border >> 24];
    let presets = [
        (MTLSamplerBorderColor::TransparentBlack, [0, 0, 0, 0]),
        (MTLSamplerBorderColor::OpaqueBlack, [0, 0, 0, 255]),
        (MTLSamplerBorderColor::OpaqueWhite, [255; 4]),
    ];
    presets.iter().find_map(|(p, c)| {
        let got = [0, 1, 2, 3].map(|k| match (map >> (3 * k)) & 7 {
            4 => 0,
            5 => 255,
            ch => c[ch as usize],
        });
        (got == want).then_some(p.0 as u32)
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SamplerKey {
    s: u32,
    t: u32,
    border: u32,
}

/// What a texture needs from Metal: its images and sampler state.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Plan {
    base: usize,
    pixel: usize,
    compressed: bool,
    pub layers: u32,
    /// Offset from `base`, pitch, width and height of each level.
    levels: Vec<(usize, usize, u32, u32)>,
    slices: Vec<usize>,
    span: usize,
    pub sampler: SamplerKey,
    pub map: u32,
}

/// The plan for sampling `t` with Metal, or None if the shader must sample
/// it in software.
pub(crate) fn plan(t: &Texture, vram_len: usize) -> Option<Plan> {
    let TexMem::Vram(base) = t.mem else { return None };
    if t.kind == 1 {
        return None;
    }
    let f = native_format(t.format, t.signed)?;
    let map = channel_map(t.sel, f.perm);
    let (s, tt) = (t.filter & 7, (t.filter >> 3) & 7);
    address_mode(s)?;
    address_mode(tt)?;
    let border = if s == 6 || tt == 6 { border_preset(t.format, map, t.border)? } else { 0 };
    let (bytes, bw) = super::tex::block(t.format);
    let layers = if t.kind == 2 { 6 } else { 1 };
    let mut levels = Vec::new();
    for (l, &(off, pitch)) in t.levels.iter().enumerate() {
        let (w, h) = ((t.width >> l).max(1), (t.height >> l).max(1));
        let row = w.div_ceil(bw) as usize * bytes;
        let rows = h.div_ceil(bw) as usize;
        if pitch < row || (!f.compressed && (pitch % LINEAR_ALIGN != 0 || (base + off) % LINEAR_ALIGN != 0)) {
            return None;
        }
        let end = base + off + (layers as usize - 1) * t.slices[l] + (rows - 1) * pitch + row;
        if end > vram_len {
            return None;
        }
        levels.push((off, pitch, w, h));
    }
    Some(Plan {
        base,
        pixel: f.pixel.0,
        compressed: f.compressed,
        layers,
        levels,
        slices: t.slices.clone(),
        span: t.span().min(vram_len - base),
        sampler: SamplerKey { s, t: tt, border },
        map,
    })
}

struct Entry {
    /// One image per level and face, faces first.
    images: Vec<MetalTexture>,
    /// The bytes a copy was made from.
    shadow: Option<Vec<u8>>,
    checked: u64,
}

pub(crate) struct TexCache {
    entries: HashMap<Plan, Entry>,
    samplers: HashMap<(SamplerKey, bool), Sampler>,
    copy_bytes: usize,
    epoch: u64,
    /// Textures sampled by Metal and DXT copies made, for `status`.
    pub native: u64,
    pub uploads: u64,
}

impl TexCache {
    pub fn new() -> Self {
        TexCache { entries: HashMap::new(), samplers: HashMap::new(), copy_bytes: 0, epoch: 1, native: 0, uploads: 0 }
    }

    /// Notes that VRAM may have changed outside the Metal pipeline, so
    /// copies are compared with VRAM again at their next use.
    pub fn invalidate(&mut self) {
        self.epoch += 1;
    }

    /// The sampler for `key`, filtering linearly or taking the nearest texel.
    pub fn sampler(&mut self, device: &ProtocolObject<dyn MTLDevice>, key: SamplerKey, linear: bool) -> Option<Sampler> {
        if let Some(s) = self.samplers.get(&(key, linear)) {
            return Some(s.clone());
        }
        let d = MTLSamplerDescriptor::new();
        let filter = if linear { MTLSamplerMinMagFilter::Linear } else { MTLSamplerMinMagFilter::Nearest };
        d.setMinFilter(filter);
        d.setMagFilter(filter);
        d.setMipFilter(MTLSamplerMipFilter::NotMipmapped);
        d.setSAddressMode(address_mode(key.s)?);
        d.setTAddressMode(address_mode(key.t)?);
        d.setBorderColor(MTLSamplerBorderColor(key.border as _));
        d.setSupportArgumentBuffers(true);
        let s = device.newSamplerStateWithDescriptor(&d)?;
        self.samplers.insert((key, linear), s.clone());
        Some(s)
    }

    /// The images of a planned texture, one per level and face.
    pub fn images(&mut self, device: &ProtocolObject<dyn MTLDevice>, vram_buf: &ProtocolObject<dyn MTLBuffer>, vram: &[u8], p: &Plan) -> Option<&[MetalTexture]> {
        let epoch = self.epoch;
        let stale = match self.entries.get(p) {
            Some(Entry { shadow: Some(s), checked, .. }) if *checked != epoch => s[..] != vram[p.base..p.base + p.span],
            Some(_) => false,
            None => true,
        };
        if stale {
            if let Some(old) = self.entries.remove(p) {
                self.copy_bytes -= old.shadow.map_or(0, |s| s.len());
            }
            if self.entries.len() >= MAX_ENTRIES || self.copy_bytes + p.span > MAX_COPY_BYTES {
                self.entries.clear();
                self.copy_bytes = 0;
            }
            let entry = make(device, vram_buf, vram, p)?;
            if let Some(s) = &entry.shadow {
                self.uploads += 1;
                self.copy_bytes += s.len();
            }
            self.entries.insert(p.clone(), entry);
        }
        let e = self.entries.get_mut(p)?;
        e.checked = epoch;
        self.native += 1;
        Some(&e.images)
    }
}

fn make(device: &ProtocolObject<dyn MTLDevice>, vram_buf: &ProtocolObject<dyn MTLBuffer>, vram: &[u8], p: &Plan) -> Option<Entry> {
    let pixel = MTLPixelFormat(p.pixel);
    let block = if pixel == MTLPixelFormat::BC1_RGBA { 8 } else { 16 };
    let mut images = Vec::new();
    for (l, &(off, pitch, w, h)) in p.levels.iter().enumerate() {
        for face in 0..p.layers as usize {
            let at = p.base + off + face * p.slices[l];
            let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(pixel, w as usize, h as usize, false) };
            d.setUsage(MTLTextureUsage::ShaderRead);
            d.setStorageMode(MTLStorageMode::Shared);
            let img = if p.compressed {
                let img = device.newTextureWithDescriptor(&d)?;
                let row = w.div_ceil(4) as usize * block;
                let mut packed: Vec<u8> = (0..h.div_ceil(4) as usize).flat_map(|y| &vram[at + y * pitch..at + y * pitch + row]).copied().collect();
                let region = MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w as usize, height: h as usize, depth: 1 } };
                unsafe { img.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, std::ptr::NonNull::new(packed.as_mut_ptr().cast())?, row) };
                img
            } else {
                d.setResourceOptions(MTLResourceOptions::StorageModeShared);
                vram_buf.newTextureWithDescriptor_offset_bytesPerRow(&d, at, pitch)?
            };
            images.push(img);
        }
    }
    let shadow = p.compressed.then(|| vram[p.base..p.base + p.span].to_vec());
    Some(Entry { images, shadow, checked: 0 })
}

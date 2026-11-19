//! Textures for the Metal backend. Each mipmap level and cube face or volume
//! slice of a guest texture becomes a 2D `MTLTexture`. Uncompressed levels
//! in VRAM whose layout Metal accepts are linear textures over the VRAM
//! buffer, so they always show what VRAM holds. Other textures in a format
//! Metal has are copies; the rest are decoded on the CPU with tex.rs into
//! RGBA8 or RGBA32Float images. Copies and decoded images keep the bytes
//! they were made from and are compared with them again once VRAM may have
//! changed, or at every use for textures in system memory.
//!
//! The fragment shader samples with a Metal sampler, or for wrap modes and
//! border colours Metal lacks and for volume textures reads the texels it
//! needs and filters them as tex.rs does.

use super::tex::{TexMem, Texture};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;
use std::collections::HashMap;

pub(crate) type MetalTexture = Retained<ProtocolObject<dyn MTLTexture>>;
pub(crate) type Sampler = Retained<ProtocolObject<dyn MTLSamplerState>>;

/// Entries and bytes of copies kept at most; past either the cache starts
/// over. Command buffers keep the textures they use alive.
const MAX_ENTRIES: usize = 4096;
const MAX_COPY_BYTES: usize = 256 << 20;
/// Apple GPUs' `minimumLinearTextureAlignmentForPixelFormat`.
const LINEAR_ALIGN: usize = 16;
/// Images one texture may have at most, so a draw's table stays small.
pub(crate) const MAX_IMAGES: usize = 12 * 64;

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
// packed formats, so only four-channel images qualify.
fn border_preset(four_channels: bool, map: u32, border: u32) -> Option<u32> {
    if !four_channels {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Origin {
    Vram(usize),
    /// System memory, by its address in the memory controller's space.
    Bus(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Image {
    /// Linear textures over the VRAM buffer.
    Alias,
    /// Copies in the native pixel format.
    Copy,
    /// Decoded on the CPU, into RGBA32Float if true, else RGBA8Unorm.
    Decoded(bool),
}

/// What identifies a texture's images.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ImageKey {
    pub origin: Origin,
    pub image: Image,
    pixel: usize,
    format: u32,
    signed: u32,
    kind: u32,
    depth: u32,
    width: u32,
    height: u32,
    /// Images per level: 6 cube faces, the slices of a volume's base level
    /// or 1.
    pub layers: u32,
    /// Offset from the origin and pitch of each level.
    levels: Vec<(usize, usize)>,
    slices: Vec<usize>,
    pub span: usize,
}

/// What a texture needs from Metal: its images and how to sample them.
pub(crate) struct Plan {
    pub key: ImageKey,
    pub sampler: SamplerKey,
    pub map: u32,
    /// The shader reads texels and filters them itself.
    pub manual: bool,
}

/// Formats whose components RGBA8Unorm holds exactly.
fn fits_rgba8(format: u32, signed: u32) -> bool {
    signed == 0 && matches!(format, 0 | 2 | 3 | 10 | 12)
}

/// How Metal samples `t`, using its native pixel formats only if `native`;
/// None if the texture has more images than a draw may bind.
pub(crate) fn plan(t: &Texture, vram_len: usize, native: bool) -> Option<Plan> {
    let layers = match t.kind {
        1 => t.depth.max(1),
        2 => 6,
        _ => 1,
    };
    if layers as usize * t.levels.len() > MAX_IMAGES {
        return None;
    }
    let (s, tt) = (t.filter & 7, (t.filter >> 3) & 7);
    let mut manual = t.kind == 1 || address_mode(s).is_none() || address_mode(tt).is_none();
    let mut fmt = native.then(|| native_format(t.format, t.signed)).flatten();
    let border_colour = s == 6 || tt == 6;
    let map_of = |f: Option<NativeFormat>| match f {
        Some(f) => (channel_map(t.sel, f.perm), matches!(t.format, 12 | 13 | 14 | 0x1a | 0x1d)),
        None => (channel_map(t.sel, [0, 1, 2, 3]), true),
    };
    let mut border = 0;
    if !manual && border_colour {
        let (map, four) = map_of(fmt);
        match border_preset(four, map, t.border) {
            Some(b) => border = b,
            None => manual = true,
        }
    }
    if manual && fmt.is_some_and(|f| f.compressed) {
        fmt = None;
    }
    let (map, _) = map_of(fmt);
    let origin = match &t.mem {
        TexMem::Vram(o) => Origin::Vram(*o),
        TexMem::Copy { addr, .. } => Origin::Bus(*addr),
    };
    let (image, pixel) = match fmt {
        Some(f) => {
            let alias = matches!(origin, Origin::Vram(base) if !f.compressed && aliasable(t, base, vram_len));
            (if alias { Image::Alias } else { Image::Copy }, f.pixel)
        }
        None if fits_rgba8(t.format, t.signed) => (Image::Decoded(false), MTLPixelFormat::RGBA8Unorm),
        None => (Image::Decoded(true), MTLPixelFormat::RGBA32Float),
    };
    let span = match origin {
        Origin::Vram(base) => t.span().min(vram_len.saturating_sub(base)),
        Origin::Bus(_) => t.span(),
    };
    Some(Plan {
        key: ImageKey {
            origin,
            image,
            pixel: pixel.0,
            format: t.format,
            signed: t.signed,
            kind: t.kind,
            depth: t.depth,
            width: t.width,
            height: t.height,
            layers,
            levels: t.levels.clone(),
            slices: t.slices.clone(),
            span,
        },
        sampler: SamplerKey { s, t: tt, border },
        map,
        manual,
    })
}

/// Whether every level of `t` at VRAM offset `base` can be a linear texture.
fn aliasable(t: &Texture, base: usize, vram_len: usize) -> bool {
    let (bytes, _) = super::tex::block(t.format);
    let layers = if t.kind == 2 { 6 } else { 1 };
    t.levels.iter().enumerate().all(|(l, &(off, pitch))| {
        let (w, h) = ((t.width >> l).max(1) as usize, (t.height >> l).max(1) as usize);
        let row = w * bytes;
        let end = base + off + (layers - 1) * t.slices[l] + (h - 1) * pitch + row;
        pitch >= row && pitch % LINEAR_ALIGN == 0 && (base + off) % LINEAR_ALIGN == 0 && end <= vram_len
    })
}

struct Entry {
    /// One image per level and layer, layers first.
    images: Vec<MetalTexture>,
    /// The bytes a copy or decoded image was made from.
    shadow: Option<Vec<u8>>,
    checked: u64,
}

pub(crate) struct TexCache {
    entries: HashMap<ImageKey, Entry>,
    samplers: HashMap<(SamplerKey, bool), Sampler>,
    copy_bytes: usize,
    epoch: u64,
    /// Texture binds Metal sampled from native formats and from decoded
    /// images, binds the shader filters itself, and copies and decodes
    /// made, for `status`.
    pub native: u64,
    pub decoded: u64,
    pub manual: u64,
    pub uploads: u64,
}

impl TexCache {
    pub fn new() -> Self {
        TexCache { entries: HashMap::new(), samplers: HashMap::new(), copy_bytes: 0, epoch: 1, native: 0, decoded: 0, manual: 0, uploads: 0 }
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
        d.setSAddressMode(address_mode(key.s).unwrap_or(MTLSamplerAddressMode::ClampToEdge));
        d.setTAddressMode(address_mode(key.t).unwrap_or(MTLSamplerAddressMode::ClampToEdge));
        d.setBorderColor(MTLSamplerBorderColor(key.border as _));
        d.setSupportArgumentBuffers(true);
        let s = device.newSamplerStateWithDescriptor(&d)?;
        self.samplers.insert((key, linear), s.clone());
        Some(s)
    }

    /// The images of texture `t` as planned, one per level and layer.
    pub fn images(&mut self, device: &ProtocolObject<dyn MTLDevice>, vram_buf: &ProtocolObject<dyn MTLBuffer>, vram: &[u8], t: &Texture, k: &ImageKey) -> Option<&[MetalTexture]> {
        let epoch = self.epoch;
        let source = source(vram, t, k);
        let stale = match self.entries.get(k) {
            Some(Entry { shadow: Some(s), checked, .. }) if *checked != epoch || matches!(k.origin, Origin::Bus(_)) => s[..] != *source,
            Some(_) => false,
            None => true,
        };
        if stale {
            if let Some(old) = self.entries.remove(k) {
                self.copy_bytes -= old.shadow.map_or(0, |s| s.len());
            }
            if self.entries.len() >= MAX_ENTRIES || self.copy_bytes + k.span > MAX_COPY_BYTES {
                self.entries.clear();
                self.copy_bytes = 0;
            }
            let entry = make(device, vram_buf, vram, t, k, source)?;
            if let Some(s) = &entry.shadow {
                self.uploads += 1;
                self.copy_bytes += s.len();
            }
            self.entries.insert(k.clone(), entry);
        }
        let e = self.entries.get_mut(k)?;
        e.checked = epoch;
        if matches!(k.image, Image::Decoded(_)) {
            self.decoded += 1;
        } else {
            self.native += 1;
        }
        Some(&e.images)
    }
}

/// The bytes texture `t`'s images are made from.
fn source<'a>(vram: &'a [u8], t: &'a Texture, k: &ImageKey) -> &'a [u8] {
    match (&t.mem, k.origin) {
        (TexMem::Copy { bytes, .. }, _) => bytes,
        (_, Origin::Vram(base)) => vram.get(base..base + k.span).unwrap_or(&[]),
        _ => &[],
    }
}

fn new_image(device: &ProtocolObject<dyn MTLDevice>, pixel: MTLPixelFormat, w: u32, h: u32) -> Option<MetalTexture> {
    let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(pixel, w as usize, h as usize, false) };
    d.setUsage(MTLTextureUsage::ShaderRead);
    d.setStorageMode(MTLStorageMode::Shared);
    device.newTextureWithDescriptor(&d)
}

fn upload(img: &ProtocolObject<dyn MTLTexture>, w: u32, h: u32, data: &mut [u8], row: usize) -> Option<()> {
    let region = MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w as usize, height: h as usize, depth: 1 } };
    unsafe { img.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, std::ptr::NonNull::new(data.as_mut_ptr().cast())?, row) };
    Some(())
}

fn make(device: &ProtocolObject<dyn MTLDevice>, vram_buf: &ProtocolObject<dyn MTLBuffer>, vram: &[u8], t: &Texture, k: &ImageKey, source: &[u8]) -> Option<Entry> {
    let pixel = MTLPixelFormat(k.pixel);
    let (bytes, bw) = super::tex::block(k.format);
    let mut images: Vec<MetalTexture> = Vec::new();
    for (l, &(off, pitch)) in k.levels.iter().enumerate() {
        let (w, h) = ((k.width >> l).max(1), (k.height >> l).max(1));
        let count = if k.kind == 1 { (k.depth >> l).max(1) } else { k.layers };
        for layer in 0..k.layers {
            if layer >= count {
                images.push(images.last()?.clone());
                continue;
            }
            let at = off + layer as usize * k.slices[l];
            let img = match (k.image, k.origin) {
                (Image::Alias, Origin::Vram(base)) => {
                    let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(pixel, w as usize, h as usize, false) };
                    d.setUsage(MTLTextureUsage::ShaderRead);
                    d.setResourceOptions(MTLResourceOptions::StorageModeShared);
                    vram_buf.newTextureWithDescriptor_offset_bytesPerRow(&d, base + at, pitch)?
                }
                (Image::Alias, Origin::Bus(_)) => return None,
                (Image::Copy, _) => {
                    let img = new_image(device, pixel, w, h)?;
                    let row = w.div_ceil(bw) as usize * bytes;
                    let mut packed: Vec<u8> = (0..h.div_ceil(bw) as usize)
                        .flat_map(|y| {
                            let start = at + y * pitch;
                            (start..start + row).map(|i| source.get(i).copied().unwrap_or(0))
                        })
                        .collect();
                    upload(&img, w, h, &mut packed, row)?;
                    img
                }
                (Image::Decoded(float), _) => {
                    let img = new_image(device, pixel, w, h)?;
                    let texels = t.decode(vram, l, layer);
                    let mut data: Vec<u8> = if float {
                        texels.iter().flatten().flat_map(|c| c.to_le_bytes()).collect()
                    } else {
                        texels.iter().flatten().map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8).collect()
                    };
                    upload(&img, w, h, &mut data, w as usize * if float { 16 } else { 4 })?;
                    img
                }
            };
            images.push(img);
        }
    }
    let shadow = (k.image != Image::Alias).then(|| source.to_vec());
    Some(Entry { images, shadow, checked: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba8_holds_only_components_it_stores_exactly() {
        for (format, exact) in [(0, true), (2, true), (3, true), (10, true), (12, true), (5, false), (6, false), (11, false), (13, false), (0xf, false)] {
            assert_eq!(fits_rgba8(format, 0), exact, "format {format:#x}");
        }
        assert!(!fits_rgba8(12, 1), "signed components need negative values");
    }

    #[test]
    fn textures_metal_cannot_alias_are_copied_or_decoded() {
        let vram = 1 << 20;
        let at = |mem: TexMem, format: u32| plan(&Texture::new(mem, 8, 8, format, 0, None, 0, 1, 0), vram, true).expect("a small texture has a plan").key.image;
        assert_eq!(at(TexMem::Vram(0x1000), 12), Image::Alias);
        assert_eq!(at(TexMem::Vram(0x1004), 12), Image::Copy, "unaligned levels are copied");
        assert_eq!(at(TexMem::Vram(0x1000), 0xf), Image::Copy, "DXT has no linear layout");
        assert_eq!(at(TexMem::Copy { bytes: vec![0; 256], addr: 0 }, 12), Image::Copy, "system memory is copied");
        assert_eq!(at(TexMem::Vram(0x1000), 7), Image::Decoded(true), "Metal has no 5:5:6 format");
        let p = plan(&Texture::new(TexMem::Vram(0x1000), 8, 8, 0xf, 0, None, 1, 4, 0), vram, true).unwrap();
        assert!(p.manual && p.key.image == Image::Decoded(true), "volumes are read texel by texel, which DXT cannot be");
        assert_eq!(p.key.layers, 4);
    }
}

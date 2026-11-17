//! Texture units: TX_FORMAT0..2, TX_FILTER0 and TX_OFFSET, texel formats,
//! wrap modes and point or bilinear filtering. Formats are numbered as in
//! the R3xx register reference; DXT1/3/5 take the reserved numbers 15 to 17,
//! as in Mesa's r300_reg.h. Mipmap level layout follows Mesa's
//! r300_texture_desc.c (MIT license; Marek Olšák and others).

use super::V4;

#[derive(Clone)]
pub(crate) enum TexMem {
    /// Byte offset into VRAM.
    Vram(usize),
    /// A copy of the texture's bytes from system memory.
    Copy(Vec<u8>),
}

#[derive(Clone)]
pub(crate) struct Texture {
    pub mem: TexMem,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    /// TX_FORMAT1's SEL_ALPHA, SEL_RED, SEL_GREEN and SEL_BLUE.
    pub sel: [u32; 4],
    pub filter: u32,
    pub border: u32,
    /// Byte offset and row pitch of each mipmap level.
    pub levels: Vec<(usize, usize)>,
}

/// Bytes per block and block width and height.
pub(crate) fn block(format: u32) -> (usize, u32) {
    match format {
        0 | 2 | 5 | 0x12 => (1, 1),
        1 | 3 | 6 | 7 | 10 | 11 | 0x14 | 0x15 | 0x18 => (2, 1),
        4 | 8 | 9 | 12 | 13 | 0x13 | 0x19 | 0x1b | 0x1e => (4, 1),
        14 | 0x1a | 0x1c => (8, 1),
        0x1d => (16, 1),
        0xf => (8, 4),
        0x10 | 0x11 => (16, 4),
        _ => (4, 1),
    }
}

impl Texture {
    /// Lays out `levels` mipmap levels after the base at `mem`, as the
    /// hardware addresses them when TXPITCH_EN is clear.
    pub fn new(mem: TexMem, width: u32, height: u32, format: u32, num_levels: u32, pitch_texels: Option<u32>) -> Self {
        let (bytes, bw) = block(format);
        let mut levels = Vec::new();
        let mut off = 0usize;
        for l in 0..=num_levels.min(11) {
            let w = (width >> l).max(1);
            let h = (height >> l).max(1);
            let blocks_x = w.div_ceil(bw) as usize;
            let pitch = match (l, pitch_texels) {
                (0, Some(p)) => p.div_ceil(bw) as usize * bytes,
                _ => (blocks_x * bytes).next_multiple_of(32),
            };
            let rows = if num_levels > 0 { h.next_power_of_two() } else { h }.div_ceil(bw) as usize;
            levels.push((off, pitch));
            off = (off + pitch * rows).next_multiple_of(32);
        }
        Texture { mem, width, height, format, sel: [0, 1, 2, 3], filter: 0, border: 0, levels }
    }

    /// Bytes the texture spans from its base.
    pub fn span(&self) -> usize {
        let (bytes, bw) = block(self.format);
        let mut end = 0;
        for (l, &(off, pitch)) in self.levels.iter().enumerate() {
            let h = (self.height >> l).max(1);
            let rows = if self.levels.len() > 1 { h.next_power_of_two() } else { h }.div_ceil(bw) as usize;
            end = end.max(off + pitch * rows.max(1)).max(off + bytes);
        }
        end
    }

    fn bytes<'a>(&'a self, vram: &'a [u8]) -> (&'a [u8], usize) {
        match &self.mem {
            TexMem::Vram(o) => (vram, *o),
            TexMem::Copy(v) => (v, 0),
        }
    }

    /// The raw components of texel (x, y) of `level`, C0 first.
    fn texel(&self, vram: &[u8], level: usize, x: u32, y: u32) -> V4 {
        let (data, base) = self.bytes(vram);
        let (off, pitch) = self.levels[level];
        let (bytes, bw) = block(self.format);
        let a = base + off + (y / bw) as usize * pitch + (x / bw) as usize * bytes;
        let rd = |i: usize| -> u32 {
            let p = (a + i) % data.len().max(1);
            *data.get(p).unwrap_or(&0) as u32
        };
        let d32 = |i: usize| rd(i) | rd(i + 1) << 8 | rd(i + 2) << 16 | rd(i + 3) << 24;
        let d16 = |i: usize| rd(i) | rd(i + 1) << 8;
        let un = |v: u32, bits: u32| (v & ((1 << bits) - 1)) as f32 / ((1u32 << bits) - 1) as f32;
        let fields = |v: u32, widths: &[u32]| {
            let mut out = [0.0, 0.0, 0.0, 1.0];
            let mut shift = 0;
            for (c, &w) in widths.iter().enumerate() {
                out[c] = un(v >> shift, w);
                shift += w;
            }
            out
        };
        match self.format {
            0 => fields(rd(0), &[8]),
            1 => fields(d16(0), &[16]),
            2 => fields(rd(0), &[4, 4]),
            3 => fields(d16(0), &[8, 8]),
            4 => fields(d32(0), &[16, 16]),
            5 => fields(rd(0), &[2, 3, 3]),
            6 => fields(d16(0), &[5, 6, 5]),
            7 => fields(d16(0), &[5, 5, 6]),
            8 => fields(d32(0), &[10, 11, 11]),
            9 => fields(d32(0), &[11, 11, 10]),
            10 => fields(d16(0), &[4, 4, 4, 4]),
            11 => fields(d16(0), &[5, 5, 5, 1]),
            12 => fields(d32(0), &[8, 8, 8, 8]),
            13 => fields(d32(0), &[10, 10, 10, 2]),
            14 => [0, 1, 2, 3].map(|c| un(d16(2 * c), 16)),
            0xf..=0x11 => dxt(self.format, &|i| rd(i) as u8, x % 4, y % 4),
            0x18 => [half(d16(0)), 0.0, 0.0, 1.0],
            0x19 => [half(d16(0)), half(d16(2)), 0.0, 1.0],
            0x1a => [0, 1, 2, 3].map(|c| half(d16(2 * c))),
            0x1b => [f32::from_bits(d32(0)), 0.0, 0.0, 1.0],
            0x1c => [f32::from_bits(d32(0)), f32::from_bits(d32(4)), 0.0, 1.0],
            0x1d => [0, 1, 2, 3].map(|c| f32::from_bits(d32(4 * c))),
            _ => fields(d32(0), &[8, 8, 8, 8]),
        }
    }

    fn swizzle(&self, c: V4) -> V4 {
        let pick = |s: u32| match s {
            0..=3 => c[s as usize],
            5 => 1.0,
            _ => 0.0,
        };
        [pick(self.sel[1]), pick(self.sel[2]), pick(self.sel[3]), pick(self.sel[0])]
    }

    fn border_colour(&self) -> V4 {
        let b = self.border;
        let comp = |s: u32| ((b >> s) & 0xff) as f32 / 255.0;
        [comp(16), comp(8), comp(0), comp(24)]
    }

    /// Texel coordinate `i` after the wrap mode, or None for the border colour.
    fn wrap(mode: u32, i: i64, size: u32) -> Option<u32> {
        let n = size as i64;
        Some(match mode {
            0 => i.rem_euclid(n),
            1 => {
                let m = i.rem_euclid(2 * n);
                if m < n { m } else { 2 * n - 1 - m }
            }
            2 | 4 => i.clamp(0, n - 1),
            3 | 5 => {
                let m = if i < 0 { -1 - i } else { i };
                m.clamp(0, n - 1)
            }
            _ => {
                let m = if mode == 7 && i < 0 { -1 - i } else { i };
                if m < 0 || m >= n {
                    return None;
                }
                m
            }
        } as u32)
    }

    fn fetch(&self, vram: &[u8], level: usize, x: i64, y: i64) -> V4 {
        let (w, h) = ((self.width >> level).max(1), (self.height >> level).max(1));
        match (Self::wrap(self.filter & 7, x, w), Self::wrap((self.filter >> 3) & 7, y, h)) {
            (Some(x), Some(y)) => self.swizzle(self.texel(vram, level, x, y)),
            _ => self.border_colour(),
        }
    }

    /// Samples at normalized (s, t) with level-of-detail `lod` (log2 of
    /// texels per pixel) and returns RGBA.
    pub fn sample(&self, vram: &[u8], s: f32, t: f32, lod: f32) -> V4 {
        let minify = lod > 0.0;
        let mip = (self.filter >> 13) & 3;
        let max_level = self.levels.len() - 1;
        let level = if minify && mip != 0 {
            let base = ((self.filter >> 17) & 0xf) as usize;
            ((lod.round() as usize) + base).min(max_level)
        } else {
            0
        };
        let linear = if minify { (self.filter >> 11) & 3 == 2 } else { (self.filter >> 9) & 3 == 2 };
        let (w, h) = ((self.width >> level).max(1) as f32, (self.height >> level).max(1) as f32);
        let (u, v) = (s * w, t * h);
        if !linear {
            return self.fetch(vram, level, u.floor() as i64, v.floor() as i64);
        }
        let (u, v) = (u - 0.5, v - 0.5);
        let (x0, y0) = (u.floor(), v.floor());
        let (fx, fy) = (u - x0, v - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let t00 = self.fetch(vram, level, x0, y0);
        let t10 = self.fetch(vram, level, x0 + 1, y0);
        let t01 = self.fetch(vram, level, x0, y0 + 1);
        let t11 = self.fetch(vram, level, x0 + 1, y0 + 1);
        [0, 1, 2, 3].map(|c| {
            let top = t00[c] + (t10[c] - t00[c]) * fx;
            let bottom = t01[c] + (t11[c] - t01[c]) * fx;
            top + (bottom - top) * fy
        })
    }
}

fn half(h: u32) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1f) as i32;
    let m = (h & 0x3ff) as f32;
    s * match e {
        0 => m * (2f32).powi(-24),
        31 => if m == 0.0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m / 1024.0) * (2f32).powi(e - 15),
    }
}

fn rgb565(c: u16) -> [f32; 3] {
    [
        ((c >> 11) & 31) as f32 / 31.0,
        ((c >> 5) & 63) as f32 / 63.0,
        (c & 31) as f32 / 31.0,
    ]
}

/// One texel of a DXT block, as (B, G, R, A) to match the component order of
/// TX_FMT_8_8_8_8.
fn dxt(format: u32, rd: &dyn Fn(usize) -> u8, x: u32, y: u32) -> V4 {
    let colour_at = if format == 0xf { 0 } else { 8 };
    let c0 = rd(colour_at) as u16 | (rd(colour_at + 1) as u16) << 8;
    let c1 = rd(colour_at + 2) as u16 | (rd(colour_at + 3) as u16) << 8;
    let (a, b) = (rgb565(c0), rgb565(c1));
    let bits = rd(colour_at + 4 + y as usize) >> (2 * x) & 3;
    let mix = |wa: f32, wb: f32| [0, 1, 2].map(|i| a[i] * wa + b[i] * wb);
    let (rgb, mut alpha) = if c0 > c1 || format != 0xf {
        match bits {
            0 => (a, 1.0),
            1 => (b, 1.0),
            2 => (mix(2.0 / 3.0, 1.0 / 3.0), 1.0),
            _ => (mix(1.0 / 3.0, 2.0 / 3.0), 1.0),
        }
    } else {
        match bits {
            0 => (a, 1.0),
            1 => (b, 1.0),
            2 => (mix(0.5, 0.5), 1.0),
            _ => ([0.0; 3], 0.0),
        }
    };
    let i = (y * 4 + x) as usize;
    if format == 0x10 {
        alpha = ((rd(i / 2) >> (4 * (i % 2))) & 15) as f32 / 15.0;
    } else if format == 0x11 {
        let (a0, a1) = (rd(0) as f32, rd(1) as f32);
        let mut code = 0u64;
        for k in 0..6 {
            code |= (rd(2 + k) as u64) << (8 * k);
        }
        let idx = (code >> (3 * i)) & 7;
        let v = match idx {
            0 => a0,
            1 => a1,
            _ if a0 > a1 => ((8 - idx) as f32 * a0 + (idx - 1) as f32 * a1) / 7.0,
            6 => 0.0,
            7 => 255.0,
            _ => ((6 - idx) as f32 * a0 + (idx - 1) as f32 * a1) / 5.0,
        };
        alpha = v / 255.0;
    }
    [rgb[2], rgb[1], rgb[0], alpha]
}

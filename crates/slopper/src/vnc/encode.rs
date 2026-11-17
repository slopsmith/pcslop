//! Rectangle encodings for framebuffer updates: Raw and ZRLE (RFC 6143
//! section 7.7.6).

use flate2::{Compress, Compression, FlushCompress};

use super::proto::{self, PixelFormat, Rect};

pub const ENC_RAW: i32 = proto::ENC_RAW;
pub const ENC_ZRLE: i32 = 16;
const ENC_COMPRESS_LEVEL_0: i32 = -256;
const ENC_COMPRESS_LEVEL_9: i32 = -247;

const ZRLE_TILE: usize = 64;
const ZRLE_MAX_PALETTE: usize = 127;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Raw,
    Zrle,
}

/// Encodes update rectangles in the encoding the client prefers, keeping the
/// zlib stream that ZRLE carries across a whole connection.
pub struct Encoder {
    pub kind: Kind,
    level: u32,
    zrle: Option<Compress>,
    palette: Palette,
    tile: Vec<u32>,
    data: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Encoder {
        Encoder {
            kind: Kind::Raw,
            level: 6,
            zrle: None,
            palette: Palette::new(),
            tile: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Picks the first encoding in the client's SetEncodings list that this
    /// server knows, and the zlib level from a CompressLevel pseudo-encoding.
    /// A new level only applies to zlib streams that have not started yet.
    pub fn set_encodings(&mut self, encs: &[i32]) {
        self.kind = encs
            .iter()
            .find_map(|&e| match e {
                ENC_RAW => Some(Kind::Raw),
                ENC_ZRLE => Some(Kind::Zrle),
                _ => None,
            })
            .unwrap_or(Kind::Raw);
        if let Some(&e) = encs.iter().find(|&&e| (ENC_COMPRESS_LEVEL_0..=ENC_COMPRESS_LEVEL_9).contains(&e)) {
            self.level = (e - ENC_COMPRESS_LEVEL_0) as u32;
        }
    }

    /// Appends rectangle `r` of `pixels`, a picture `stride` pixels wide.
    pub fn rect(&mut self, out: &mut Vec<u8>, pf: &PixelFormat, pixels: &[u32], stride: usize, r: Rect) {
        match self.kind {
            Kind::Raw => proto::raw_rect(out, pf, pixels, stride, r),
            Kind::Zrle => self.zrle_rect(out, pf, pixels, stride, r),
        }
    }

    fn cut(&mut self, pixels: &[u32], stride: usize, x: usize, y: usize, w: usize, h: usize) {
        self.tile.clear();
        for row in y..y + h {
            self.tile.extend_from_slice(&pixels[row * stride + x..row * stride + x + w]);
        }
    }

    fn zrle_rect(&mut self, out: &mut Vec<u8>, pf: &PixelFormat, pixels: &[u32], stride: usize, r: Rect) {
        let cpixel = CPixel::new(pf);
        self.data.clear();
        let (rx, ry, rw, rh) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        for ty in (0..rh).step_by(ZRLE_TILE) {
            for tx in (0..rw).step_by(ZRLE_TILE) {
                let (w, h) = (ZRLE_TILE.min(rw - tx), ZRLE_TILE.min(rh - ty));
                self.cut(pixels, stride, rx + tx, ry + ty, w, h);
                zrle_tile(&self.tile, w, h, &cpixel, &mut self.palette, &mut self.data);
            }
        }
        proto::rect_header(out, r, ENC_ZRLE);
        let level = self.level;
        let z = self.zrle.get_or_insert_with(|| Compress::new(Compression::new(level), true));
        let len_at = out.len();
        out.extend([0; 4]);
        deflate(z, &self.data, out);
        let len = (out.len() - len_at - 4) as u32;
        out[len_at..len_at + 4].copy_from_slice(&len.to_be_bytes());
    }
}

// Ends with a sync flush so the client can decode everything before the next message.
fn deflate(z: &mut Compress, input: &[u8], out: &mut Vec<u8>) {
    let start_in = z.total_in();
    out.reserve(input.len() / 2 + 64);
    loop {
        let used = (z.total_in() - start_in) as usize;
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(4096));
        }
        z.compress_vec(&input[used..], out, FlushCompress::Sync).expect("deflate into memory cannot fail");
        if (z.total_in() - start_in) as usize == input.len() && out.len() < out.capacity() {
            return;
        }
    }
}

// Most significant bit first, each row padded to a whole byte.
fn pack_indices(indices: impl Iterator<Item = u8>, bits: u32, out: &mut Vec<u8>) {
    let (mut acc, mut n) = (0u8, 0);
    for i in indices {
        acc |= i << (8 - bits - n);
        n += bits;
        if n == 8 {
            out.push(acc);
            (acc, n) = (0, 0);
        }
    }
    if n > 0 {
        out.push(acc);
    }
}

// ZRLE's CPIXEL drops the unused byte of a 32-bit pixel whose colour bits
// all sit in its low or high three bytes.
struct CPixel {
    pf: PixelFormat,
    skip: usize,
    len: usize,
}

impl CPixel {
    fn new(pf: &PixelFormat) -> CPixel {
        let bits = |i: usize| (pf.max[i] as u32) << pf.shift[i];
        let used = bits(0) | bits(1) | bits(2);
        let full = CPixel { pf: *pf, skip: 0, len: pf.bpp as usize / 8 };
        if !(pf.true_color && pf.bpp == 32 && pf.depth <= 24) {
            return full;
        }
        let low = used < 1 << 24;
        let high = used & 0xff == 0;
        let skip = match (low, high, pf.big_endian) {
            (true, _, false) | (false, true, true) => 0,
            (true, _, true) | (false, true, false) => 1,
            _ => return full,
        };
        CPixel { pf: *pf, skip, len: 3 }
    }

    fn put(&self, p: u32, out: &mut Vec<u8>) {
        let (b, _) = self.pf.pixel_bytes(p);
        out.extend_from_slice(&b[self.skip..self.skip + self.len]);
    }
}

fn run_len_bytes(len: usize) -> usize {
    (len - 1) / 255 + 1
}

fn put_run_len(len: usize, out: &mut Vec<u8>) {
    let mut rest = len - 1;
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest as u8);
}

fn zrle_tile(px: &[u32], w: usize, h: usize, cp: &CPixel, pal: &mut Palette, out: &mut Vec<u8>) {
    pal.clear();
    let indexed = px.iter().all(|&p| pal.index(p, ZRLE_MAX_PALETTE).is_some());
    let colors = pal.colors.len();
    if indexed && colors == 1 {
        out.push(1);
        cp.put(px[0], out);
        return;
    }
    let runs = px.chunk_by(|a, b| a == b);
    let (mut plain_rle, mut palette_rle) = (0, 0);
    for run in runs.clone() {
        plain_rle += cp.len + run_len_bytes(run.len());
        palette_rle += if run.len() == 1 { 1 } else { 1 + run_len_bytes(run.len()) };
    }
    let raw = px.len() * cp.len;
    let bits = match colors {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    };
    let packed = if indexed && colors <= 16 { colors * cp.len + h * (w * bits).div_ceil(8) } else { usize::MAX };
    let palette_rle = if indexed { colors * cp.len + palette_rle } else { usize::MAX };
    let best = raw.min(plain_rle).min(packed).min(palette_rle);
    if best == packed {
        out.push(colors as u8);
        pal.colors.iter().for_each(|&c| cp.put(c, out));
        for row in px.chunks(w) {
            pack_indices(row.iter().map(|&p| pal.index(p, ZRLE_MAX_PALETTE).unwrap()), bits as u32, out);
        }
    } else if best == palette_rle {
        out.push(128 + colors as u8);
        pal.colors.iter().for_each(|&c| cp.put(c, out));
        for run in runs {
            let i = pal.index(run[0], ZRLE_MAX_PALETTE).unwrap();
            if run.len() == 1 {
                out.push(i);
            } else {
                out.push(i | 0x80);
                put_run_len(run.len(), out);
            }
        }
    } else if best == plain_rle {
        out.push(128);
        for run in runs {
            cp.put(run[0], out);
            put_run_len(run.len(), out);
        }
    } else {
        out.push(0);
        px.iter().for_each(|&p| cp.put(p, out));
    }
}

const SLOTS: usize = 1024;

// Distinct colours in order of first use, found through a small hash table.
struct Palette {
    colors: Vec<u32>,
    keys: Box<[u32; SLOTS]>,
    // Palette index plus one; zero marks a free slot.
    vals: Box<[u16; SLOTS]>,
    used: Vec<usize>,
}

impl Palette {
    fn new() -> Palette {
        Palette { colors: Vec::new(), keys: Box::new([0; SLOTS]), vals: Box::new([0; SLOTS]), used: Vec::new() }
    }

    fn clear(&mut self) {
        for &s in &self.used {
            self.vals[s] = 0;
        }
        self.used.clear();
        self.colors.clear();
    }

    fn index(&mut self, p: u32, max: usize) -> Option<u8> {
        let mut s = (p.wrapping_mul(0x9e37_79b1) >> 22) as usize;
        loop {
            match self.vals[s] {
                0 => {
                    if self.colors.len() >= max {
                        return None;
                    }
                    self.colors.push(p);
                    self.keys[s] = p;
                    self.vals[s] = self.colors.len() as u16;
                    self.used.push(s);
                    return Some((self.colors.len() - 1) as u8);
                }
                v if self.keys[s] == p => return Some((v - 1) as u8),
                _ => s = (s + 1) % SLOTS,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Decompress, FlushDecompress};

    struct Inflater(Decompress);

    impl Inflater {
        fn new() -> Inflater {
            Inflater(Decompress::new(true))
        }

        fn inflate(&mut self, data: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(1 << 20);
            let before = self.0.total_in();
            self.0.decompress_vec(data, &mut out, FlushDecompress::Sync).unwrap();
            assert_eq!((self.0.total_in() - before) as usize, data.len());
            out
        }
    }

    fn rect_header(b: &[u8]) -> (Rect, i32) {
        let v = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        (Rect { x: v(0), y: v(2), w: v(4), h: v(6) }, i32::from_be_bytes(b[8..12].try_into().unwrap()))
    }

    // An independent decoder written from RFC 6143, for 3-byte CPIXELs.
    fn decode_zrle(z: &mut Inflater, msg: &[u8]) -> (Rect, Vec<u32>) {
        let (r, enc) = rect_header(msg);
        assert_eq!(enc, ENC_ZRLE);
        let len = u32::from_be_bytes(msg[12..16].try_into().unwrap()) as usize;
        assert_eq!(msg.len(), 16 + len);
        let data = z.inflate(&msg[16..]);
        let mut d = data.as_slice();
        let mut take = |n: usize| {
            let (a, b) = d.split_at(n);
            d = b;
            a.to_vec()
        };
        let pixel = |b: &[u8]| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16;
        let (w, h) = (r.w as usize, r.h as usize);
        let mut img = vec![0u32; w * h];
        for ty in (0..h).step_by(64) {
            for tx in (0..w).step_by(64) {
                let (tw, th) = (64.min(w - tx), 64.min(h - ty));
                let mut tile = Vec::with_capacity(tw * th);
                let sub = take(1)[0] as usize;
                let read_len = |take: &mut dyn FnMut(usize) -> Vec<u8>| {
                    let mut n = 1;
                    loop {
                        let b = take(1)[0] as usize;
                        n += b;
                        if b != 255 {
                            return n;
                        }
                    }
                };
                match sub {
                    0 => (0..tw * th).for_each(|_| tile.push(pixel(&take(3)))),
                    1 => tile.resize(tw * th, pixel(&take(3))),
                    2..=16 => {
                        let pal: Vec<u32> = (0..sub).map(|_| pixel(&take(3))).collect();
                        let bits = match sub {
                            2 => 1,
                            3..=4 => 2,
                            _ => 4,
                        };
                        for _ in 0..th {
                            let row = take((tw * bits).div_ceil(8));
                            for x in 0..tw {
                                let bit = x * bits;
                                let i = (row[bit / 8] >> (8 - bits - bit % 8)) & ((1 << bits) - 1) as u8;
                                tile.push(pal[i as usize]);
                            }
                        }
                    }
                    128 => {
                        while tile.len() < tw * th {
                            let p = pixel(&take(3));
                            let n = read_len(&mut take);
                            tile.extend(std::iter::repeat_n(p, n));
                        }
                    }
                    130..=255 => {
                        let pal: Vec<u32> = (0..sub - 128).map(|_| pixel(&take(3))).collect();
                        while tile.len() < tw * th {
                            let i = take(1)[0];
                            let n = if i & 0x80 != 0 { read_len(&mut take) } else { 1 };
                            tile.extend(std::iter::repeat_n(pal[(i & 0x7f) as usize], n));
                        }
                    }
                    s => panic!("bad subencoding {s}"),
                }
                assert_eq!(tile.len(), tw * th);
                for y in 0..th {
                    img[(ty + y) * w + tx..(ty + y) * w + tx + tw].copy_from_slice(&tile[y * tw..(y + 1) * tw]);
                }
            }
        }
        assert!(d.is_empty());
        (r, img)
    }

    fn picture(w: usize, h: usize) -> Vec<u32> {
        (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                match (x / 40 + y / 30) % 4 {
                    0 => 0x3a6ea5,
                    1 => ((x * 7) as u32 & 0xff) << 8 | (y as u32 & 0xff),
                    2 => [0xffffff, 0x000000, 0xd4d0c8][(x + y) % 3],
                    _ => ((x * 2654435761) ^ (y * 40503)) as u32 & 0xffffff,
                }
            })
            .collect()
    }

    fn whole(w: usize, h: usize) -> Rect {
        Rect { x: 0, y: 0, w: w as u16, h: h as u16 }
    }

    fn crop(px: &[u32], stride: usize, r: Rect) -> Vec<u32> {
        (r.y as usize..(r.y + r.h) as usize)
            .flat_map(|y| px[y * stride + r.x as usize..y * stride + (r.x + r.w) as usize].iter().copied())
            .collect()
    }

    #[test]
    fn picks_the_first_known_encoding_in_client_order() {
        let mut e = Encoder::new();
        e.set_encodings(&[5, ENC_ZRLE, ENC_RAW]);
        assert_eq!(e.kind, Kind::Zrle);
        e.set_encodings(&[ENC_RAW, ENC_ZRLE]);
        assert_eq!(e.kind, Kind::Raw);
        e.set_encodings(&[-223, 5]);
        assert_eq!(e.kind, Kind::Raw);
    }

    #[test]
    fn reads_the_compress_level() {
        let mut e = Encoder::new();
        e.set_encodings(&[ENC_ZRLE, -254]);
        assert_eq!(e.level, 2);
    }

    #[test]
    fn zrle_round_trips_through_one_zlib_stream() {
        let (w, h) = (300, 170);
        let px = picture(w, h);
        let mut e = Encoder::new();
        e.set_encodings(&[ENC_ZRLE]);
        let mut z = Inflater::new();
        for r in [whole(w, h), Rect { x: 13, y: 70, w: 100, h: 65 }, Rect { x: 299, y: 169, w: 1, h: 1 }] {
            let mut out = Vec::new();
            e.rect(&mut out, &PixelFormat::NATIVE, &px, w, r);
            assert_eq!(decode_zrle(&mut z, &out), (r, crop(&px, w, r)));
        }
    }

    fn zrle_subencoding(px: &[u32]) -> u8 {
        let mut out = Vec::new();
        zrle_tile(px, 64, 64, &CPixel::new(&PixelFormat::NATIVE), &mut Palette::new(), &mut out);
        out[0]
    }

    #[test]
    fn zrle_picks_solid_tiles() {
        assert_eq!(zrle_subencoding(&[5; 64 * 64]), 1);
    }

    #[test]
    fn zrle_packs_few_scattered_colours() {
        let two: Vec<u32> = (0..64 * 64).map(|i| (i % 3 == 0) as u32).collect();
        assert_eq!(zrle_subencoding(&two), 2);
    }

    #[test]
    fn zrle_uses_palette_runs_for_short_runs_of_many_colours() {
        let px: Vec<u32> = (0..64 * 64).map(|i| (i / 3) as u32 % 20).collect();
        assert_eq!(zrle_subencoding(&px), 128 + 20);
    }

    #[test]
    fn zrle_uses_plain_runs_past_the_palette_limit() {
        let px: Vec<u32> = (0..64 * 64).map(|i| (i / 16) as u32 * 0x010101).collect();
        assert_eq!(zrle_subencoding(&px), 128);
    }

    #[test]
    fn zrle_sends_noise_raw() {
        let px: Vec<u32> = (0..64 * 64).map(|i| (i as u32).wrapping_mul(2654435761) & 0xffffff).collect();
        assert_eq!(zrle_subencoding(&px), 0);
    }

    #[test]
    fn zrle_sends_three_byte_pixels_only_when_they_fit() {
        assert_eq!(CPixel::new(&PixelFormat::NATIVE).len, 3);
        let high = PixelFormat { shift: [24, 16, 8], ..PixelFormat::NATIVE };
        assert_eq!((CPixel::new(&high).len, CPixel::new(&high).skip), (3, 1));
        let be = PixelFormat { big_endian: true, ..PixelFormat::NATIVE };
        assert_eq!((CPixel::new(&be).len, CPixel::new(&be).skip), (3, 1));
        let rgb565 = PixelFormat { bpp: 16, depth: 16, big_endian: false, true_color: true, max: [31, 63, 31], shift: [11, 5, 0] };
        assert_eq!(CPixel::new(&rgb565).len, 2);
        let mut out = Vec::new();
        CPixel::new(&PixelFormat::NATIVE).put(0x112233, &mut out);
        assert_eq!(out, [0x33, 0x22, 0x11]);
    }

    #[test]
    fn indices_pack_most_significant_bit_first() {
        let mut v = Vec::new();
        pack_indices([1, 0, 1].into_iter(), 1, &mut v);
        pack_indices([3, 1, 2, 0, 1].into_iter(), 2, &mut v);
        pack_indices([0xa, 0x5, 0xf].into_iter(), 4, &mut v);
        assert_eq!(v, [0b1010_0000, 0b1101_1000, 0b0100_0000, 0xa5, 0xf0]);
    }

    #[test]
    fn palette_gives_up_past_its_limit() {
        let mut p = Palette::new();
        assert!((0..127).all(|c| p.index(c * 1024, 127).is_some()));
        assert_eq!(p.index(5 * 1024, 127), Some(5));
        assert_eq!(p.index(9999, 127), None);
        p.clear();
        assert_eq!(p.index(9999, 127), Some(0));
    }
}

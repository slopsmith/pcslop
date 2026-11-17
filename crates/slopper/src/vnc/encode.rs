//! Rectangle encodings for framebuffer updates: Raw, ZRLE (RFC 6143 section
//! 7.7.6) and lossless Tight (the community RFB specification, rfbproto,
//! "Tight Encoding").

use flate2::{Compress, Compression, FlushCompress};

use super::proto::{self, PixelFormat, Rect};

pub const ENC_RAW: i32 = proto::ENC_RAW;
pub const ENC_TIGHT: i32 = 7;
pub const ENC_ZRLE: i32 = 16;
const ENC_COMPRESS_LEVEL_0: i32 = -256;
const ENC_COMPRESS_LEVEL_9: i32 = -247;

const ZRLE_TILE: usize = 64;
const ZRLE_MAX_PALETTE: usize = 127;

const TIGHT_MAX_WIDTH: usize = 2048;
const TIGHT_MAX_PIXELS: usize = 65536;
const TIGHT_MIN_TO_COMPRESS: usize = 12;
const TIGHT_FILL: u8 = 0x80;
const TIGHT_EXPLICIT_FILTER: u8 = 0x40;
const TIGHT_FILTER_PALETTE: u8 = 1;
const TIGHT_STREAM_FULL: usize = 0;
const TIGHT_STREAM_MONO: usize = 1;
const TIGHT_STREAM_PALETTE: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Raw,
    Zrle,
    Tight,
}

/// Encodes update rectangles in the encoding the client prefers, keeping the
/// zlib streams that ZRLE and Tight carry across a whole connection.
pub struct Encoder {
    pub kind: Kind,
    level: u32,
    zrle: Option<Compress>,
    tight: [Option<Compress>; 4],
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
            tight: [None, None, None, None],
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
                ENC_TIGHT => Some(Kind::Tight),
                _ => None,
            })
            .unwrap_or(Kind::Raw);
        if let Some(&e) = encs.iter().find(|&&e| (ENC_COMPRESS_LEVEL_0..=ENC_COMPRESS_LEVEL_9).contains(&e)) {
            self.level = (e - ENC_COMPRESS_LEVEL_0) as u32;
        }
    }

    /// Splits rectangles into the pieces this encoding sends, each of which
    /// becomes one rectangle of the update.
    pub fn split(&self, rects: Vec<Rect>) -> Vec<Rect> {
        if self.kind != Kind::Tight {
            return rects;
        }
        let mut out = Vec::new();
        for r in rects {
            for x in (0..r.w as usize).step_by(TIGHT_MAX_WIDTH) {
                let w = TIGHT_MAX_WIDTH.min(r.w as usize - x);
                let band = (TIGHT_MAX_PIXELS / w).max(1);
                for y in (0..r.h as usize).step_by(band) {
                    let h = band.min(r.h as usize - y);
                    out.push(Rect { x: r.x + x as u16, y: r.y + y as u16, w: w as u16, h: h as u16 });
                }
            }
        }
        out
    }

    /// Appends rectangle `r` of `pixels`, a picture `stride` pixels wide.
    pub fn rect(&mut self, out: &mut Vec<u8>, pf: &PixelFormat, pixels: &[u32], stride: usize, r: Rect) {
        match self.kind {
            Kind::Raw => proto::raw_rect(out, pf, pixels, stride, r),
            Kind::Zrle => self.zrle_rect(out, pf, pixels, stride, r),
            Kind::Tight => self.tight_rect(out, pf, pixels, stride, r),
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

    fn tight_rect(&mut self, out: &mut Vec<u8>, pf: &PixelFormat, pixels: &[u32], stride: usize, r: Rect) {
        proto::rect_header(out, r, ENC_TIGHT);
        let tpixel = TPixel::new(pf);
        self.cut(pixels, stride, r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        let (w, h) = (r.w as usize, r.h as usize);
        self.palette.clear();
        let indexed = self.tile.iter().all(|&p| self.palette.index(p, 256).is_some());
        let colors = self.palette.colors.len();
        self.data.clear();
        let stream = if indexed && colors == 1 {
            out.push(TIGHT_FILL);
            tpixel.put(self.tile[0], out);
            return;
        } else if indexed && colors == 2 {
            out.extend([(TIGHT_STREAM_MONO as u8) << 4 | TIGHT_EXPLICIT_FILTER, TIGHT_FILTER_PALETTE, 1]);
            self.palette.colors.iter().for_each(|&c| tpixel.put(c, out));
            for row in self.tile.chunks(w) {
                pack_indices(row.iter().map(|&p| self.palette.index(p, 256).unwrap()), 1, &mut self.data);
            }
            TIGHT_STREAM_MONO
        } else if indexed {
            out.extend([(TIGHT_STREAM_PALETTE as u8) << 4 | TIGHT_EXPLICIT_FILTER, TIGHT_FILTER_PALETTE, (colors - 1) as u8]);
            self.palette.colors.iter().for_each(|&c| tpixel.put(c, out));
            for &p in &self.tile {
                self.data.push(self.palette.index(p, 256).unwrap());
            }
            TIGHT_STREAM_PALETTE
        } else {
            out.push((TIGHT_STREAM_FULL as u8) << 4);
            self.data.reserve(w * h * tpixel.len);
            self.tile.iter().for_each(|&p| tpixel.put(p, &mut self.data));
            TIGHT_STREAM_FULL
        };
        if self.data.len() < TIGHT_MIN_TO_COMPRESS {
            out.extend_from_slice(&self.data);
            return;
        }
        let level = self.level;
        let z = self.tight[stream].get_or_insert_with(|| Compress::new(Compression::new(level), true));
        let mut packed = Vec::with_capacity(self.data.len() / 2 + 64);
        deflate(z, &self.data, &mut packed);
        tight_length(packed.len(), out);
        out.extend_from_slice(&packed);
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

// 7 bits per byte, low bits first; the top bit says another byte follows.
fn tight_length(len: usize, out: &mut Vec<u8>) {
    debug_assert!(len < 1 << 22);
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x4000 {
        out.extend([len as u8 | 0x80, (len >> 7) as u8]);
    } else {
        out.extend([len as u8 | 0x80, (len >> 7) as u8 | 0x80, (len >> 14) as u8]);
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

// Tight's TPIXEL: red, green, blue bytes for 24-bit colour in 32-bit pixels.
struct TPixel {
    pf: PixelFormat,
    rgb: bool,
    len: usize,
}

impl TPixel {
    fn new(pf: &PixelFormat) -> TPixel {
        let rgb = pf.true_color && pf.bpp == 32 && pf.depth == 24 && pf.max == [255; 3];
        TPixel { pf: *pf, rgb, len: if rgb { 3 } else { pf.bpp as usize / 8 } }
    }

    fn put(&self, p: u32, out: &mut Vec<u8>) {
        if self.rgb {
            out.extend([(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        } else {
            let (b, n) = self.pf.pixel_bytes(p);
            out.extend_from_slice(&b[..n]);
        }
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

    // An independent decoder written from rfbproto, for RGB TPIXELs. Also
    // returns how many bytes of `msg` the rectangle took.
    fn decode_tight(zs: &mut [Inflater; 4], msg: &[u8]) -> (Rect, Vec<u32>, usize) {
        let (r, enc) = rect_header(msg);
        assert_eq!(enc, ENC_TIGHT);
        let mut d = &msg[12..];
        let n = r.w as usize * r.h as usize;
        let rgb = |b: &[u8]| u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        let ctl = d[0];
        d = &d[1..];
        if ctl == TIGHT_FILL {
            return (r, vec![rgb(d); n], 16);
        }
        let stream = (ctl >> 4 & 3) as usize;
        let mut palette = Vec::new();
        if ctl & TIGHT_EXPLICIT_FILTER != 0 {
            assert_eq!(d[0], TIGHT_FILTER_PALETTE);
            let colors = d[1] as usize + 1;
            palette = d[2..2 + 3 * colors].chunks(3).map(rgb).collect();
            d = &d[2 + 3 * colors..];
        }
        let row_bytes = match palette.len() {
            0 => r.w as usize * 3,
            2 => (r.w as usize).div_ceil(8),
            _ => r.w as usize,
        };
        let raw_len = row_bytes * r.h as usize;
        let (data, used) = if raw_len < TIGHT_MIN_TO_COMPRESS {
            (d[..raw_len].to_vec(), raw_len)
        } else {
            let (mut len, mut i) = (0usize, 0);
            loop {
                len |= ((d[i] & 0x7f) as usize) << (7 * i);
                i += 1;
                if d[i - 1] & 0x80 == 0 || i == 3 {
                    break;
                }
            }
            (zs[stream].inflate(&d[i..i + len]), i + len)
        };
        assert_eq!(data.len(), raw_len);
        let px = match palette.len() {
            0 => data.chunks(3).map(rgb).collect(),
            2 => data
                .chunks(row_bytes)
                .flat_map(|row| (0..r.w as usize).map(move |x| (row[x / 8] >> (7 - x % 8)) & 1))
                .map(|i| palette[i as usize])
                .collect(),
            _ => data.iter().map(|&i| palette[i as usize]).collect(),
        };
        (r, px, msg.len() - d.len() + used)
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
        e.set_encodings(&[5, ENC_TIGHT, ENC_ZRLE, ENC_RAW]);
        assert_eq!(e.kind, Kind::Tight);
        e.set_encodings(&[ENC_ZRLE, ENC_TIGHT]);
        assert_eq!(e.kind, Kind::Zrle);
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
    fn tight_round_trips_every_kind_of_rect() {
        let (w, h) = (300, 170);
        let px = picture(w, h);
        let mut e = Encoder::new();
        e.set_encodings(&[ENC_TIGHT]);
        let mut zs = [Inflater::new(), Inflater::new(), Inflater::new(), Inflater::new()];
        let cases = [
            Rect { x: 0, y: 0, w: 40, h: 30 },
            Rect { x: 80, y: 0, w: 40, h: 30 },
            Rect { x: 120, y: 0, w: 40, h: 30 },
            whole(w, h),
            Rect { x: 1, y: 1, w: 2, h: 1 },
        ];
        for r in cases {
            let mut out = Vec::new();
            e.rect(&mut out, &PixelFormat::NATIVE, &px, w, r);
            let (got, img, used) = decode_tight(&mut zs, &out);
            assert_eq!((got, used), (r, out.len()));
            assert_eq!(img, crop(&px, w, r));
        }
    }

    #[test]
    fn tight_chooses_fill_mono_palette_and_full_colour() {
        let mut e = Encoder::new();
        e.set_encodings(&[ENC_TIGHT]);
        let control = |e: &mut Encoder, px: &[u32]| {
            let mut out = Vec::new();
            e.rect(&mut out, &PixelFormat::NATIVE, px, 64, whole(64, px.len() / 64));
            out[12]
        };
        assert_eq!(control(&mut e, &[7; 64 * 4]), TIGHT_FILL);
        let mono: Vec<u32> = (0..64 * 4).map(|i| i as u32 & 1).collect();
        assert_eq!(control(&mut e, &mono), 0x50);
        let pal: Vec<u32> = (0..64 * 4).map(|i| i as u32 % 9).collect();
        assert_eq!(control(&mut e, &pal), 0x60);
        let full: Vec<u32> = (0..64 * 8).map(|i| i as u32 * 77).collect();
        assert_eq!(control(&mut e, &full), 0x00);
    }

    #[test]
    fn tight_splits_rects_to_its_size_limits() {
        let mut e = Encoder::new();
        e.set_encodings(&[ENC_TIGHT]);
        let parts = e.split(vec![whole(2500, 100)]);
        assert!(parts.iter().all(|r| r.w as usize <= TIGHT_MAX_WIDTH && r.w as usize * r.h as usize <= TIGHT_MAX_PIXELS));
        assert_eq!(parts.iter().map(|r| r.w as usize * r.h as usize).sum::<usize>(), 2500 * 100);
        e.set_encodings(&[ENC_ZRLE]);
        assert_eq!(e.split(vec![whole(2500, 100)]), [whole(2500, 100)]);
    }

    #[test]
    fn tight_lengths_use_seven_bits_per_byte() {
        let enc = |n| {
            let mut v = Vec::new();
            tight_length(n, &mut v);
            v
        };
        assert_eq!(enc(0x7f), [0x7f]);
        assert_eq!(enc(0x80), [0x80, 0x01]);
        assert_eq!(enc(0x3fff), [0xff, 0x7f]);
        assert_eq!(enc(0x4000), [0x80, 0x80, 0x01]);
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

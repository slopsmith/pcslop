//! The parts of the RFB protocol (RFC 6143) the VNC server needs: the
//! handshake, pixel formats, client messages and framebuffer updates.

use std::io::{self, Read, Write};

pub const ENC_RAW: i32 = 0;
pub const ENC_DESKTOP_SIZE: i32 = -223;

const SEC_NONE: u8 = 1;
const SEC_VNC_AUTH: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bpp: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub max: [u16; 3],
    pub shift: [u8; 3],
}

impl PixelFormat {
    /// The frontend's own `0x00RRGGBB` pixels, sent little-endian.
    pub const NATIVE: PixelFormat =
        PixelFormat { bpp: 32, depth: 24, big_endian: false, true_color: true, max: [255; 3], shift: [16, 8, 0] };

    fn to_bytes(self) -> [u8; 16] {
        let [r, g, b] = self.max.map(u16::to_be_bytes);
        [
            self.bpp,
            self.depth,
            self.big_endian as u8,
            self.true_color as u8,
            r[0],
            r[1],
            g[0],
            g[1],
            b[0],
            b[1],
            self.shift[0],
            self.shift[1],
            self.shift[2],
            0,
            0,
            0,
        ]
    }

    fn from_bytes(b: &[u8; 16]) -> PixelFormat {
        let be = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        PixelFormat {
            bpp: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_color: b[3] != 0,
            max: [be(4), be(6), be(8)],
            shift: [b[10], b[11], b[12]],
        }
    }

    /// Whether the server can produce pixels in this format.
    pub fn supported(&self) -> bool {
        self.true_color && matches!(self.bpp, 8 | 16 | 32) && self.shift.iter().all(|&s| s < self.bpp)
    }

    /// Appends `0x00RRGGBB` pixels to `out` in this format.
    pub fn encode(&self, pixels: &[u32], out: &mut Vec<u8>) {
        if *self == Self::NATIVE {
            out.extend(pixels.iter().flat_map(|p| p.to_le_bytes()));
            return;
        }
        for &p in pixels {
            let mut v = 0u32;
            for (i, at) in [16, 8, 0].into_iter().enumerate() {
                let c = (p >> at) & 0xff;
                let max = self.max[i] as u32;
                v |= ((c * max + 127) / 255) << self.shift[i];
            }
            match (self.bpp, self.big_endian) {
                (8, _) => out.push(v as u8),
                (16, false) => out.extend((v as u16).to_le_bytes()),
                (16, true) => out.extend((v as u16).to_be_bytes()),
                (_, false) => out.extend(v.to_le_bytes()),
                (_, true) => out.extend(v.to_be_bytes()),
            }
        }
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// Protocol minor version (3, 7 or 8) for a client's `RFB xxx.yyy\n` reply.
/// Version 3.5 counts as 3.3, and newer ones (Apple's 3.889) as 3.8.
fn client_version(reply: &[u8; 12]) -> io::Result<u32> {
    let text = std::str::from_utf8(reply).map_err(|_| invalid("bad protocol version"))?;
    let rest = text.strip_prefix("RFB ").and_then(|r| r.strip_suffix('\n')).ok_or_else(|| invalid("bad protocol version"))?;
    let (major, minor) = rest.split_once('.').ok_or_else(|| invalid("bad protocol version"))?;
    let major: u32 = major.parse().map_err(|_| invalid("bad protocol version"))?;
    let minor: u32 = minor.parse().map_err(|_| invalid("bad protocol version"))?;
    match (major, minor) {
        (3, 0..=6) => Ok(3),
        (3, 7) => Ok(7),
        (3, _) => Ok(8),
        _ => Err(invalid("unsupported protocol version")),
    }
}

/// Runs the handshake up to and including ServerInit. With a password, the
/// client must answer `challenge` with `vnc_auth_response`. Returns an error
/// if the client fails authentication or breaks the protocol.
pub fn handshake<S: Read + Write>(
    s: &mut S,
    password: Option<&[u8]>,
    challenge: [u8; 16],
    size: (u16, u16),
    name: &str,
) -> io::Result<()> {
    s.write_all(b"RFB 003.008\n")?;
    let mut reply = [0; 12];
    s.read_exact(&mut reply)?;
    let minor = client_version(&reply)?;
    let sec = if password.is_some() { SEC_VNC_AUTH } else { SEC_NONE };
    if minor == 3 {
        s.write_all(&(sec as u32).to_be_bytes())?;
    } else {
        s.write_all(&[1, sec])?;
        let chosen = read_u8(s)?;
        if chosen != sec {
            if minor == 8 {
                write_failure(s, "unsupported security type")?;
            }
            return Err(invalid("client chose an unsupported security type"));
        }
    }
    if let Some(pw) = password {
        s.write_all(&challenge)?;
        let mut response = [0; 16];
        s.read_exact(&mut response)?;
        if response != super::des::vnc_auth_response(pw, &challenge) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if minor == 8 {
                write_failure(s, "authentication failed")?;
            } else {
                s.write_all(&1u32.to_be_bytes())?;
            }
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "authentication failed"));
        }
        s.write_all(&0u32.to_be_bytes())?;
    } else if minor == 8 {
        s.write_all(&0u32.to_be_bytes())?;
    }
    let _shared = read_u8(s)?;
    let mut init = Vec::with_capacity(24 + name.len());
    init.extend(size.0.to_be_bytes());
    init.extend(size.1.to_be_bytes());
    init.extend(PixelFormat::NATIVE.to_bytes());
    init.extend((name.len() as u32).to_be_bytes());
    init.extend(name.as_bytes());
    s.write_all(&init)?;
    s.flush()
}

fn write_failure(s: &mut impl Write, reason: &str) -> io::Result<()> {
    s.write_all(&1u32.to_be_bytes())?;
    s.write_all(&(reason.len() as u32).to_be_bytes())?;
    s.write_all(reason.as_bytes())?;
    s.flush()
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClientMsg {
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<i32>),
    UpdateRequest { incremental: bool },
    Key { down: bool, keysym: u32 },
    Pointer { mask: u8, x: u16, y: u16 },
    CutText,
}

/// Reads one client-to-server message.
pub fn read_client_msg(r: &mut impl Read) -> io::Result<ClientMsg> {
    let mut pad = [0; 3];
    Ok(match read_u8(r)? {
        0 => {
            r.read_exact(&mut pad)?;
            let mut pf = [0; 16];
            r.read_exact(&mut pf)?;
            ClientMsg::SetPixelFormat(PixelFormat::from_bytes(&pf))
        }
        2 => {
            r.read_exact(&mut pad[..1])?;
            let n = read_u16(r)?;
            let encs = (0..n).map(|_| read_u32(r).map(|e| e as i32)).collect::<io::Result<_>>()?;
            ClientMsg::SetEncodings(encs)
        }
        3 => {
            let incremental = read_u8(r)? != 0;
            let mut rect = [0; 8];
            r.read_exact(&mut rect)?;
            ClientMsg::UpdateRequest { incremental }
        }
        4 => {
            let down = read_u8(r)? != 0;
            r.read_exact(&mut pad[..2])?;
            ClientMsg::Key { down, keysym: read_u32(r)? }
        }
        5 => {
            let mask = read_u8(r)?;
            let x = read_u16(r)?;
            let y = read_u16(r)?;
            ClientMsg::Pointer { mask, x, y }
        }
        6 => {
            r.read_exact(&mut pad)?;
            let len = read_u32(r)? as u64;
            io::copy(&mut r.take(len), &mut io::sink())?;
            ClientMsg::CutText
        }
        t => return Err(invalid(&format!("unknown client message type {t}"))),
    })
}

/// Starts a FramebufferUpdate message with `rects` rectangles.
pub fn update_header(out: &mut Vec<u8>, rects: u16) {
    out.extend([0, 0]);
    out.extend(rects.to_be_bytes());
}

pub fn rect_header(out: &mut Vec<u8>, r: Rect, encoding: i32) {
    for v in [r.x, r.y, r.w, r.h] {
        out.extend(v.to_be_bytes());
    }
    out.extend(encoding.to_be_bytes());
}

/// Appends a Raw rectangle cut from `pixels`, a picture `stride` pixels wide.
pub fn raw_rect(out: &mut Vec<u8>, pf: &PixelFormat, pixels: &[u32], stride: usize, r: Rect) {
    rect_header(out, r, ENC_RAW);
    for row in r.y as usize..(r.y + r.h) as usize {
        let start = row * stride + r.x as usize;
        pf.encode(&pixels[start..start + r.w as usize], out);
    }
}

/// Side of the square tiles compared to find changed areas.
pub const TILE: usize = 32;

/// Rectangles covering every tile where `new` differs from `old`, both
/// pictures `width` x `height`. Neighbouring changed tiles on a tile row
/// merge into one rectangle, and equal rectangles on consecutive tile rows
/// merge too.
pub fn changed_rects(old: &[u32], new: &[u32], width: usize, height: usize) -> Vec<Rect> {
    let mut rects: Vec<Rect> = Vec::new();
    let mut prev_row: Vec<usize> = Vec::new();
    for ty in (0..height).step_by(TILE) {
        let th = TILE.min(height - ty);
        let mut row = Vec::new();
        let mut runs = Vec::new();
        let mut run: Option<(usize, usize)> = None;
        for tx in (0..width).step_by(TILE) {
            let tw = TILE.min(width - tx);
            let dirty = (ty..ty + th).any(|y| {
                let i = y * width + tx;
                old[i..i + tw] != new[i..i + tw]
            });
            run = match (run, dirty) {
                (Some((x, w)), true) => Some((x, w + tw)),
                (None, true) => Some((tx, tw)),
                (Some(r), false) => {
                    runs.push(r);
                    None
                }
                (None, false) => None,
            };
        }
        runs.extend(run);
        for (x, w) in runs {
            let (x, w) = (x as u16, w as u16);
            let above = prev_row.iter().copied().find(|&i| rects[i].x == x && rects[i].w == w);
            match above {
                Some(i) => {
                    rects[i].h += th as u16;
                    row.push(i);
                }
                None => {
                    row.push(rects.len());
                    rects.push(Rect { x, y: ty as u16, w, h: th as u16 });
                }
            }
        }
        prev_row = row;
    }
    rects
}

/// Copies `src` (`sw` x `sh`) into a `dw` x `dh` picture, cropping or padding
/// with black at the right and bottom.
pub fn fit(src: &[u32], sw: usize, sh: usize, dw: usize, dh: usize, out: &mut Vec<u32>) {
    out.clear();
    out.resize(dw * dh, 0);
    let (w, h) = (sw.min(dw), sh.min(dh));
    for y in 0..h {
        out[y * dw..y * dw + w].copy_from_slice(&src[y * sw..y * sw + w]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A connection with canned client bytes that records what the server sends.
    struct Pipe {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Pipe {
        fn new(input: &[u8]) -> Pipe {
            Pipe { input: Cursor::new(input.to_vec()), output: Vec::new() }
        }
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn server_init(w: u16, h: u16, name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.extend([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
        v.extend((name.len() as u32).to_be_bytes());
        v.extend(name.as_bytes());
        v
    }

    #[test]
    fn handshake_without_password_on_3_8() {
        let mut p = Pipe::new(b"RFB 003.008\n\x01\x01");
        handshake(&mut p, None, [0; 16], (720, 400), "vm").unwrap();
        let mut want = b"RFB 003.008\n\x01\x01\0\0\0\0".to_vec();
        want.extend(server_init(720, 400, "vm"));
        assert_eq!(p.output, want);
    }

    #[test]
    fn handshake_without_password_on_3_3() {
        let mut p = Pipe::new(b"RFB 003.003\n\x01");
        handshake(&mut p, None, [0; 16], (640, 480), "vm").unwrap();
        let mut want = b"RFB 003.008\n\0\0\0\x01".to_vec();
        want.extend(server_init(640, 480, "vm"));
        assert_eq!(p.output, want);
    }

    #[test]
    fn handshake_treats_apple_version_as_3_8() {
        let mut p = Pipe::new(b"RFB 003.889\n\x01\x01");
        handshake(&mut p, None, [0; 16], (720, 400), "vm").unwrap();
        assert_eq!(&p.output[12..18], b"\x01\x01\0\0\0\0");
    }

    #[test]
    fn handshake_with_the_right_password() {
        let challenge = *b"0123456789abcdef";
        let mut input = b"RFB 003.008\n\x02".to_vec();
        input.extend(crate::vnc::des::vnc_auth_response(b"secret", &challenge));
        input.push(1);
        let mut p = Pipe::new(&input);
        handshake(&mut p, Some(b"secret"), challenge, (720, 400), "vm").unwrap();
        let mut want = b"RFB 003.008\n\x01\x02".to_vec();
        want.extend(challenge);
        want.extend([0, 0, 0, 0]);
        want.extend(server_init(720, 400, "vm"));
        assert_eq!(p.output, want);
    }

    #[test]
    fn handshake_with_a_wrong_password_fails_with_a_reason() {
        let challenge = *b"0123456789abcdef";
        let mut input = b"RFB 003.008\n\x02".to_vec();
        input.extend(crate::vnc::des::vnc_auth_response(b"guess", &challenge));
        let mut p = Pipe::new(&input);
        let e = handshake(&mut p, Some(b"secret"), challenge, (720, 400), "vm").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        let tail = &p.output[12 + 2 + 16..];
        assert_eq!(&tail[..4], &[0, 0, 0, 1]);
        let len = u32::from_be_bytes(tail[4..8].try_into().unwrap()) as usize;
        assert_eq!(tail.len(), 8 + len);
    }

    #[test]
    fn handshake_rejects_no_auth_when_a_password_is_set() {
        let mut p = Pipe::new(b"RFB 003.008\n\x01");
        assert!(handshake(&mut p, Some(b"secret"), [0; 16], (720, 400), "vm").is_err());
        assert_eq!(&p.output[14..18], &[0, 0, 0, 1]);
    }

    #[test]
    fn handshake_rejects_other_security_types() {
        let mut p = Pipe::new(b"RFB 003.008\n\x02");
        assert!(handshake(&mut p, None, [0; 16], (720, 400), "vm").is_err());
        assert_eq!(&p.output[14..18], &[0, 0, 0, 1]);
    }

    #[test]
    fn handshake_rejects_other_protocols() {
        let mut p = Pipe::new(b"GET / HTTP/1");
        assert!(handshake(&mut p, None, [0; 16], (720, 400), "vm").is_err());
    }

    #[test]
    fn parses_client_messages() {
        let mut bytes = vec![0, 0, 0, 0, 16, 16, 1, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0];
        bytes.extend([2, 0, 0, 2, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x21]);
        bytes.extend([3, 1, 0, 0, 0, 0, 2, 208, 1, 144]);
        bytes.extend([4, 1, 0, 0, 0, 0, 0xff, 0x0d]);
        bytes.extend([5, 0x09, 1, 2, 0, 3]);
        bytes.extend([6, 0, 0, 0, 0, 0, 0, 3, b'a', b'b', b'c']);
        let mut r = Cursor::new(bytes);
        let pf = PixelFormat { bpp: 16, depth: 16, big_endian: true, true_color: true, max: [31, 63, 31], shift: [11, 5, 0] };
        assert_eq!(read_client_msg(&mut r).unwrap(), ClientMsg::SetPixelFormat(pf));
        assert_eq!(read_client_msg(&mut r).unwrap(), ClientMsg::SetEncodings(vec![0, -223]));
        assert_eq!(read_client_msg(&mut r).unwrap(), ClientMsg::UpdateRequest { incremental: true });
        assert_eq!(read_client_msg(&mut r).unwrap(), ClientMsg::Key { down: true, keysym: 0xff0d });
        assert_eq!(read_client_msg(&mut r).unwrap(), ClientMsg::Pointer { mask: 9, x: 258, y: 3 });
        assert_eq!(read_client_msg(&mut r).unwrap(), ClientMsg::CutText);
        assert_eq!(read_client_msg(&mut r).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn rejects_unknown_client_messages() {
        assert!(read_client_msg(&mut Cursor::new(vec![200, 0, 0])).is_err());
    }

    #[test]
    fn encodes_native_pixels_as_little_endian_words() {
        let mut out = Vec::new();
        PixelFormat::NATIVE.encode(&[0x00112233], &mut out);
        assert_eq!(out, [0x33, 0x22, 0x11, 0]);
    }

    #[test]
    fn encodes_rgb565_big_endian() {
        let pf = PixelFormat { bpp: 16, depth: 16, big_endian: true, true_color: true, max: [31, 63, 31], shift: [11, 5, 0] };
        let mut out = Vec::new();
        pf.encode(&[0xff0000, 0x00ff00, 0x0000ff, 0xffffff], &mut out);
        assert_eq!(out, [0xf8, 0x00, 0x07, 0xe0, 0x00, 0x1f, 0xff, 0xff]);
    }

    #[test]
    fn encodes_bgr233() {
        let pf = PixelFormat { bpp: 8, depth: 8, big_endian: false, true_color: true, max: [7, 7, 3], shift: [0, 3, 6] };
        let mut out = Vec::new();
        pf.encode(&[0xff0000, 0x00ff00, 0x0000ff, 0x000000], &mut out);
        assert_eq!(out, [0x07, 0x38, 0xc0, 0x00]);
    }

    #[test]
    fn colour_map_formats_are_unsupported() {
        let pf = PixelFormat { true_color: false, ..PixelFormat::NATIVE };
        assert!(!pf.supported());
        assert!(PixelFormat::NATIVE.supported());
    }

    #[test]
    fn raw_rect_cuts_rows_out_of_the_picture() {
        let pixels: Vec<u32> = (0..12).collect();
        let mut out = Vec::new();
        raw_rect(&mut out, &PixelFormat::NATIVE, &pixels, 4, Rect { x: 1, y: 1, w: 2, h: 2 });
        assert_eq!(&out[..12], &[0, 1, 0, 1, 0, 2, 0, 2, 0, 0, 0, 0]);
        let words: Vec<u32> = out[12..].chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(words, [5, 6, 9, 10]);
    }

    #[test]
    fn unchanged_pictures_have_no_changed_rects() {
        let a = vec![7u32; 100 * 70];
        assert!(changed_rects(&a, &a, 100, 70).is_empty());
    }

    #[test]
    fn a_changed_pixel_marks_its_tile() {
        let a = vec![0u32; 100 * 70];
        let mut b = a.clone();
        b[40 * 100 + 99] = 1;
        assert_eq!(changed_rects(&a, &b, 100, 70), [Rect { x: 96, y: 32, w: 4, h: 32 }]);
    }

    #[test]
    fn neighbouring_tiles_merge_across_and_down() {
        let (w, h) = (128, 96);
        let a = vec![0u32; w * h];
        let mut b = a.clone();
        for y in [0, 40, 70] {
            for x in [0, 33, 64] {
                b[y * w + x] = 1;
            }
        }
        b[w + 127] = 1;
        assert_eq!(
            changed_rects(&a, &b, w, h),
            [Rect { x: 0, y: 0, w: 128, h: 32 }, Rect { x: 0, y: 32, w: 96, h: 64 }]
        );
    }

    #[test]
    fn a_full_picture_change_is_one_rect() {
        let (w, h) = (720, 400);
        let a = vec![0u32; w * h];
        let b = vec![1u32; w * h];
        assert_eq!(changed_rects(&a, &b, w, h), [Rect { x: 0, y: 0, w: 720, h: 400 }]);
    }

    #[test]
    fn separate_changes_stay_separate() {
        let (w, h) = (128, 64);
        let a = vec![0u32; w * h];
        let mut b = a.clone();
        b[0] = 1;
        b[40 * w + 100] = 1;
        assert_eq!(
            changed_rects(&a, &b, w, h),
            [Rect { x: 0, y: 0, w: 32, h: 32 }, Rect { x: 96, y: 32, w: 32, h: 32 }]
        );
    }

    #[test]
    fn fit_crops_and_pads() {
        let src: Vec<u32> = (1..=6).collect();
        let mut out = Vec::new();
        fit(&src, 3, 2, 2, 3, &mut out);
        assert_eq!(out, [1, 2, 4, 5, 0, 0]);
    }
}

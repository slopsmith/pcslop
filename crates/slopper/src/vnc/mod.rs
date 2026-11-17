//! A VNC (RFB 3.8) server that shows the guest screen and feeds keyboard and
//! pointer input back into the guest.

mod des;
mod encode;
mod keysym;
mod proto;

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use proto::{ClientMsg, ENC_DESKTOP_SIZE, PixelFormat, Rect};

use crate::{Event, Shared, pixel_to_tablet};

/// Environment variable holding the VNC password. Without it the server
/// asks for no authentication.
pub const PASSWORD_VAR: &str = "SLOPPER_VNC_PASSWORD";

const DEFAULT_SIZE: (u32, u32) = (720, 400);

/// Turns `[ADDR:]PORT` into a socket address to bind. The address defaults to
/// 127.0.0.1; IPv6 addresses go in brackets.
pub fn parse_addr(v: &str) -> Result<String, String> {
    let bad = || format!("--vnc needs [ADDR:]PORT, not {v:?}");
    let (host, port) = v.rsplit_once(':').unwrap_or(("", v));
    port.parse::<u16>().map_err(|_| bad())?;
    if host.contains(':') && !host.starts_with('[') {
        return Err(bad());
    }
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    Ok(format!("{host}:{port}"))
}

/// Accepts VNC clients on `listener` until the process exits.
pub fn serve(listener: TcpListener, tx: Sender<Event>, shared: Arc<Shared>, password: Option<String>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (tx, shared, password) = (tx.clone(), shared.clone(), password.clone());
        std::thread::spawn(move || {
            let peer = conn.peer_addr().map_or_else(|_| "?".into(), |a| a.to_string());
            match client(conn, tx, shared, password) {
                Ok(()) => eprintln!("vnc: {peer} disconnected"),
                Err(e) => eprintln!("vnc: {peer}: {e}"),
            }
        });
    }
}

fn random_challenge() -> io::Result<[u8; 16]> {
    let mut c = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut c)?;
    Ok(c)
}

fn pack_size(w: u32, h: u32) -> u32 {
    (w << 16) | h
}

fn client(mut conn: TcpStream, tx: Sender<Event>, shared: Arc<Shared>, password: Option<String>) -> io::Result<()> {
    conn.set_nodelay(true)?;
    let (w, h) = {
        let f = shared.frame.lock().unwrap();
        if f.width == 0 { DEFAULT_SIZE } else { (f.width, f.height) }
    };
    let pw = password.as_deref().map(str::as_bytes);
    proto::handshake(&mut conn, pw, random_challenge()?, (w as u16, h as u16), "86slopper")?;

    let size = Arc::new(AtomicU32::new(pack_size(w, h)));
    let (msg_tx, msg_rx) = channel();
    let reader = conn.try_clone()?;
    let reader_size = size.clone();
    std::thread::spawn(move || {
        let mut input = Input::new(tx, reader_size);
        let mut r = BufReader::new(reader);
        while let Ok(msg) = proto::read_client_msg(&mut r) {
            match msg {
                ClientMsg::Key { down, keysym } => input.key(down, keysym),
                ClientMsg::Pointer { mask, x, y } => input.pointer(mask, x, y),
                ClientMsg::CutText => {}
                m => {
                    if msg_tx.send(m).is_err() {
                        break;
                    }
                }
            }
        }
        input.release_all();
    });
    let peer = conn.peer_addr().map_or_else(|_| "?".into(), |a| a.to_string());
    let result = Session::new(w, h, size, peer).run(&mut conn, msg_rx, &shared);
    let _ = conn.shutdown(Shutdown::Both);
    result
}

struct Input {
    tx: Sender<Event>,
    /// The client's framebuffer size, `width << 16 | height`.
    size: Arc<AtomicU32>,
    pressed: HashMap<u32, u16>,
    last: Option<(u16, u16, u8)>,
}

impl Input {
    fn new(tx: Sender<Event>, size: Arc<AtomicU32>) -> Input {
        Input { tx, size, pressed: HashMap::new(), last: None }
    }

    fn key(&mut self, down: bool, keysym: u32) {
        let code = if down {
            let Some(code) = keysym::scancode(keysym) else { return };
            self.pressed.insert(keysym, code);
            code
        } else {
            match self.pressed.remove(&keysym).or_else(|| keysym::scancode(keysym)) {
                Some(code) => code,
                None => return,
            }
        };
        let _ = self.tx.send(Event::Key(code, down));
    }

    // RFB button mask: bit 0 left, 1 middle, 2 right; pressing bit 3 or 4 is
    // one wheel step up or down.
    fn pointer(&mut self, mask: u8, x: u16, y: u16) {
        let (lx, ly, lmask) = self.last.unwrap_or((x, y, 0));
        let pressed = mask & !lmask;
        let wheel = (pressed >> 3 & 1) as i32 - (pressed >> 4 & 1) as i32;
        let buttons = (mask & 1) | (mask >> 1 & 1) << 2 | (mask >> 2 & 1) << 1;
        let moved = self.last.is_none() || (lx, ly) != (x, y);
        self.last = Some((x, y, mask));
        if !moved && (mask ^ lmask) & 7 == 0 && wheel == 0 {
            return;
        }
        let size = self.size.load(Ordering::Relaxed);
        let (w, h) = (size >> 16, size & 0xffff);
        let _ = self.tx.send(Event::Pointer {
            x: pixel_to_tablet(x as u32, w),
            y: pixel_to_tablet(y as u32, h),
            dx: x as i32 - lx as i32,
            dy: ly as i32 - y as i32,
            buttons,
            wheel,
        });
    }

    fn release_all(&mut self) {
        for (_, code) in self.pressed.drain() {
            let _ = self.tx.send(Event::Key(code, false));
        }
        if let Some((x, y, mask)) = self.last
            && mask & 7 != 0
        {
            self.pointer(0, x, y);
        }
    }
}

struct Session {
    pf: PixelFormat,
    desktop_size: bool,
    width: u32,
    height: u32,
    /// What the client shows now.
    sent: Vec<u32>,
    frame: Vec<u32>,
    frame_size: (u32, u32),
    frame_seq: u64,
    sent_seq: Option<u64>,
    /// A pending update request, true if the client wants the whole picture.
    request: Option<bool>,
    size: Arc<AtomicU32>,
    view: Vec<u32>,
    encoder: encode::Encoder,
    peer: String,
}

impl Session {
    fn new(width: u32, height: u32, size: Arc<AtomicU32>, peer: String) -> Session {
        Session {
            pf: PixelFormat::NATIVE,
            desktop_size: false,
            width,
            height,
            sent: vec![0; (width * height) as usize],
            frame: Vec::new(),
            frame_size: (0, 0),
            frame_seq: 0,
            sent_seq: None,
            request: None,
            size,
            view: Vec::new(),
            encoder: encode::Encoder::new(),
            peer,
        }
    }

    fn run(&mut self, out: &mut impl Write, msgs: Receiver<ClientMsg>, shared: &Shared) -> io::Result<()> {
        let mut buf = Vec::new();
        loop {
            match msgs.recv_timeout(Duration::from_millis(15)) {
                Ok(m) => self.handle(m)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            while let Ok(m) = msgs.try_recv() {
                self.handle(m)?;
            }
            if shared.done.load(Ordering::Relaxed) {
                return Ok(());
            }
            if self.request.is_none() {
                continue;
            }
            {
                let f = shared.frame.lock().unwrap();
                if f.seq != self.frame_seq && f.width > 0 {
                    self.frame.clone_from(&f.pixels);
                    self.frame_size = (f.width, f.height);
                    self.frame_seq = f.seq;
                }
            }
            buf.clear();
            if self.update(&mut buf) {
                out.write_all(&buf)?;
                out.flush()?;
            }
        }
    }

    fn handle(&mut self, m: ClientMsg) -> io::Result<()> {
        match m {
            ClientMsg::SetPixelFormat(pf) => {
                if !pf.supported() {
                    return Err(io::Error::new(io::ErrorKind::Unsupported, format!("unsupported pixel format {pf:?}")));
                }
                self.pf = pf;
            }
            ClientMsg::SetEncodings(encs) => {
                self.desktop_size = encs.contains(&ENC_DESKTOP_SIZE);
                let before = self.encoder.kind;
                self.encoder.set_encodings(&encs);
                if self.encoder.kind != before || self.sent_seq.is_none() {
                    eprintln!("vnc: {} gets {:?} encoding", self.peer, self.encoder.kind);
                }
            }
            ClientMsg::UpdateRequest { incremental } => {
                self.request = Some(self.request.unwrap_or(false) || !incremental);
            }
            _ => {}
        }
        Ok(())
    }

    fn update(&mut self, out: &mut Vec<u8>) -> bool {
        let Some(mut full) = self.request else { return false };
        if !full && self.sent_seq == Some(self.frame_seq) {
            return false;
        }
        let resize = self.desktop_size && self.frame_size.0 > 0 && self.frame_size != (self.width, self.height);
        if resize {
            (self.width, self.height) = self.frame_size;
            self.size.store(pack_size(self.width, self.height), Ordering::Relaxed);
            self.sent.clear();
            self.sent.resize((self.width * self.height) as usize, 0);
            full = true;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        if self.frame_size.0 > 0 {
            let (fw, fh) = (self.frame_size.0 as usize, self.frame_size.1 as usize);
            if (fw, fh) == (w, h) {
                self.view.clone_from(&self.frame);
            } else {
                // Clients without DesktopSize keep the size they started with.
                proto::fit(&self.frame, fw, fh, w, h, &mut self.view);
            }
        } else {
            self.view.clone_from(&self.sent);
        }
        let rects = self.encoder.split(if full {
            vec![Rect { x: 0, y: 0, w: w as u16, h: h as u16 }]
        } else {
            proto::changed_rects(&self.sent, &self.view, w, h)
        });
        self.sent_seq = Some(self.frame_seq);
        if rects.is_empty() {
            return false;
        }
        proto::update_header(out, rects.len() as u16 + resize as u16);
        if resize {
            proto::rect_header(out, Rect { x: 0, y: 0, w: w as u16, h: h as u16 }, ENC_DESKTOP_SIZE);
        }
        for r in rects {
            self.encoder.rect(out, &self.pf, &self.view, w, r);
        }
        std::mem::swap(&mut self.sent, &mut self.view);
        self.request = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Frame;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    #[test]
    fn parses_listen_addresses() {
        assert_eq!(parse_addr("5900").unwrap(), "127.0.0.1:5900");
        assert_eq!(parse_addr(":5901").unwrap(), "127.0.0.1:5901");
        assert_eq!(parse_addr("0.0.0.0:5900").unwrap(), "0.0.0.0:5900");
        assert_eq!(parse_addr("[::1]:5900").unwrap(), "[::1]:5900");
        assert_eq!(parse_addr("localhost:5900").unwrap(), "localhost:5900");
        assert!(parse_addr("::1:5900").is_err());
        assert!(parse_addr("host").is_err());
        assert!(parse_addr("70000").is_err());
    }

    fn input() -> (Input, Receiver<Event>) {
        let (tx, rx) = channel();
        (Input::new(tx, Arc::new(AtomicU32::new(pack_size(800, 600)))), rx)
    }

    fn pointers(rx: &Receiver<Event>) -> Vec<(u16, u16, i32, i32, u8, i32)> {
        rx.try_iter()
            .filter_map(|e| match e {
                Event::Pointer { x, y, dx, dy, buttons, wheel } => Some((x, y, dx, dy, buttons, wheel)),
                _ => None,
            })
            .collect()
    }

    fn keys(rx: &Receiver<Event>) -> Vec<(u16, bool)> {
        rx.try_iter()
            .filter_map(|e| match e {
                Event::Key(c, d) => Some((c, d)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pointer_positions_become_tablet_coordinates() {
        let (mut i, rx) = input();
        i.pointer(0, 400, 300);
        i.pointer(0, 410, 290);
        let p = pointers(&rx);
        assert_eq!(p[0].0, pixel_to_tablet(400, 800));
        assert_eq!(p[0].1, pixel_to_tablet(300, 600));
        assert_eq!((p[1].2, p[1].3), (10, 10));
    }

    #[test]
    fn vnc_buttons_map_to_left_right_middle() {
        let (mut i, rx) = input();
        i.pointer(1, 0, 0);
        i.pointer(2, 0, 0);
        i.pointer(4, 0, 0);
        let b: Vec<u8> = pointers(&rx).iter().map(|p| p.4).collect();
        assert_eq!(b, [1, 4, 2]);
    }

    #[test]
    fn wheel_presses_become_single_steps() {
        let (mut i, rx) = input();
        i.pointer(0, 5, 5);
        i.pointer(8, 5, 5);
        i.pointer(0, 5, 5);
        i.pointer(16, 5, 5);
        i.pointer(0, 5, 5);
        let w: Vec<i32> = pointers(&rx).iter().map(|p| p.5).filter(|&w| w != 0).collect();
        assert_eq!(w, [1, -1]);
    }

    #[test]
    fn repeated_identical_pointer_events_are_dropped() {
        let (mut i, rx) = input();
        i.pointer(0, 5, 5);
        i.pointer(0, 5, 5);
        assert_eq!(pointers(&rx).len(), 1);
    }

    #[test]
    fn key_release_matches_the_key_that_was_pressed() {
        let (mut i, rx) = input();
        i.key(true, 'A' as u32);
        i.key(false, 'a' as u32);
        assert_eq!(keys(&rx), [(0x1e, true), (0x1e, false)]);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let (mut i, rx) = input();
        i.key(true, 0x1234567);
        i.key(false, 0x1234567);
        assert!(keys(&rx).is_empty());
    }

    #[test]
    fn disconnecting_releases_held_keys_and_buttons() {
        let (mut i, rx) = input();
        i.key(true, 0xffe3);
        i.pointer(1, 10, 10);
        let _ = rx.try_iter().count();
        i.release_all();
        let evs: Vec<Event> = rx.try_iter().collect();
        assert!(matches!(evs[0], Event::Key(0x1d, false)));
        assert!(matches!(evs[1], Event::Pointer { buttons: 0, .. }));
    }

    fn shared_with(pixels: Vec<u32>, w: u32, h: u32) -> Arc<Shared> {
        Arc::new(Shared {
            frame: Mutex::new(Frame { pixels, width: w, height: h, seq: 1 }),
            done: AtomicBool::new(false),
            instructions: AtomicU64::new(0),
        })
    }

    fn read_n<const N: usize>(s: &mut impl Read) -> [u8; N] {
        let mut b = [0; N];
        s.read_exact(&mut b).unwrap();
        b
    }

    fn read_update(s: &mut impl Read) -> Vec<(Rect, i32, Vec<u32>)> {
        let head = read_n::<4>(s);
        assert_eq!(head[0], 0);
        let n = u16::from_be_bytes([head[2], head[3]]);
        (0..n)
            .map(|_| {
                let r = read_n::<12>(s);
                let v = |i: usize| u16::from_be_bytes([r[i], r[i + 1]]);
                let rect = Rect { x: v(0), y: v(2), w: v(4), h: v(6) };
                let enc = i32::from_be_bytes(r[8..12].try_into().unwrap());
                let mut px = Vec::new();
                if enc == proto::ENC_RAW {
                    let mut data = vec![0; rect.w as usize * rect.h as usize * 4];
                    s.read_exact(&mut data).unwrap();
                    px = data.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
                }
                (rect, enc, px)
            })
            .collect()
    }

    fn connect(shared: &Arc<Shared>) -> (TcpStream, Receiver<Event>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = channel();
        let shared = shared.clone();
        std::thread::spawn(move || serve(listener, tx, shared, None));
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        read_n::<12>(&mut s);
        s.write_all(b"RFB 003.008\n").unwrap();
        read_n::<2>(&mut s);
        s.write_all(&[1]).unwrap();
        read_n::<4>(&mut s);
        s.write_all(&[1]).unwrap();
        let init = read_n::<24>(&mut s);
        let name_len = u32::from_be_bytes(init[20..24].try_into().unwrap()) as usize;
        s.read_exact(&mut vec![0; name_len]).unwrap();
        (s, rx)
    }

    fn set_encodings(s: &mut TcpStream, encs: &[i32]) {
        let mut m = vec![2, 0];
        m.extend((encs.len() as u16).to_be_bytes());
        for e in encs {
            m.extend(e.to_be_bytes());
        }
        s.write_all(&m).unwrap();
    }

    fn request(s: &mut TcpStream, incremental: bool, w: u16, h: u16) {
        let mut m = vec![3, incremental as u8, 0, 0, 0, 0];
        m.extend(w.to_be_bytes());
        m.extend(h.to_be_bytes());
        s.write_all(&m).unwrap();
    }

    fn set_frame(shared: &Shared, pixels: Vec<u32>, w: u32, h: u32) {
        let mut f = shared.frame.lock().unwrap();
        *f = Frame { pixels, width: w, height: h, seq: f.seq + 1 };
    }

    #[test]
    fn serves_the_picture_then_only_changed_tiles() {
        let pixels: Vec<u32> = (0..64 * 48).collect();
        let shared = shared_with(pixels.clone(), 64, 48);
        let (mut s, _rx) = connect(&shared);
        set_encodings(&mut s, &[0, ENC_DESKTOP_SIZE]);
        request(&mut s, false, 64, 48);
        let u = read_update(&mut s);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].0, Rect { x: 0, y: 0, w: 64, h: 48 });
        assert_eq!(u[0].2, pixels);

        let mut changed = pixels.clone();
        changed[40 * 64 + 50] = 0xffffff;
        set_frame(&shared, changed, 64, 48);
        request(&mut s, true, 64, 48);
        let u = read_update(&mut s);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].0, Rect { x: 32, y: 32, w: 32, h: 16 });
        assert_eq!(u[0].2[8 * 32 + 18], 0xffffff);
    }

    #[test]
    fn tells_the_client_about_a_new_resolution() {
        let shared = shared_with(vec![1; 64 * 48], 64, 48);
        let (mut s, _rx) = connect(&shared);
        set_encodings(&mut s, &[0, ENC_DESKTOP_SIZE]);
        request(&mut s, false, 64, 48);
        read_update(&mut s);
        set_frame(&shared, vec![2; 80 * 60], 80, 60);
        request(&mut s, true, 64, 48);
        let u = read_update(&mut s);
        assert_eq!(u[0].0, Rect { x: 0, y: 0, w: 80, h: 60 });
        assert_eq!(u[0].1, ENC_DESKTOP_SIZE);
        assert_eq!(u[1].0, Rect { x: 0, y: 0, w: 80, h: 60 });
        assert!(u[1].2.iter().all(|&p| p == 2));
    }

    #[test]
    fn crops_a_new_resolution_for_clients_that_cannot_resize() {
        let shared = shared_with(vec![1; 64 * 48], 64, 48);
        let (mut s, _rx) = connect(&shared);
        set_encodings(&mut s, &[0]);
        request(&mut s, false, 64, 48);
        read_update(&mut s);
        set_frame(&shared, vec![2; 80 * 60], 80, 60);
        request(&mut s, true, 64, 48);
        let u = read_update(&mut s);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].0, Rect { x: 0, y: 0, w: 64, h: 48 });
        assert!(u[0].2.iter().all(|&p| p == 2));
    }

    #[test]
    fn forwards_keys_from_the_client() {
        let shared = shared_with(vec![0; 64 * 48], 64, 48);
        let (mut s, rx) = connect(&shared);
        s.write_all(&[4, 1, 0, 0, 0, 0, 0xff, 0x0d]).unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(ev, Event::Key(0x1c, true)));
    }
}

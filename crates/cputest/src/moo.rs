//! Reader for the MOO test format, written from the format specification in
//! <https://github.com/dbalsom/moo/blob/main/doc/moo_format_v1.md>.


pub const REG_NAMES: [&str; 20] = [
    "cr0", "cr3", "eax", "ebx", "ecx", "edx", "esi", "edi", "ebp", "esp", "cs", "ds", "es", "fs", "gs", "ss", "eip",
    "eflags", "dr6", "dr7",
];

pub const R_CR0: usize = 0;
pub const R_CR3: usize = 1;
pub const R_EAX: usize = 2;
pub const R_CS: usize = 10;
pub const R_EIP: usize = 16;
pub const R_EFLAGS: usize = 17;
pub const R_DR6: usize = 18;
pub const R_DR7: usize = 19;

/// A register file where `None` means "not present in this snapshot".
pub type Regs = [Option<u32>; 20];

#[derive(Debug, Default)]
pub struct State {
    pub regs: Regs,
    pub masks: Regs,
    pub ram: Vec<(u32, u8)>,
}

#[derive(Debug, Default)]
pub struct Test {
    pub index: u32,
    pub name: String,
    pub bytes: Vec<u8>,
    pub initial: State,
    pub fin: State,
    pub exception: Option<(u8, u32)>,
    pub hash: [u8; 20],
}

pub struct File {
    pub masks: Regs,
    pub tests: Vec<Test>,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> u8 {
        let v = self.data[self.pos];
        self.pos += 1;
        v
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        s
    }

    fn chunks(data: &'a [u8]) -> impl Iterator<Item = ([u8; 4], &'a [u8])> {
        let mut r = Reader { data, pos: 0 };
        std::iter::from_fn(move || {
            if r.pos + 8 > r.data.len() {
                return None;
            }
            let id: [u8; 4] = r.bytes(4).try_into().unwrap();
            let len = r.u32() as usize;
            Some((id, r.bytes(len)))
        })
    }
}

fn regs32(data: &[u8]) -> Regs {
    let mut r = Reader { data, pos: 0 };
    let mask = r.u32();
    let mut out = [None; 20];
    for (i, slot) in out.iter_mut().enumerate() {
        if mask & (1 << i) != 0 {
            *slot = Some(r.u32());
        }
    }
    out
}

fn regs16(data: &[u8]) -> Regs {
    const MAP: [usize; 14] = [2, 3, 4, 5, 10, 15, 11, 12, 9, 8, 6, 7, 16, 17];
    let mut r = Reader { data, pos: 0 };
    let mask = r.u16();
    let mut out = [None; 20];
    for (i, &slot) in MAP.iter().enumerate() {
        if mask & (1 << i) != 0 {
            out[slot] = Some(r.u16() as u32);
        }
    }
    out
}

fn state(data: &[u8]) -> State {
    let mut s = State::default();
    for (id, body) in Reader::chunks(data) {
        match &id {
            b"RG32" => s.regs = regs32(body),
            b"REGS" => s.regs = regs16(body),
            b"RM32" => s.masks = regs32(body),
            b"RMSK" => s.masks = regs16(body),
            b"RAM " => {
                let mut r = Reader { data: body, pos: 0 };
                let n = r.u32();
                s.ram = (0..n).map(|_| (r.u32(), r.u8())).collect();
            }
            _ => {}
        }
    }
    s
}

pub fn parse(data: &[u8]) -> Result<File, String> {
    let mut chunks = Reader::chunks(data);
    match chunks.next() {
        Some((id, _)) if &id == b"MOO " => {}
        _ => return Err("missing MOO header".into()),
    }
    let mut file = File { masks: [None; 20], tests: Vec::new() };
    for (id, body) in chunks {
        match &id {
            b"RM32" => file.masks = regs32(body),
            b"RMSK" => file.masks = regs16(body),
            b"TEST" => {
                let mut t = Test { index: u32::from_le_bytes(body[..4].try_into().unwrap()), ..Default::default() };
                for (sid, sbody) in Reader::chunks(&body[4..]) {
                    let mut r = Reader { data: sbody, pos: 0 };
                    match &sid {
                        b"NAME" => {
                            let n = r.u32() as usize;
                            t.name = String::from_utf8_lossy(r.bytes(n)).into_owned();
                        }
                        b"BYTS" => {
                            let n = r.u32() as usize;
                            t.bytes = r.bytes(n).to_vec();
                        }
                        b"INIT" => t.initial = state(sbody),
                        b"FINA" => t.fin = state(sbody),
                        b"EXCP" => t.exception = Some((r.u8(), r.u32())),
                        b"HASH" => t.hash = sbody[..20].try_into().unwrap(),
                        _ => {}
                    }
                }
                file.tests.push(t);
            }
            _ => {}
        }
    }
    Ok(file)
}

pub fn hash_hex(h: &[u8; 20]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}


//! Device state for save states: a little-endian binary encoding that loads
//! back into existing objects, so host resources a device holds (disk files,
//! audio outputs, network backends) stay where they are.
//!
//! Every type also has a `SCHEMA` hash of its saved fields, their names and
//! types. A snapshot records the schema of what it holds and a build whose
//! devices changed shape refuses it instead of misreading it.

use std::collections::VecDeque;

extern crate self as snapshot;

pub use snapshot_derive::Snap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub trait Snap {
    const SCHEMA: u64;
    fn save(&self, w: &mut Writer);
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()>;
}

/// FNV-1a, 64-bit.
pub const fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
        i += 1;
    }
    h
}

pub const fn mix(seed: u64, parts: &[u64]) -> u64 {
    let mut h = seed;
    let mut i = 0;
    while i < parts.len() {
        h = (h ^ parts[i]).wrapping_mul(0x100_0000_01b3).rotate_left(29);
        i += 1;
    }
    h
}

/// A fresh `T` with its saved fields read from `r`.
pub fn read_new<T: Snap + Default>(r: &mut Reader<'_>) -> Result<T> {
    let mut v = T::default();
    v.load(r)?;
    Ok(v)
}

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    /// A length-prefixed byte string.
    pub fn blob(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.bytes(b);
    }

    pub fn put<T: Snap + ?Sized>(&mut self, v: &T) {
        v.save(self);
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or_else(|| Error::new("snapshot data ends early"))?;
        let b = &self.buf[self.pos..end];
        self.pos = end;
        Ok(b)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.bytes(N)?.try_into().unwrap())
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn len(&mut self) -> Result<usize> {
        let n = self.u64()?;
        usize::try_from(n).ok().filter(|&n| n <= self.remaining()).ok_or_else(|| Error::new(format!("bad length {n} in snapshot")))
    }

    pub fn blob(&mut self) -> Result<&'a [u8]> {
        let n = self.len()?;
        self.bytes(n)
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn get<T: Snap + ?Sized>(&mut self, v: &mut T) -> Result<()> {
        v.load(self)
    }
}

macro_rules! int {
    ($($t:ty),*) => {$(
        impl Snap for $t {
            const SCHEMA: u64 = fnv(stringify!($t).as_bytes());
            fn save(&self, w: &mut Writer) {
                w.bytes(&self.to_le_bytes());
            }
            fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
                *self = <$t>::from_le_bytes(r.array()?);
                Ok(())
            }
        }
    )*};
}

int!(u8, u16, u32, u64, u128, i8, i16, i32, i64);

impl Snap for usize {
    const SCHEMA: u64 = fnv(b"usize");
    fn save(&self, w: &mut Writer) {
        w.u64(*self as u64);
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        let v = r.u64()?;
        *self = usize::try_from(v).map_err(|_| Error::new("usize out of range"))?;
        Ok(())
    }
}

impl Snap for bool {
    const SCHEMA: u64 = fnv(b"bool");
    fn save(&self, w: &mut Writer) {
        w.bytes(&[*self as u8]);
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        *self = match r.array::<1>()?[0] {
            0 => false,
            1 => true,
            v => return Err(Error::new(format!("bad bool {v}"))),
        };
        Ok(())
    }
}

impl Snap for String {
    const SCHEMA: u64 = fnv(b"String");
    fn save(&self, w: &mut Writer) {
        w.blob(self.as_bytes());
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        *self = std::str::from_utf8(r.blob()?).map_err(|_| Error::new("bad string"))?.to_string();
        Ok(())
    }
}

impl<T: Snap, const N: usize> Snap for [T; N] {
    const SCHEMA: u64 = mix(fnv(b"array"), &[T::SCHEMA, N as u64]);
    fn save(&self, w: &mut Writer) {
        self.iter().for_each(|v| v.save(w));
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.iter_mut().try_for_each(|v| v.load(r))
    }
}

impl<T: Snap + Default> Snap for Vec<T> {
    const SCHEMA: u64 = mix(fnv(b"Vec"), &[T::SCHEMA]);
    fn save(&self, w: &mut Writer) {
        w.u64(self.len() as u64);
        self.iter().for_each(|v| v.save(w));
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        let n = r.len()?;
        self.clear();
        for _ in 0..n {
            self.push(read_new(r)?);
        }
        Ok(())
    }
}

impl<T: Snap + Default> Snap for VecDeque<T> {
    const SCHEMA: u64 = mix(fnv(b"VecDeque"), &[T::SCHEMA]);
    fn save(&self, w: &mut Writer) {
        w.u64(self.len() as u64);
        self.iter().for_each(|v| v.save(w));
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        let n = r.len()?;
        self.clear();
        for _ in 0..n {
            self.push_back(read_new(r)?);
        }
        Ok(())
    }
}

impl<T: Snap + Default> Snap for Option<T> {
    const SCHEMA: u64 = mix(fnv(b"Option"), &[T::SCHEMA]);
    fn save(&self, w: &mut Writer) {
        match self {
            None => w.bytes(&[0]),
            Some(v) => {
                w.bytes(&[1]);
                v.save(w);
            }
        }
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        let mut some = false;
        some.load(r)?;
        match (some, self.as_mut()) {
            (false, _) => *self = None,
            (true, Some(v)) => v.load(r)?,
            (true, None) => *self = Some(read_new(r)?),
        }
        Ok(())
    }
}

impl<T: Snap + ?Sized> Snap for Box<T> {
    const SCHEMA: u64 = T::SCHEMA;
    fn save(&self, w: &mut Writer) {
        (**self).save(w);
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        (**self).load(r)
    }
}

impl<A: Snap, B: Snap> Snap for (A, B) {
    const SCHEMA: u64 = mix(fnv(b"pair"), &[A::SCHEMA, B::SCHEMA]);
    fn save(&self, w: &mut Writer) {
        self.0.save(w);
        self.1.save(w);
    }
    fn load(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.0.load(r)?;
        self.1.load(r)
    }
}

/// Saves `v` and loads the result into a fresh `T::default()`.
pub fn round_trip<T: Snap + Default>(v: &T) -> T {
    let mut w = Writer::new();
    v.save(&mut w);
    let mut r = Reader::new(&w.buf);
    let out = read_new(&mut r).expect("round trip");
    assert!(r.is_empty(), "load left {} bytes unread", r.remaining());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Snap, Default, Debug, PartialEq)]
    struct Inner {
        a: u8,
        b: Option<u64>,
    }

    #[derive(Snap, Default, Debug, PartialEq)]
    enum Mode {
        #[default]
        Off,
        Count(u32, Vec<u8>),
        Named {
            x: i16,
            #[snap(skip)]
            host: u8,
        },
    }

    #[derive(Snap, Default, Debug, PartialEq)]
    struct Outer {
        inner: Inner,
        q: VecDeque<u16>,
        mode: Mode,
        regs: [u32; 3],
        #[snap(skip)]
        host: u32,
        pair: (bool, String),
    }

    #[test]
    fn a_struct_comes_back_equal_except_skipped_fields() {
        let v = Outer {
            inner: Inner { a: 7, b: Some(1 << 40) },
            q: [1, 2, 3].into(),
            mode: Mode::Count(9, vec![4, 5]),
            regs: [10, 20, 30],
            host: 99,
            pair: (true, "hi".into()),
        };
        let back = round_trip(&v);
        assert_eq!(back, Outer { host: 0, ..v });
    }

    #[test]
    fn load_keeps_skipped_fields_of_the_target() {
        let mut w = Writer::new();
        Outer { host: 1, ..Default::default() }.save(&mut w);
        let mut target = Outer { host: 42, ..Default::default() };
        target.load(&mut Reader::new(&w.buf)).unwrap();
        assert_eq!(target.host, 42);
    }

    #[test]
    fn an_enum_variant_with_skipped_fields_gets_their_default() {
        let back = round_trip(&Mode::Named { x: -3, host: 5 });
        assert_eq!(back, Mode::Named { x: -3, host: 0 });
    }

    #[test]
    fn truncated_data_is_an_error_not_a_panic() {
        let mut w = Writer::new();
        Outer::default().save(&mut w);
        w.buf.pop();
        assert!(Outer::default().load(&mut Reader::new(&w.buf)).is_err());
    }

    #[test]
    fn schema_changes_with_field_types_and_names() {
        #[derive(Snap)]
        struct A {
            x: u8,
        }
        mod other {
            #[derive(super::Snap)]
            pub struct A {
                pub x: u16,
            }
        }
        #[derive(Snap)]
        struct B {
            y: u8,
        }
        assert_ne!(A::SCHEMA, other::A::SCHEMA);
        assert_ne!(A::SCHEMA, B::SCHEMA);
        let _ = (A { x: 0 }.x, B { y: 0 }.y, other::A { x: 0 }.x);
    }

    #[test]
    fn schema_follows_nested_types() {
        assert_ne!(<Vec<Inner>>::SCHEMA, <Vec<u8>>::SCHEMA);
    }
}

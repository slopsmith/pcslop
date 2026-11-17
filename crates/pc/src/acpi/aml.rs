//! A small ACPI Machine Language encoder (ACPI 1.0b, section 16). Each
//! function returns the bytes of one term; terms nest by passing their bytes
//! as the body of another. The API loosely follows QEMU's
//! `hw/acpi/aml-build.c` (Igor Mammedov and the QEMU authors).

pub type Aml = Vec<u8>;

const EXT_OP: u8 = 0x5b;

/// Encodes a name path such as `\_SB.PCI0`, `^ISA` or `_HID`.
pub fn name_string(path: &str) -> Aml {
    let mut out = Vec::new();
    let mut rest = path;
    while let Some(r) = rest.strip_prefix('\\').or_else(|| rest.strip_prefix('^')) {
        out.push(rest.as_bytes()[0]);
        rest = r;
    }
    let segs: Vec<&str> = if rest.is_empty() { Vec::new() } else { rest.split('.').collect() };
    match segs.len() {
        0 => out.push(0),
        1 => {}
        2 => out.push(0x2e),
        n => out.extend([0x2f, n as u8]),
    }
    for seg in segs {
        out.extend(name_seg(seg));
    }
    out
}

fn name_seg(seg: &str) -> [u8; 4] {
    assert!(
        (1..=4).contains(&seg.len())
            && seg.bytes().enumerate().all(|(i, c)| c == b'_' || c.is_ascii_uppercase() || (i > 0 && c.is_ascii_digit())),
        "bad ACPI name segment {seg:?}"
    );
    let mut s = *b"____";
    s[..seg.len()].copy_from_slice(seg.as_bytes());
    s
}

/// PkgLength for a body of `len` bytes, counting the encoding's own bytes.
pub fn pkg_length(len: usize) -> Aml {
    let n = match len {
        _ if len + 1 < 1 << 6 => 1,
        _ if len + 2 < 1 << 12 => 2,
        _ if len + 3 < 1 << 20 => 3,
        _ => 4,
    };
    encode_pkg_length(len + n, n)
}

fn encode_pkg_length(total: usize, n: usize) -> Aml {
    assert!(total < 1 << 28);
    if n == 1 {
        return vec![total as u8];
    }
    let mut out = vec![((n - 1) << 6) as u8 | (total & 0xf) as u8];
    out.extend((0..n - 1).map(|i| (total >> (4 + 8 * i)) as u8));
    out
}

fn package_op(op: &[u8], body: Aml) -> Aml {
    let mut out = op.to_vec();
    out.extend(pkg_length(body.len()));
    out.extend(body);
    out
}

fn concat(parts: impl IntoIterator<Item = Aml>) -> Aml {
    parts.into_iter().flatten().collect()
}

pub fn int(v: u64) -> Aml {
    match v {
        0 => vec![0x00],
        1 => vec![0x01],
        2..=0xff => vec![0x0a, v as u8],
        0x100..=0xffff => concat([vec![0x0b], (v as u16).to_le_bytes().to_vec()]),
        0x1_0000..=0xffff_ffff => concat([vec![0x0c], (v as u32).to_le_bytes().to_vec()]),
        _ => concat([vec![0x0e], v.to_le_bytes().to_vec()]),
    }
}

pub fn string(s: &str) -> Aml {
    assert!(s.is_ascii() && !s.contains('\0'));
    concat([vec![0x0d], s.as_bytes().to_vec(), vec![0]])
}

/// The 32-bit compressed EISA ID of a PNP ID such as `PNP0A03`.
pub fn eisa_id(id: &str) -> u32 {
    let b = id.as_bytes();
    assert!(b.len() == 7 && b[..3].iter().all(u8::is_ascii_uppercase), "bad EISA ID {id:?}");
    let hex = |c: u8| (c as char).to_digit(16).expect("bad EISA ID");
    let v = ((b[0] - 0x40) as u32) << 26
        | ((b[1] - 0x40) as u32) << 21
        | ((b[2] - 0x40) as u32) << 16
        | hex(b[3]) << 12
        | hex(b[4]) << 8
        | hex(b[5]) << 4
        | hex(b[6]);
    v.swap_bytes()
}

pub fn eisaid(id: &str) -> Aml {
    int(eisa_id(id) as u64)
}

pub fn name(path: &str, value: Aml) -> Aml {
    concat([vec![0x08], name_string(path), value])
}

pub fn scope(path: &str, body: impl IntoIterator<Item = Aml>) -> Aml {
    package_op(&[0x10], concat([name_string(path), concat(body)]))
}

pub fn device(path: &str, body: impl IntoIterator<Item = Aml>) -> Aml {
    package_op(&[EXT_OP, 0x82], concat([name_string(path), concat(body)]))
}

/// An ACPI 1.0 Processor object; a `pblk` of 0 means no P_BLK registers.
pub fn processor(path: &str, id: u8, pblk: u32, pblk_len: u8, body: impl IntoIterator<Item = Aml>) -> Aml {
    package_op(&[EXT_OP, 0x83], concat([name_string(path), vec![id], pblk.to_le_bytes().to_vec(), vec![pblk_len], concat(body)]))
}

pub fn method(path: &str, args: u8, serialized: bool, body: impl IntoIterator<Item = Aml>) -> Aml {
    assert!(args <= 7);
    let flags = args | (serialized as u8) << 3;
    package_op(&[0x14], concat([name_string(path), vec![flags], concat(body)]))
}

pub fn package(elements: impl IntoIterator<Item = Aml>) -> Aml {
    let elements: Vec<Aml> = elements.into_iter().collect();
    assert!(elements.len() < 256);
    package_op(&[0x12], concat([vec![elements.len() as u8], concat(elements)]))
}

pub fn buffer(bytes: &[u8]) -> Aml {
    package_op(&[0x11], concat([int(bytes.len() as u64), bytes.to_vec()]))
}

pub fn ret(v: Aml) -> Aml {
    concat([vec![0xa4], v])
}

pub fn if_(predicate: Aml, body: impl IntoIterator<Item = Aml>) -> Aml {
    package_op(&[0xa0], concat([predicate, concat(body)]))
}

pub fn arg(n: u8) -> Aml {
    assert!(n <= 6);
    vec![0x68 + n]
}

pub fn local(n: u8) -> Aml {
    assert!(n <= 7);
    vec![0x60 + n]
}

/// A reference to a named object, or a call when followed by its arguments.
pub fn path(p: &str) -> Aml {
    name_string(p)
}

pub fn call(method: &str, args: impl IntoIterator<Item = Aml>) -> Aml {
    concat([name_string(method), concat(args)])
}

pub fn store(src: Aml, dst: Aml) -> Aml {
    concat([vec![0x70], src, dst])
}

/// `And(a, b)` with no target.
pub fn and(a: Aml, b: Aml) -> Aml {
    concat([vec![0x7b], a, b, vec![0]])
}

pub fn or(a: Aml, b: Aml, target: Aml) -> Aml {
    concat([vec![0x7d], a, b, target])
}

pub fn lless(a: Aml, b: Aml) -> Aml {
    concat([vec![0x95], a, b])
}

/// `Index(source, index)` with no target: a reference to one element, which
/// `store` can write through.
pub fn index(source: Aml, index: Aml) -> Aml {
    concat([vec![0x88], source, index, vec![0]])
}

pub fn notify(object: Aml, value: Aml) -> Aml {
    concat([vec![0x86], object, value])
}

pub fn create_dword_field(buffer: Aml, byte_index: Aml, field: &str) -> Aml {
    concat([vec![0x8a], buffer, byte_index, name_string(field)])
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum RegionSpace {
    SystemIo = 1,
    PciConfig = 2,
}

pub fn op_region(path: &str, space: RegionSpace, offset: u64, len: u64) -> Aml {
    concat([vec![EXT_OP, 0x80], name_string(path), vec![space as u8], int(offset), int(len)])
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum Access {
    Byte = 1,
    DWord = 3,
}

/// A `Field` with no lock and the Preserve update rule, listing (name, width
/// in bits) in order from offset 0.
pub fn field(region: &str, access: Access, fields: &[(&str, usize)]) -> Aml {
    let mut body = name_string(region);
    body.push(access as u8);
    for &(n, bits) in fields {
        body.extend(name_seg(n));
        let n = [1 << 6, 1 << 12, 1 << 20].iter().take_while(|&&limit| bits >= limit).count() + 1;
        body.extend(encode_pkg_length(bits, n));
    }
    package_op(&[EXT_OP, 0x81], body)
}

/// Resource descriptors for a `ResourceTemplate` (ACPI 1.0b, section 6.4).
pub mod res {
    use super::Aml;

    pub fn io(base: u16, len: u8) -> Aml {
        let [lo, hi] = base.to_le_bytes();
        vec![0x47, 0x01, lo, hi, lo, hi, 0x01, len]
    }

    pub fn irq_no_flags(irq: u8) -> Aml {
        let [lo, hi] = (1u16 << irq).to_le_bytes();
        vec![0x22, lo, hi]
    }

    /// A compatibility-timing, 8-bit, non-bus-master DMA channel.
    pub fn dma(channel: u8) -> Aml {
        vec![0x2a, 1 << channel, 0x00]
    }

    const MIN_MAX_FIXED: u8 = 0x0c;

    fn word_space(kind: u8, type_flags: u8, min: u16, max: u16) -> Aml {
        let mut out = vec![0x88, 0x0d, 0x00, kind, MIN_MAX_FIXED, type_flags];
        for v in [0, min, max, 0, max.wrapping_sub(min).wrapping_add(1)] {
            out.extend(v.to_le_bytes());
        }
        out
    }

    pub fn word_bus_number(min: u16, max: u16) -> Aml {
        word_space(2, 0, min, max)
    }

    pub fn word_io(min: u16, max: u16) -> Aml {
        word_space(1, 0x03, min, max)
    }

    /// Cacheable, read-write memory.
    pub fn dword_memory(min: u32, max: u32) -> Aml {
        let mut out = vec![0x87, 0x17, 0x00, 0, MIN_MAX_FIXED, 0x03];
        for v in [0, min, max, 0, max - min + 1] {
            out.extend(v.to_le_bytes());
        }
        out
    }

    /// An Extended Interrupt descriptor for level-triggered, active-high,
    /// shared interrupts, as QEMU declares for its PCI links.
    pub fn ext_irq(irqs: &[u32]) -> Aml {
        let len = 2 + 4 * irqs.len() as u16;
        let mut out = vec![0x89];
        out.extend(len.to_le_bytes());
        out.extend([0x09, irqs.len() as u8]);
        for irq in irqs {
            out.extend(irq.to_le_bytes());
        }
        out
    }

    pub fn template(items: impl IntoIterator<Item = Aml>) -> Aml {
        let mut out: Aml = items.into_iter().flatten().collect();
        out.extend([0x79, 0x00]);
        super::buffer(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_strings_pad_segments_and_use_prefixes() {
        assert_eq!(name_string("_S5"), b"_S5_");
        assert_eq!(name_string("\\_SB"), b"\\_SB_");
        assert_eq!(name_string("^ISA"), b"^ISA_");
        assert_eq!(name_string("\\_SB.PCI0"), b"\\\x2e_SB_PCI0");
        assert_eq!(name_string("PCI0.ISA.P40C"), b"\x2f\x03PCI0ISA_P40C");
        assert_eq!(name_string("\\"), b"\\\x00");
    }

    #[test]
    #[should_panic]
    fn name_segments_longer_than_four_are_rejected() {
        name_string("TOOLONG");
    }

    #[test]
    fn package_lengths_grow_at_the_spec_boundaries() {
        assert_eq!(pkg_length(0), [0x01]);
        assert_eq!(pkg_length(62), [0x3f]);
        assert_eq!(pkg_length(63), [0x41, 0x04]);
        assert_eq!(pkg_length(0xffd), [0x4f, 0xff]);
        assert_eq!(pkg_length(0xffe), [0x81, 0x00, 0x01]);
        assert_eq!(pkg_length(0x10_0000), [0xc4, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn integers_use_the_shortest_encoding() {
        assert_eq!(int(0), [0x00]);
        assert_eq!(int(1), [0x01]);
        assert_eq!(int(0x80), [0x0a, 0x80]);
        assert_eq!(int(0x1234), [0x0b, 0x34, 0x12]);
        assert_eq!(int(0xffff_ffff), [0x0c, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(int(1 << 32), [0x0e, 0, 0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn eisa_ids_match_iasl() {
        assert_eq!(eisaid("PNP0A03"), [0x0c, 0x41, 0xd0, 0x0a, 0x03]);
        assert_eq!(eisaid("PNP0303"), [0x0c, 0x41, 0xd0, 0x03, 0x03]);
        assert_eq!(eisaid("PNP0C0C"), [0x0c, 0x41, 0xd0, 0x0c, 0x0c]);
    }

    #[test]
    fn device_wraps_its_body_in_a_package() {
        let d = device("KBD", [name("_HID", eisaid("PNP0303"))]);
        assert_eq!(&d[..2], [0x5b, 0x82]);
        assert_eq!(d[2] as usize, d.len() - 2);
        assert_eq!(&d[3..7], b"KBD_");
        assert_eq!(&d[7..12], b"\x08_HID");
    }

    #[test]
    fn processor_matches_iasl() {
        assert_eq!(processor("CPU0", 0, 0, 0, []), [0x5b, 0x83, 0x0b, b'C', b'P', b'U', b'0', 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn method_flags_carry_args_and_serialization() {
        assert_eq!(method("_SRS", 1, false, []), [0x14, 0x06, b'_', b'S', b'R', b'S', 0x01]);
        assert_eq!(method("IQCR", 1, true, [])[6], 0x09);
    }

    #[test]
    fn s5_package_matches_iasl() {
        let s5 = name("\\_S5", package([int(0), int(0), int(0), int(0)]));
        assert_eq!(s5, [0x08, b'\\', b'_', b'S', b'5', b'_', 0x12, 0x06, 0x04, 0, 0, 0, 0]);
    }

    #[test]
    fn fields_encode_bit_widths_without_counting_themselves() {
        let f = field("P40C", Access::Byte, &[("PRQ0", 8), ("PRQ1", 8)]);
        assert_eq!(f, [0x5b, 0x81, 0x10, b'P', b'4', b'0', b'C', 0x01, b'P', b'R', b'Q', b'0', 8, b'P', b'R', b'Q', b'1', 8]);
        let wide = field("R", Access::Byte, &[("BIG", 64)]);
        assert_eq!(&wide[wide.len() - 2..], [0x40, 0x04]);
    }

    #[test]
    fn dword_field_over_an_io_region_matches_iasl() {
        let region = op_region("BATR", RegionSpace::SystemIo, 0x640, 0x30);
        assert_eq!(region, [0x5b, 0x80, b'B', b'A', b'T', b'R', 0x01, 0x0b, 0x40, 0x06, 0x0a, 0x30]);
        let f = field("BATR", Access::DWord, &[("BSTS", 32), ("BEVT", 32)]);
        assert_eq!(f, [0x5b, 0x81, 0x10, b'B', b'A', b'T', b'R', 0x03, b'B', b'S', b'T', b'S', 0x20, b'B', b'E', b'V', b'T', 0x20]);
    }

    #[test]
    fn store_through_an_index_matches_iasl() {
        let s = store(path("BSTS"), index(path("BSTP"), int(1)));
        assert_eq!(s, [0x70, b'B', b'S', b'T', b'S', 0x88, b'B', b'S', b'T', b'P', 0x01, 0x00]);
    }

    #[test]
    fn store_between_a_local_and_a_field_matches_iasl() {
        assert_eq!(store(path("\\_SB.BEVT"), local(0)), [0x70, b'\\', 0x2e, b'_', b'S', b'B', b'_', b'B', b'E', b'V', b'T', 0x60]);
        assert_eq!(store(local(0), path("\\_SB.BEVT")), [0x70, 0x60, b'\\', 0x2e, b'_', b'S', b'B', b'_', b'B', b'E', b'V', b'T']);
    }

    #[test]
    fn resource_template_ends_with_an_end_tag() {
        let t = res::template([res::io(0x60, 1), res::irq_no_flags(1)]);
        assert_eq!(
            t,
            [0x11, 0x10, 0x0a, 0x0d, 0x47, 0x01, 0x60, 0x00, 0x60, 0x00, 0x01, 0x01, 0x22, 0x02, 0x00, 0x79, 0x00]
        );
    }

    #[test]
    fn address_space_descriptors_have_their_declared_lengths() {
        let w = res::word_io(0x0d00, 0xffff);
        assert_eq!(w.len(), 3 + w[1] as usize);
        assert_eq!(&w[w.len() - 2..], [0x00, 0xf3]);
        let d = res::dword_memory(0xe000_0000, 0xfebf_ffff);
        assert_eq!(d.len(), 3 + d[1] as usize);
        assert_eq!(&d[d.len() - 4..], 0x1ec0_0000u32.to_le_bytes());
        let i = res::ext_irq(&[5, 10, 11]);
        assert_eq!(i.len(), 3 + i[1] as usize);
    }
}

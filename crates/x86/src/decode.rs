//! x86-32 instruction decoder (one-byte and `0F` maps of a Pentium 4).

pub const MAX_LEN: usize = 15;

pub const NO_REG: u8 = 0xff;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rep {
    #[default]
    None,
    /// `F3`: REP / REPE.
    Rep,
    /// `F2`: REPNE.
    Repne,
}

/// A decoded memory operand. The effective address is
/// `base + (index << scale) + disp`, truncated to 16 bits when `addr32` is false.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemOperand {
    pub base: u8,
    pub index: u8,
    pub scale: u8,
    pub disp: u32,
    /// Segment used when there is no override: SS for BP/EBP/ESP-based forms.
    pub default_seg: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Insn {
    /// `0x000..=0x0ff` for the one-byte map, `0x100..=0x1ff` for `0F xx`.
    pub op: u16,
    pub len: u8,
    pub op32: bool,
    pub addr32: bool,
    /// Set when a `66` prefix was present; SSE uses it as a mandatory prefix.
    pub opsize_prefix: bool,
    pub seg_override: Option<u8>,
    pub rep: Rep,
    pub lock: bool,
    pub has_modrm: bool,
    pub modrm: u8,
    pub mem: Option<MemOperand>,
    pub imm: u32,
    pub imm2: u32,
}

impl Insn {
    pub fn reg(&self) -> usize {
        ((self.modrm >> 3) & 7) as usize
    }

    pub fn rm(&self) -> usize {
        (self.modrm & 7) as usize
    }

    pub fn seg(&self, mem: &MemOperand) -> usize {
        self.seg_override.unwrap_or(mem.default_seg) as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError<E> {
    /// The instruction would be longer than 15 bytes.
    TooLong,
    Fetch(E),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Imm {
    None,
    B,
    W,
    /// 16 or 32 bits, by operand size.
    Z,
    /// 16 or 32 bits, by address size (moffs).
    A,
    /// Far pointer: offset (Z) then a 16-bit selector.
    P,
    /// ENTER: 16-bit size then 8-bit level.
    WB,
}

fn has_modrm_1(op: u8) -> bool {
    match op {
        0x00..=0x3f => op & 7 < 4,
        0x62 | 0x63 | 0x69 | 0x6b => true,
        0x80..=0x8f => true,
        0xc0 | 0xc1 | 0xc4..=0xc7 => true,
        0xd0..=0xd3 | 0xd8..=0xdf => true,
        0xf6 | 0xf7 | 0xfe | 0xff => true,
        _ => false,
    }
}

fn has_modrm_0f(op: u8) -> bool {
    matches!(
        op,
        0x00..=0x03
            | 0x0d
            | 0x10..=0x23
            | 0x28..=0x2f
            | 0x40..=0x76
            | 0x78..=0x7f
            | 0x90..=0x9f
            | 0xa3..=0xa5
            | 0xab..=0xb7
            | 0xb9..=0xc7
            | 0xd0..=0xff
    )
}

fn imm_1(op: u8, reg: u8) -> Imm {
    match op {
        0x00..=0x3f => match op & 7 {
            4 => Imm::B,
            5 => Imm::Z,
            _ => Imm::None,
        },
        0x68 | 0x69 | 0x81 | 0xa9 | 0xb8..=0xbf | 0xc7 | 0xe8 | 0xe9 => Imm::Z,
        0x6a | 0x6b | 0x70..=0x7f | 0x80 | 0x82 | 0x83 | 0xa8 | 0xb0..=0xb7 => Imm::B,
        0xc0 | 0xc1 | 0xc6 | 0xcd | 0xd4 | 0xd5 | 0xe0..=0xe7 | 0xeb => Imm::B,
        0xa0..=0xa3 => Imm::A,
        0x9a | 0xea => Imm::P,
        0xc2 | 0xca => Imm::W,
        0xc8 => Imm::WB,
        0xf6 if reg < 2 => Imm::B,
        0xf7 if reg < 2 => Imm::Z,
        _ => Imm::None,
    }
}

fn imm_0f(op: u8) -> Imm {
    match op {
        0x70..=0x73 | 0xa4 | 0xac | 0xba | 0xc2 | 0xc4..=0xc6 => Imm::B,
        0x80..=0x8f => Imm::Z,
        _ => Imm::None,
    }
}

/// Decodes one instruction. `fetch(i)` returns the byte at offset `i` from the
/// start of the instruction; `code32` is the default operand/address size.
pub fn decode<E>(code32: bool, mut fetch: impl FnMut(u32) -> Result<u8, E>) -> Result<Insn, DecodeError<E>> {
    let mut insn = Insn { op32: code32, addr32: code32, ..Default::default() };
    let mut pos: u32 = 0;
    let mut next = |pos: &mut u32| -> Result<u8, DecodeError<E>> {
        if *pos as usize >= MAX_LEN {
            return Err(DecodeError::TooLong);
        }
        let b = fetch(*pos).map_err(DecodeError::Fetch)?;
        *pos += 1;
        Ok(b)
    };

    let mut op = loop {
        let b = next(&mut pos)?;
        match b {
            0x26 => insn.seg_override = Some(0),
            0x2e => insn.seg_override = Some(1),
            0x36 => insn.seg_override = Some(2),
            0x3e => insn.seg_override = Some(3),
            0x64 => insn.seg_override = Some(4),
            0x65 => insn.seg_override = Some(5),
            0x66 => {
                insn.op32 = !code32;
                insn.opsize_prefix = true;
            }
            0x67 => insn.addr32 = !code32,
            0xf0 => insn.lock = true,
            0xf2 => insn.rep = Rep::Repne,
            0xf3 => insn.rep = Rep::Rep,
            _ => break b as u16,
        }
    };

    let two_byte = op == 0x0f;
    if two_byte {
        op = 0x100 | next(&mut pos)? as u16;
    }
    insn.op = op;

    insn.has_modrm = if two_byte { has_modrm_0f(op as u8) } else { has_modrm_1(op as u8) };
    if insn.has_modrm {
        let modrm = next(&mut pos)?;
        insn.modrm = modrm;
        let md = modrm >> 6;
        let rm = modrm & 7;
        // MOV to/from control and debug registers ignores `mod` and always uses a register.
        let forced_reg = two_byte && (0x20..=0x23).contains(&(op as u8));
        if md != 3 && !forced_reg {
            insn.mem = Some(if insn.addr32 {
                decode_mem32(md, rm, &mut || next(&mut pos))?
            } else {
                decode_mem16(md, rm, &mut || next(&mut pos))?
            });
        }
    }

    let kind = if two_byte { imm_0f(op as u8) } else { imm_1(op as u8, insn.reg() as u8) };
    let mut read = |n: u32, pos: &mut u32| -> Result<u32, DecodeError<E>> {
        let mut v = 0u32;
        for i in 0..n {
            v |= (next(pos)? as u32) << (8 * i);
        }
        Ok(v)
    };
    let z = if insn.op32 { 4 } else { 2 };
    match kind {
        Imm::None => {}
        Imm::B => insn.imm = read(1, &mut pos)?,
        Imm::W => insn.imm = read(2, &mut pos)?,
        Imm::Z => insn.imm = read(z, &mut pos)?,
        Imm::A => insn.imm = read(if insn.addr32 { 4 } else { 2 }, &mut pos)?,
        Imm::P => {
            insn.imm = read(z, &mut pos)?;
            insn.imm2 = read(2, &mut pos)?;
        }
        Imm::WB => {
            insn.imm = read(2, &mut pos)?;
            insn.imm2 = read(1, &mut pos)?;
        }
    }

    insn.len = pos as u8;
    Ok(insn)
}

fn decode_mem16<E>(
    md: u8,
    rm: u8,
    next: &mut impl FnMut() -> Result<u8, DecodeError<E>>,
) -> Result<MemOperand, DecodeError<E>> {
    use crate::cpu::{EBP, EBX, EDI, ESI};
    const SS: u8 = crate::cpu::SS as u8;
    const DS: u8 = crate::cpu::DS as u8;
    let (base, index, seg) = match rm {
        0 => (EBX as u8, ESI as u8, DS),
        1 => (EBX as u8, EDI as u8, DS),
        2 => (EBP as u8, ESI as u8, SS),
        3 => (EBP as u8, EDI as u8, SS),
        4 => (ESI as u8, NO_REG, DS),
        5 => (EDI as u8, NO_REG, DS),
        6 if md == 0 => (NO_REG, NO_REG, DS),
        6 => (EBP as u8, NO_REG, SS),
        _ => (EBX as u8, NO_REG, DS),
    };
    let disp = match (md, rm) {
        (0, 6) | (2, _) => u16::from_le_bytes([next()?, next()?]) as u32,
        (1, _) => next()? as i8 as i32 as u32,
        _ => 0,
    };
    Ok(MemOperand { base, index, scale: 0, disp, default_seg: seg })
}

fn decode_mem32<E>(
    md: u8,
    rm: u8,
    next: &mut impl FnMut() -> Result<u8, DecodeError<E>>,
) -> Result<MemOperand, DecodeError<E>> {
    const SS: u8 = crate::cpu::SS as u8;
    const DS: u8 = crate::cpu::DS as u8;
    let mut m = MemOperand { base: rm, index: NO_REG, scale: 0, disp: 0, default_seg: DS };
    let mut disp32 = md == 2;
    if rm == 4 {
        let sib = next()?;
        let base = sib & 7;
        let index = (sib >> 3) & 7;
        m.scale = sib >> 6;
        m.index = if index == 4 { NO_REG } else { index };
        m.base = base;
        if base == 5 && md == 0 {
            m.base = NO_REG;
            disp32 = true;
        }
    } else if rm == 5 && md == 0 {
        m.base = NO_REG;
        disp32 = true;
    }
    if m.base == 4 || m.base == 5 {
        m.default_seg = SS;
    }
    if disp32 {
        m.disp = u32::from_le_bytes([next()?, next()?, next()?, next()?]);
    } else if md == 1 {
        m.disp = next()? as i8 as i32 as u32;
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(code32: bool, bytes: &[u8]) -> Insn {
        decode::<()>(code32, |i| bytes.get(i as usize).copied().ok_or(())).unwrap()
    }

    #[test]
    fn decodes_sib_with_disp8() {
        let i = dec(true, &[0x8b, 0x44, 0x24, 0x08]);
        assert_eq!((i.op, i.len), (0x8b, 4));
        assert_eq!(i.mem, Some(MemOperand { base: 4, index: NO_REG, scale: 0, disp: 8, default_seg: 2 }));
    }

    #[test]
    fn decodes_16bit_bp_disp_as_ss() {
        let i = dec(false, &[0x88, 0x5e, 0x60]);
        assert_eq!(i.mem.unwrap().default_seg, 2);
        assert_eq!(i.mem.unwrap().disp, 0x60);
    }

    #[test]
    fn decodes_prefixes_and_far_jump() {
        let i = dec(false, &[0x66, 0xea, 0x78, 0x56, 0x34, 0x12, 0x08, 0x00]);
        assert!(i.op32);
        assert_eq!((i.imm, i.imm2, i.len), (0x1234_5678, 8, 8));
    }

    #[test]
    fn group3_test_takes_immediate_only_for_test() {
        assert_eq!(dec(true, &[0xf6, 0xc0, 0x01]).len, 3);
        assert_eq!(dec(true, &[0xf6, 0xd0]).len, 2);
    }

    #[test]
    fn sse_opcodes_take_imm8_only_where_defined() {
        assert_eq!(dec(true, &[0x66, 0x0f, 0x73, 0xfb, 0x03]).len, 5);
        assert_eq!(dec(true, &[0x0f, 0xc2, 0x41, 0x10, 0x01]).len, 5);
        assert_eq!(dec(true, &[0xf2, 0x0f, 0x70, 0xc1, 0x1b]).len, 5);
        assert_eq!(dec(true, &[0x66, 0x0f, 0xc5, 0xc1, 0x07]).len, 5);
        assert_eq!(dec(true, &[0x0f, 0xc6, 0xc1, 0x44]).len, 4);
        assert_eq!(dec(true, &[0x0f, 0x77]).len, 2);
        assert_eq!(dec(true, &[0x66, 0x0f, 0xd6, 0xc1]).len, 4);
        assert_eq!(dec(true, &[0x0f, 0xae, 0xf8]).len, 3);
    }

    #[test]
    fn mandatory_66_is_recorded_separately_from_operand_size() {
        let i = dec(false, &[0x66, 0x0f, 0x58, 0xc1]);
        assert!(i.opsize_prefix);
        let i = dec(true, &[0x66, 0xf3, 0x0f, 0x6f, 0xc1]);
        assert_eq!((i.opsize_prefix, i.rep), (true, Rep::Rep));
    }

    #[test]
    fn rejects_too_long() {
        let bytes = [0x26u8; 16];
        assert_eq!(decode::<()>(true, |i| Ok(bytes[i as usize])), Err(DecodeError::TooLong));
    }
}

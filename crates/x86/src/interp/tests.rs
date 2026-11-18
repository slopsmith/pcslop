use super::{Ctx, Exception, Trap, vector};
use crate::bus::Bus;
use crate::cpu::{Cpu, Segment, attr, cr0, cr4, flags};
use crate::decode::Insn;
use crate::fpu::{F80, exc, sw};

const CODE: u32 = 0x1000;
const DATA: u32 = 0x2000;

struct Ram(Vec<u8>);

impl Bus for Ram {
    fn read8(&mut self, addr: u32) -> u8 {
        self.0[addr as usize & 0xf_ffff]
    }

    fn write8(&mut self, addr: u32, value: u8) {
        self.0[addr as usize & 0xf_ffff] = value;
    }

    fn io_read(&mut self, _: u16, _: u8) -> u32 {
        0
    }

    fn io_write(&mut self, _: u16, _: u8, _: u32) {}
}

struct Machine {
    cpu: Cpu,
    ram: Ram,
}

impl Machine {
    /// Flat 32-bit protected mode with SSE enabled and CR0.NE set.
    fn new() -> Machine {
        let mut cpu = Cpu::new();
        let flat = |code: bool| Segment {
            selector: if code { 8 } else { 0x10 },
            attr: attr::P | attr::S | attr::RW | attr::ACCESSED | attr::DB | attr::G | if code { attr::CODE } else { 0 },
            base: 0,
            limit: 0xffff_ffff,
        };
        for (i, s) in cpu.segs.iter_mut().enumerate() {
            *s = flat(i == crate::cpu::CS);
        }
        cpu.cr0 = cr0::PE | cr0::ET | cr0::NE;
        cpu.cr4 = cr4::OSFXSR | cr4::OSXMMEXCPT;
        Machine { cpu, ram: Ram(vec![0; 1 << 20]) }
    }

    fn run(&mut self, code: &[u8]) -> Result<(), Exception> {
        self.ram.0[CODE as usize..CODE as usize + code.len()].copy_from_slice(code);
        self.cpu.eip = CODE;
        let end = CODE + code.len() as u32;
        while self.cpu.eip < end {
            let start = self.cpu.eip;
            let mut ctx = Ctx { cpu: &mut self.cpu, bus: &mut self.ram, insn: Insn::default(), start_eip: start };
            match ctx.exec() {
                Ok(()) => {}
                Err(Trap::Fault(e)) => {
                    self.cpu.eip = start;
                    return Err(e);
                }
                Err(Trap::Unimplemented(what)) => panic!("unimplemented: {what}"),
            }
        }
        Ok(())
    }

    fn put(&mut self, addr: u32, bytes: &[u8]) {
        self.ram.0[addr as usize..addr as usize + bytes.len()].copy_from_slice(bytes);
    }

    fn get(&self, addr: u32, n: usize) -> &[u8] {
        &self.ram.0[addr as usize..addr as usize + n]
    }

    fn put_f64(&mut self, addr: u32, v: f64) {
        self.put(addr, &v.to_le_bytes());
    }

    fn get_f64(&self, addr: u32) -> f64 {
        f64::from_le_bytes(self.get(addr, 8).try_into().unwrap())
    }

    fn get_u32(&self, addr: u32) -> u32 {
        u32::from_le_bytes(self.get(addr, 4).try_into().unwrap())
    }

    fn st0(&self) -> f64 {
        self.cpu.fpu.st(0).to_f64(0).0
    }
}

/// `[disp32]` ModRM byte for a /`reg` memory operand.
fn m(reg: u8) -> u8 {
    reg << 3 | 5
}

fn addr(a: u32) -> [u8; 4] {
    a.to_le_bytes()
}

fn code(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn x87_loads_adds_and_stores_doubles() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, 1.5);
    mc.put_f64(DATA + 8, 2.25);
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xdc, m(0)], &addr(DATA + 8), &[0xdd, m(3)], &addr(DATA + 16)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.get_f64(DATA + 16), 3.75);
    assert_eq!(mc.cpu.fpu.tags, 0);
}

#[test]
fn x87_reversed_subtract_and_divide_pop_forms() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, 10.0);
    mc.put_f64(DATA + 8, 4.0);
    // fld [a]; fld [b]; fsubp st(1), st0 (DE E9): st1 = st1 - st0 = 6
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xdd, m(0)], &addr(DATA + 8), &[0xde, 0xe9]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), 6.0);
    // fld [b]; fdivrp st(1), st0 (DE F1): st1 = st0 / st1 = 4 / 6
    let c = code(&[&[0xdd, m(0)], &addr(DATA + 8), &[0xde, 0xf1]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), 4.0 / 6.0);
}

#[test]
fn fistp_honours_rounding_control() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, -2.5);
    let mut results = vec![];
    for rc in 0..4u16 {
        mc.put(DATA + 8, &(0x037f | rc << 10).to_le_bytes());
        let c = code(&[&[0xd9, m(5)], &addr(DATA + 8), &[0xdd, m(0)], &addr(DATA), &[0xdb, m(3)], &addr(DATA + 16)]);
        mc.run(&c).unwrap();
        results.push(mc.get_u32(DATA + 16) as i32);
    }
    assert_eq!(results, [-2, -3, -2, -2]);
    assert!(mc.cpu.fpu.sw & exc::PE != 0);
}

#[test]
fn fist_out_of_range_stores_integer_indefinite() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, 40000.0);
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xdf, m(3)], &addr(DATA + 8)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.get(DATA + 8, 2), [0x00, 0x80]);
    assert!(mc.cpu.fpu.sw & exc::IE != 0);
}

#[test]
fn fprem_reports_quotient_bits() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, 3.0);
    mc.put_f64(DATA + 8, 10.5);
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xdd, m(0)], &addr(DATA + 8), &[0xd9, 0xf8]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), 1.5);
    let s = mc.cpu.fpu.sw;
    assert_eq!((s & sw::C2, s & sw::C0 != 0, s & sw::C3 != 0, s & sw::C1 != 0), (0, false, true, true));
}

#[test]
fn fprem1_rounds_quotient_to_nearest() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, 3.0);
    mc.put_f64(DATA + 8, 11.0);
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xdd, m(0)], &addr(DATA + 8), &[0xd9, 0xf5]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), -1.0);
}

#[test]
fn fprem_on_huge_exponent_gap_is_partial() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, 3.0);
    mc.put_f64(DATA + 8, 1e300);
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xdd, m(0)], &addr(DATA + 8), &[0xd9, 0xf8]]);
    mc.run(&c).unwrap();
    assert!(mc.cpu.fpu.sw & sw::C2 != 0);
    for _ in 0..40 {
        if mc.cpu.fpu.sw & sw::C2 == 0 {
            break;
        }
        mc.run(&[0xd9, 0xf8]).unwrap();
    }
    assert_eq!(mc.cpu.fpu.sw & sw::C2, 0);
    let expect = 1e300f64 % 3.0;
    assert_eq!(mc.st0(), expect);
}

#[test]
fn stack_overflow_sets_invalid_and_c1() {
    let mut mc = Machine::new();
    let c = [0xd9, 0xe8].repeat(9);
    mc.run(&c).unwrap();
    let s = mc.cpu.fpu.sw;
    assert_eq!(s & (exc::IE | sw::SF | sw::C1), exc::IE | sw::SF | sw::C1);
    assert_eq!(mc.cpu.fpu.st(0), F80::INDEFINITE);
}

#[test]
fn unmasked_exception_raises_mf_on_next_fp_instruction() {
    let mut mc = Machine::new();
    mc.put(DATA, &(0x037fu16 & !(exc::ZE)).to_le_bytes());
    mc.put_f64(DATA + 8, 0.0);
    // fldcw; fld1; fdiv qword [zero]
    let c = code(&[&[0xd9, m(5)], &addr(DATA), &[0xd9, 0xe8, 0xdc, m(6)], &addr(DATA + 8)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), 1.0);
    assert!(mc.cpu.fpu.sw & (sw::ES | exc::ZE) == sw::ES | exc::ZE);
    assert_eq!(mc.run(&[0xd9, 0xe8]), Err(Exception::new(vector::MF)));
    assert_eq!(mc.run(&[0x9b]), Err(Exception::new(vector::MF)));
    mc.run(&[0xdb, 0xe2, 0x9b]).unwrap();
}

#[test]
fn pending_exception_without_ne_asserts_ferr() {
    let mut mc = Machine::new();
    mc.cpu.cr0 &= !cr0::NE;
    mc.put(DATA, &(0x037fu16 & !(exc::IE)).to_le_bytes());
    let c = code(&[&[0xd9, m(5)], &addr(DATA), &[0xd9, 0xe8, 0xd9, 0xfa, 0xd9, 0xe0, 0xd9, 0xfa, 0x9b]]);
    mc.run(&c).unwrap();
    assert!(mc.cpu.ferr_pending);
    mc.run(&[0xdb, 0xe2]).unwrap();
    assert!(!mc.cpu.ferr_pending);
}

#[test]
fn em_and_ts_raise_nm() {
    let mut mc = Machine::new();
    mc.cpu.cr0 |= cr0::TS;
    assert_eq!(mc.run(&[0xd9, 0xe8]), Err(Exception::new(vector::NM)));
    assert_eq!(mc.run(&[0x9b]), Ok(()));
    mc.cpu.cr0 |= cr0::MP;
    assert_eq!(mc.run(&[0x9b]), Err(Exception::new(vector::NM)));
    mc.cpu.cr0 = (mc.cpu.cr0 & !cr0::TS) | cr0::EM;
    assert_eq!(mc.run(&[0x0f, 0xfc, 0xc1]), Err(Exception::new(vector::UD)));
    assert_eq!(mc.run(&[0x0f, 0x58, 0xc1]), Err(Exception::new(vector::UD)));
}

#[test]
fn fcomi_sets_eflags() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, 1.0);
    mc.put_f64(DATA + 8, 2.0);
    // fld [2.0]; fld [1.0]; fcomi st, st(1): 1 < 2 -> CF
    let c = code(&[&[0xdd, m(0)], &addr(DATA + 8), &[0xdd, m(0)], &addr(DATA), &[0xdb, 0xf1]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.cpu.eflags & (flags::ZF | flags::PF | flags::CF), flags::CF);
    // fucomip with a QNaN in st0 -> unordered, no IE
    mc.put_f64(DATA + 16, f64::NAN);
    mc.cpu.fpu.sw &= !exc::IE;
    let c = code(&[&[0xdd, m(0)], &addr(DATA + 16), &[0xdf, 0xe9]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.cpu.eflags & (flags::ZF | flags::PF | flags::CF), flags::ZF | flags::PF | flags::CF);
    assert_eq!(mc.cpu.fpu.sw & exc::IE, 0);
}

#[test]
fn fnstsw_ax_reports_condition_codes() {
    let mut mc = Machine::new();
    // fldz; ftst; fnstsw ax -> C3 set
    mc.run(&[0xd9, 0xee, 0xd9, 0xe4, 0xdf, 0xe0]).unwrap();
    assert_eq!(mc.cpu.regs[0] as u16 & (sw::C3 | sw::C2 | sw::C0), sw::C3);
    assert_eq!((mc.cpu.regs[0] as u16 & sw::TOP) >> 11, 7);
}

#[test]
fn fbstp_and_fbld_round_trip() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, -1234567.0);
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xdf, m(6)], &addr(DATA + 16), &[0xdf, m(4)], &addr(DATA + 16)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.get(DATA + 16, 10), [0x67, 0x45, 0x23, 0x01, 0, 0, 0, 0, 0, 0x80]);
    assert_eq!(mc.st0(), -1234567.0);
}

#[test]
fn fsave_frstor_round_trip_and_reinitialise() {
    let mut mc = Machine::new();
    // fld1; fldpi; fldz; fnsave [DATA]
    let c = code(&[&[0xd9, 0xe8, 0xd9, 0xeb, 0xd9, 0xee, 0xdd, m(6)], &addr(DATA)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.cpu.fpu.tags, 0);
    assert_eq!(mc.cpu.fpu.cw, 0x037f);
    let tw = u16::from_le_bytes(mc.get(DATA + 8, 2).try_into().unwrap());
    assert_eq!(tw, 0x03ff | 1 << 10);
    assert_eq!(mc.get(DATA + 28, 10), &F80::ZERO.to_bytes());
    let c = code(&[&[0xdd, m(4)], &addr(DATA)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.cpu.fpu.top(), 5);
    assert_eq!(mc.st0(), 0.0);
    assert_eq!(mc.cpu.fpu.st(2), F80::ONE);
}

#[test]
fn fstenv_16bit_real_mode_stores_linear_ip() {
    let mut mc = Machine::new();
    mc.cpu.cr0 = cr0::ET | cr0::NE;
    for s in mc.cpu.segs.iter_mut() {
        *s = Segment::real(0x100);
    }
    mc.cpu.segs[crate::cpu::CS].attr |= attr::CODE;
    // In real mode the code lives at 0x1000 + 0x1000 = linear 0x2000.
    let fld1 = [0xd9u8, 0xe8];
    let fnstenv = [0xd9u8, 0x36, 0x00, 0x30];
    mc.put(0x2000, &fld1);
    mc.put(0x2002, &fnstenv);
    mc.cpu.eip = 0x1000;
    for _ in 0..2 {
        let start = mc.cpu.eip;
        let mut ctx = Ctx { cpu: &mut mc.cpu, bus: &mut mc.ram, insn: Insn::default(), start_eip: start };
        ctx.exec().unwrap();
    }
    let env = mc.get(0x4000, 14).to_vec();
    let w = |i: usize| u16::from_le_bytes([env[2 * i], env[2 * i + 1]]);
    assert_eq!(w(0), 0x037f);
    assert_eq!(w(3), 0x2000);
    assert_eq!(w(4), 0x01e8);
    assert_eq!(mc.cpu.fpu.cw & 0x3f, 0x3f);
}

#[test]
fn transcendentals_and_constants() {
    let mut mc = Machine::new();
    // fldpi; fsin -> ~0
    mc.run(&[0xd9, 0xeb, 0xd9, 0xfe]).unwrap();
    assert!(mc.st0().abs() < 1e-15);
    // fld1; fld1; fpatan -> pi/4
    mc.run(&[0xd9, 0xe8, 0xd9, 0xe8, 0xd9, 0xf3]).unwrap();
    assert!((mc.st0() - std::f64::consts::FRAC_PI_4).abs() < 1e-15);
    // fld1; f2xm1 -> 1
    mc.run(&[0xd9, 0xe8, 0xd9, 0xf0]).unwrap();
    assert!((mc.st0() - 1.0).abs() < 1e-15);
    // fld1; fld1; fadd st0; fscale (st0 = 2 * 2^1)
    mc.run(&[0xd9, 0xe8, 0xd9, 0xe8, 0xd8, 0xc0, 0xd9, 0xfd]).unwrap();
    assert_eq!(mc.st0(), 4.0);
}

#[test]
fn fxtract_splits_exponent_and_significand() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, -24.0);
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xd9, 0xf4]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), -1.5);
    assert_eq!(mc.cpu.fpu.st(1).to_f64(0).0, 4.0);
}

#[test]
fn frndint_uses_rounding_control() {
    let mut mc = Machine::new();
    mc.put_f64(DATA, 2.5);
    mc.put(DATA + 8, &0x0b7fu16.to_le_bytes());
    let c = code(&[&[0xdd, m(0)], &addr(DATA), &[0xd9, 0xfc]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), 2.0);
    let c = code(&[&[0xd9, m(5)], &addr(DATA + 8), &[0xdd, m(0)], &addr(DATA), &[0xd9, 0xfc]]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), 3.0);
}

#[test]
fn single_precision_control_rounds_results() {
    let mut mc = Machine::new();
    mc.put(DATA, &0x007fu16.to_le_bytes());
    mc.put_f64(DATA + 8, 3.0);
    // fldcw (PC = single); fld1; fdiv qword [3.0]
    let c = code(&[&[0xd9, m(5)], &addr(DATA), &[0xd9, 0xe8, 0xdc, m(6)], &addr(DATA + 8)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.st0(), (1.0f32 / 3.0) as f64);
}

#[test]
fn mmx_instruction_sets_top_zero_and_tags_valid() {
    let mut mc = Machine::new();
    // fld1 (TOP=7); movd mm0, eax; emms
    mc.cpu.regs[0] = 0x1234_5678;
    mc.run(&[0xd9, 0xe8, 0x0f, 0x6e, 0xc0]).unwrap();
    assert_eq!(mc.cpu.fpu.top(), 0);
    assert_eq!(mc.cpu.fpu.tags, 0xff);
    assert_eq!(mc.cpu.fpu.regs[0], F80::new(0xffff, 0x1234_5678));
    mc.run(&[0x0f, 0x77]).unwrap();
    assert_eq!(mc.cpu.fpu.tags, 0);
}

#[test]
fn mmx_saturating_and_multiply_ops() {
    let mut mc = Machine::new();
    mc.cpu.fpu.set_mm(0, 0x7ff0_8000_0001_fffe);
    mc.cpu.fpu.set_mm(1, 0x0020_8000_0002_0003);
    // paddsw mm0, mm1
    mc.run(&[0x0f, 0xed, 0xc1]).unwrap();
    assert_eq!(mc.cpu.fpu.mm(0), 0x7fff_8000_0003_0001);
    mc.cpu.fpu.set_mm(2, 0x0001_0002_0003_0004);
    mc.cpu.fpu.set_mm(3, 0x0005_0006_0007_0008);
    // pmaddwd mm2, mm3
    mc.run(&[0x0f, 0xf5, 0xd3]).unwrap();
    assert_eq!(mc.cpu.fpu.mm(2), (5 + 12u64) << 32 | (21 + 32));
    // psadbw mm4, mm5
    mc.cpu.fpu.set_mm(4, 0x0102_0304_0506_0708);
    mc.cpu.fpu.set_mm(5, 0x0807_0605_0403_0201);
    mc.run(&[0x0f, 0xf6, 0xe5]).unwrap();
    assert_eq!(mc.cpu.fpu.mm(4), 32);
}

#[test]
fn pshufw_and_pextrw() {
    let mut mc = Machine::new();
    mc.cpu.fpu.set_mm(1, 0x4444_3333_2222_1111);
    mc.run(&[0x0f, 0x70, 0xc1, 0x1b, 0x0f, 0xc5, 0xd8, 0x01]).unwrap();
    assert_eq!(mc.cpu.fpu.mm(0), 0x1111_2222_3333_4444);
    assert_eq!(mc.cpu.regs[3], 0x3333);
}

#[test]
fn cvttsd2si_out_of_range_gives_integer_indefinite() {
    let mut mc = Machine::new();
    mc.cpu.xmm[1] = 3e10f64.to_bits() as u128;
    mc.run(&[0xf2, 0x0f, 0x2c, 0xc1]).unwrap();
    assert_eq!(mc.cpu.regs[0], 0x8000_0000);
    assert!(mc.cpu.mxcsr & 1 != 0);
    mc.cpu.xmm[1] = (-2.7f64).to_bits() as u128;
    mc.run(&[0xf2, 0x0f, 0x2c, 0xc1]).unwrap();
    assert_eq!(mc.cpu.regs[0] as i32, -2);
    mc.run(&[0xf2, 0x0f, 0x2d, 0xc1]).unwrap();
    assert_eq!(mc.cpu.regs[0] as i32, -3);
}

#[test]
fn cvtss2si_follows_mxcsr_rounding() {
    let mut mc = Machine::new();
    mc.cpu.xmm[1] = 2.5f32.to_bits() as u128;
    mc.cpu.mxcsr = 0x1f80 | 2 << 13;
    mc.run(&[0xf3, 0x0f, 0x2d, 0xc1]).unwrap();
    assert_eq!(mc.cpu.regs[0], 3);
}

#[test]
fn minps_returns_second_operand_on_nan_and_zeros() {
    let mut mc = Machine::new();
    let lanes = |v: [f32; 4]| v.iter().enumerate().fold(0u128, |a, (i, x)| a | (x.to_bits() as u128) << (32 * i));
    mc.cpu.xmm[0] = lanes([f32::NAN, 1.0, 0.0, -0.0]);
    mc.cpu.xmm[1] = lanes([5.0, f32::NAN, -0.0, 0.0]);
    mc.run(&[0x0f, 0x5d, 0xc1]).unwrap();
    assert_eq!(mc.cpu.xmm[0], lanes([5.0, f32::NAN, -0.0, 0.0]));
    assert!(mc.cpu.mxcsr & 1 != 0);
}

#[test]
fn maxsd_keeps_upper_lane_of_destination() {
    let mut mc = Machine::new();
    mc.cpu.xmm[0] = (7.0f64.to_bits() as u128) << 64 | 1.0f64.to_bits() as u128;
    mc.cpu.xmm[1] = (9.0f64.to_bits() as u128) << 64 | 2.0f64.to_bits() as u128;
    mc.run(&[0xf2, 0x0f, 0x5f, 0xc1]).unwrap();
    assert_eq!(mc.cpu.xmm[0], (7.0f64.to_bits() as u128) << 64 | 2.0f64.to_bits() as u128);
}

#[test]
fn cmpps_predicates() {
    let mut mc = Machine::new();
    let lanes = |v: [f32; 4]| v.iter().enumerate().fold(0u128, |a, (i, x)| a | (x.to_bits() as u128) << (32 * i));
    let a = lanes([1.0, 2.0, f32::NAN, 4.0]);
    let b = lanes([1.0, 1.0, 1.0, 5.0]);
    let mut got = vec![];
    for pred in 0..8u8 {
        mc.cpu.xmm[0] = a;
        mc.cpu.xmm[1] = b;
        mc.run(&[0x0f, 0xc2, 0xc1, pred]).unwrap();
        got.push((0..4).map(|i| (mc.cpu.xmm[0] >> (32 * i)) as u32 != 0).collect::<Vec<_>>());
    }
    let t = true;
    let f = false;
    assert_eq!(
        got,
        [[t, f, f, f], [f, f, f, t], [t, f, f, t], [f, f, t, f], [f, t, t, t], [t, t, t, f], [f, t, t, f], [t, t, f, t]]
    );
}

#[test]
fn comisd_sets_flags_and_signals_on_qnan() {
    let mut mc = Machine::new();
    mc.cpu.xmm[0] = 1.0f64.to_bits() as u128;
    mc.cpu.xmm[1] = 1.0f64.to_bits() as u128;
    mc.run(&[0x66, 0x0f, 0x2f, 0xc1]).unwrap();
    assert_eq!(mc.cpu.eflags & (flags::ZF | flags::PF | flags::CF), flags::ZF);
    mc.cpu.xmm[1] = f64::NAN.to_bits() as u128;
    mc.run(&[0x66, 0x0f, 0x2e, 0xc1]).unwrap();
    assert_eq!(mc.cpu.mxcsr & 1, 0);
    assert_eq!(mc.cpu.eflags & (flags::ZF | flags::PF | flags::CF), flags::ZF | flags::PF | flags::CF);
    mc.run(&[0x66, 0x0f, 0x2f, 0xc1]).unwrap();
    assert_eq!(mc.cpu.mxcsr & 1, 1);
}

#[test]
fn unmasked_simd_exception_raises_xm_or_ud_and_keeps_destination() {
    let mut mc = Machine::new();
    mc.cpu.mxcsr = 0x1f80 & !(1 << 9);
    mc.cpu.xmm[0] = 1.0f32.to_bits() as u128;
    mc.cpu.xmm[1] = 0;
    assert_eq!(mc.run(&[0xf3, 0x0f, 0x5e, 0xc1]), Err(Exception::new(vector::XM)));
    assert_eq!(mc.cpu.xmm[0], 1.0f32.to_bits() as u128);
    mc.cpu.cr4 &= !cr4::OSXMMEXCPT;
    assert_eq!(mc.run(&[0xf3, 0x0f, 0x5e, 0xc1]), Err(Exception::new(vector::UD)));
}

#[test]
fn sse_requires_osfxsr() {
    let mut mc = Machine::new();
    mc.cpu.cr4 = 0;
    assert_eq!(mc.run(&[0x0f, 0x28, 0xc1]), Err(Exception::new(vector::UD)));
    assert_eq!(mc.run(&[0x0f, 0xfc, 0xc1]), Ok(()));
}

#[test]
fn movaps_faults_on_misaligned_memory() {
    let mut mc = Machine::new();
    let c = code(&[&[0x0f, 0x28, m(0)], &addr(DATA + 8)]);
    assert_eq!(mc.run(&c), Err(Exception::with_error(vector::GP, 0)));
    let c = code(&[&[0x0f, 0x10, m(0)], &addr(DATA + 8)]);
    assert_eq!(mc.run(&c), Ok(()));
}

#[test]
fn daz_and_ftz() {
    let mut mc = Machine::new();
    let tiny = f32::from_bits(1);
    mc.cpu.xmm[0] = tiny.to_bits() as u128;
    mc.cpu.xmm[1] = 1.0f32.to_bits() as u128;
    mc.cpu.mxcsr = 0x1f80 | 1 << 6;
    mc.run(&[0xf3, 0x0f, 0x59, 0xc1]).unwrap();
    assert_eq!(mc.cpu.xmm[0] as u32, 0);
    assert_eq!(mc.cpu.mxcsr & exc::DE as u32, 0);
    mc.cpu.xmm[0] = f32::MIN_POSITIVE.to_bits() as u128;
    mc.cpu.xmm[1] = 0.5f32.to_bits() as u128;
    mc.cpu.mxcsr = 0x1f80 | 1 << 15;
    mc.run(&[0xf3, 0x0f, 0x59, 0xc1]).unwrap();
    assert_eq!(mc.cpu.xmm[0] as u32, 0);
    assert_eq!(mc.cpu.mxcsr & 0x30, 0x30);
}

#[test]
fn sse2_integer_shuffles_and_byte_shifts() {
    let mut mc = Machine::new();
    mc.cpu.xmm[1] = 0x0f0e0d0c_0b0a0908_07060504_03020100;
    // pshufd xmm0, xmm1, 0x1b
    mc.run(&[0x66, 0x0f, 0x70, 0xc1, 0x1b]).unwrap();
    assert_eq!(mc.cpu.xmm[0], 0x03020100_07060504_0b0a0908_0f0e0d0c);
    // psrldq xmm1, 3
    mc.run(&[0x66, 0x0f, 0x73, 0xd9, 0x03]).unwrap();
    assert_eq!(mc.cpu.xmm[1], 0x0f0e0d0c_0b0a0908_07060504_03020100 >> 24);
    // pshuflw xmm2, xmm0, 0x00
    mc.run(&[0xf2, 0x0f, 0x70, 0xd0, 0x00]).unwrap();
    assert_eq!(mc.cpu.xmm[2] as u64, 0x0d0c_0d0c_0d0c_0d0c);
    assert_eq!((mc.cpu.xmm[2] >> 64) as u64, (mc.cpu.xmm[0] >> 64) as u64);
}

#[test]
fn pmuludq_and_paddq() {
    let mut mc = Machine::new();
    mc.cpu.xmm[0] = (0xffff_ffffu128 << 64) | 3;
    mc.cpu.xmm[1] = (0xffff_ffffu128 << 64) | 0x1_0000_0005;
    mc.run(&[0x66, 0x0f, 0xf4, 0xc1]).unwrap();
    assert_eq!(mc.cpu.xmm[0], (0xffff_fffe_0000_0001u128 << 64) | 15);
    mc.cpu.fpu.set_mm(0, u64::MAX);
    mc.cpu.fpu.set_mm(1, 2);
    mc.run(&[0x0f, 0xd4, 0xc1]).unwrap();
    assert_eq!(mc.cpu.fpu.mm(0), 1);
}

#[test]
fn mandatory_66_prefix_does_not_change_operand_size() {
    let mut mc = Machine::new();
    mc.cpu.regs[0] = 0xdead_beef;
    mc.cpu.xmm[1] = 0x1122_3344_5566_7788;
    // movd xmm1, eax then movd [DATA], xmm1
    let c = code(&[&[0x66, 0x0f, 0x6e, 0xc8, 0x66, 0x0f, 0x7e, m(1)], &addr(DATA)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.cpu.xmm[1], 0xdead_beef);
    assert_eq!(mc.get_u32(DATA), 0xdead_beef);
}

#[test]
fn conversions_between_widths() {
    let mut mc = Machine::new();
    mc.cpu.xmm[1] = (2.5f64.to_bits() as u128) << 64 | (1e40f64).to_bits() as u128;
    // cvtpd2ps xmm0, xmm1
    mc.run(&[0x66, 0x0f, 0x5a, 0xc1]).unwrap();
    assert_eq!(mc.cpu.xmm[0], (2.5f32.to_bits() as u128) << 32 | f32::INFINITY.to_bits() as u128);
    assert!(mc.cpu.mxcsr & exc::OE as u32 != 0);
    mc.cpu.xmm[1] = (-7i32 as u32 as u128) << 32 | 16_777_217;
    // cvtdq2pd xmm0, xmm1
    mc.run(&[0xf3, 0x0f, 0xe6, 0xc1]).unwrap();
    assert_eq!(mc.cpu.xmm[0], ((-7.0f64).to_bits() as u128) << 64 | 16_777_217.0f64.to_bits() as u128);
}

#[test]
fn fxsave_layout_and_restore() {
    let mut mc = Machine::new();
    mc.cpu.xmm[3] = 0x0123_4567_89ab_cdef_0011_2233_4455_6677;
    mc.cpu.mxcsr = 0x1f80 | 3 << 13;
    // fld1; fldz; fxsave [DATA]
    let c = code(&[&[0xd9, 0xe8, 0xd9, 0xee, 0x0f, 0xae, m(0)], &addr(DATA)]);
    mc.run(&c).unwrap();
    let img = mc.get(DATA, 512).to_vec();
    assert_eq!(&img[0..2], &[0x7f, 0x03]);
    assert_eq!(u16::from_le_bytes([img[2], img[3]]) >> 11 & 7, 6);
    assert_eq!(img[4], 0b1100_0000);
    assert_eq!(u32::from_le_bytes(img[24..28].try_into().unwrap()), 0x7f80);
    assert_eq!(u32::from_le_bytes(img[28..32].try_into().unwrap()), 0xffff);
    assert_eq!(&img[32..42], &F80::ZERO.to_bytes());
    assert_eq!(&img[48..58], &F80::ONE.to_bytes());
    assert_eq!(&img[160 + 48..160 + 64], &mc.cpu.xmm[3].to_le_bytes());
    let saved = mc.cpu.clone();
    mc.run(&[0xdb, 0xe3, 0x0f, 0x57, 0xdb]).unwrap();
    let c = code(&[&[0x0f, 0xae, m(1)], &addr(DATA)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.cpu.fpu, saved.fpu);
    assert_eq!(mc.cpu.xmm, saved.xmm);
    assert_eq!(mc.cpu.mxcsr, saved.mxcsr);
}

#[test]
fn fxsave_skips_xmm_without_osfxsr_and_checks_alignment() {
    let mut mc = Machine::new();
    mc.cpu.cr4 = 0;
    mc.cpu.xmm[0] = u128::MAX;
    let c = code(&[&[0x0f, 0xae, m(0)], &addr(DATA)]);
    mc.run(&c).unwrap();
    assert_eq!(mc.get(DATA + 160, 16), [0; 16]);
    let c = code(&[&[0x0f, 0xae, m(0)], &addr(DATA + 4)]);
    assert_eq!(mc.run(&c), Err(Exception::with_error(vector::GP, 0)));
}

#[test]
fn ldmxcsr_rejects_reserved_bits() {
    let mut mc = Machine::new();
    mc.put(DATA, &0x1_0000u32.to_le_bytes());
    let c = code(&[&[0x0f, 0xae, m(2)], &addr(DATA)]);
    assert_eq!(mc.run(&c), Err(Exception::with_error(vector::GP, 0)));
}

#[test]
fn maskmovq_writes_selected_bytes() {
    let mut mc = Machine::new();
    mc.cpu.regs[crate::cpu::EDI] = DATA;
    mc.put(DATA, &[0xaa; 8]);
    mc.cpu.fpu.set_mm(0, 0x1122_3344_5566_7788);
    mc.cpu.fpu.set_mm(1, 0x8000_0000_0000_0080);
    mc.run(&[0x0f, 0xf7, 0xc1]).unwrap();
    assert_eq!(mc.get(DATA, 8), [0x88, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x11]);
}

#[test]
fn rdmsr_reports_the_northwood_bus_and_core_ratio() {
    use crate::cpu::{EAX, ECX, EDX};
    let mut mc = Machine::new();
    mc.cpu.regs[ECX] = 0x2c;
    mc.run(&[0x0f, 0x32]).unwrap();
    assert_eq!((mc.cpu.regs[EAX] >> 24, mc.cpu.regs[EAX] >> 16 & 7, mc.cpu.regs[EDX]), (18, 1, 0));
}

#[test]
fn debugctl_keeps_its_bits_and_the_branch_records_read_empty() {
    use crate::cpu::{EAX, ECX, EDX};
    let mut mc = Machine::new();
    mc.cpu.regs[ECX] = 0x1d9;
    mc.cpu.regs[EAX] = 0xffff_ff43;
    mc.cpu.regs[EDX] = 0;
    mc.run(&[0x0f, 0x30]).unwrap();
    mc.cpu.regs[EAX] = 0;
    mc.run(&[0x0f, 0x32]).unwrap();
    assert_eq!(mc.cpu.regs[EAX], 0x43);
    for msr in [0x1d7, 0x1d8, 0x1da, 0x1db, 0x1de] {
        mc.cpu.regs[ECX] = msr;
        mc.cpu.regs[EAX] = 1;
        mc.run(&[0x0f, 0x32]).unwrap();
        assert_eq!((mc.cpu.regs[EAX], mc.cpu.regs[EDX]), (0, 0), "MSR {msr:#x}");
    }
}

#[test]
fn misc_enable_reads_fast_strings_and_ignores_writes() {
    use crate::cpu::{EAX, ECX, EDX};
    let mut mc = Machine::new();
    mc.cpu.regs[ECX] = 0x1a0;
    mc.cpu.regs[EAX] = 0;
    mc.cpu.regs[EDX] = 0;
    mc.run(&[0x0f, 0x30]).unwrap();
    mc.run(&[0x0f, 0x32]).unwrap();
    assert_eq!((mc.cpu.regs[EAX], mc.cpu.regs[EDX]), (0x1801, 0));
}

#[test]
fn rdmsr_of_an_unknown_msr_raises_gp() {
    let mut mc = Machine::new();
    mc.cpu.regs[crate::cpu::ECX] = 0x2a;
    assert_eq!(mc.run(&[0x0f, 0x32]), Err(Exception::with_error(vector::GP, 0)));
}

#[test]
fn undefined_opcodes_raise_ud() {
    let mut mc = Machine::new();
    assert_eq!(mc.run(&[0x0f, 0x3f]), Err(Exception::new(vector::UD)), "CPU-Z probes VIA's ALTINST jump");
}

use super::*;
use std::process::Command;
use Size::{W, X};

type Case = (&'static str, fn(&mut Asm));

const CASES: &[Case] = &[
    ("add x0, x1, x2", |a| a.add(X, r(0), r(1), r(2))),
    ("add w3, w4, #4095", |a| a.add_imm(W, r(3), r(4), 4095)),
    ("add x3, sp, #0x5000", |a| a.add_imm(X, r(3), SP, 0x5000)),
    ("subs w5, w6, w7, lsl #3", |a| a.subs_shift(W, r(5), r(6), r(7), Shift::Lsl, 3)),
    ("cmp x8, #12", |a| a.cmp_imm(X, r(8), 12)),
    ("cmn w8, w9", |a| a.cmn(W, r(8), r(9))),
    ("neg x1, x2", |a| a.neg(X, r(1), r(2))),
    ("add x0, x1, w2, uxtw #2", |a| a.addsub_ext(X, false, false, r(0), r(1), r(2), Extend::Uxtw, 2)),
    ("adcs w1, w2, w3", |a| a.adcs(W, r(1), r(2), r(3))),
    ("sbc x1, x2, x3", |a| a.sbc(X, r(1), r(2), r(3))),
    ("and w0, w1, #0xff", |a| assert!(a.logic_imm(Logic::And, W, r(0), r(1), 0xff))),
    ("orr x0, x1, #0x5555555555555555", |a| assert!(a.logic_imm(Logic::Orr, X, r(0), r(1), 0x5555_5555_5555_5555))),
    ("eor w2, w3, #0x80000000", |a| assert!(a.logic_imm(Logic::Eor, W, r(2), r(3), 0x8000_0000))),
    ("ands x4, x5, #0xfffffffffffff000", |a| assert!(a.logic_imm(Logic::Ands, X, r(4), r(5), 0xffff_ffff_ffff_f000))),
    ("and w6, w7, #0x7ffffffe", |a| assert!(a.logic_imm(Logic::And, W, r(6), r(7), 0x7fff_fffe))),
    ("orr w0, w1, w2, ror #7", |a| a.logic_shift(Logic::Orr, false, W, r(0), r(1), r(2), Shift::Ror, 7)),
    ("bic x0, x1, x2", |a| a.bic(X, r(0), r(1), r(2))),
    ("mvn w3, w4", |a| a.mvn(W, r(3), r(4))),
    ("tst w1, w2", |a| a.tst(W, r(1), r(2))),
    ("mov x9, x10", |a| a.mov(X, r(9), r(10))),
    ("mov sp, x1", |a| a.mov_sp(SP, r(1))),
    ("movz w1, #0x1234, lsl #16", |a| a.movz(W, r(1), 0x1234, 1)),
    ("movk x1, #0xbeef, lsl #48", |a| a.movk(X, r(1), 0xbeef, 3)),
    ("movn x2, #0", |a| a.movn(X, r(2), 0, 0)),
    ("lsl w1, w2, #5", |a| a.lsl_imm(W, r(1), r(2), 5)),
    ("lsr x1, x2, #63", |a| a.lsr_imm(X, r(1), r(2), 63)),
    ("asr w1, w2, #31", |a| a.asr_imm(W, r(1), r(2), 31)),
    ("ror w1, w2, #8", |a| a.ror_imm(W, r(1), r(2), 8)),
    ("ubfx w1, w2, #8, #8", |a| a.ubfx(W, r(1), r(2), 8, 8)),
    ("sbfx x1, x2, #3, #10", |a| a.sbfx(X, r(1), r(2), 3, 10)),
    ("bfi w1, w2, #8, #8", |a| a.bfi(W, r(1), r(2), 8, 8)),
    ("bfxil w1, w2, #16, #16", |a| a.bfxil(W, r(1), r(2), 16, 16)),
    ("uxtb w1, w2", |a| a.uxtb(r(1), r(2))),
    ("uxth w1, w2", |a| a.uxth(r(1), r(2))),
    ("sxtb x1, w2", |a| a.sxtb(X, r(1), r(2))),
    ("sxth w1, w2", |a| a.sxth(W, r(1), r(2))),
    ("sxtw x1, w2", |a| a.sxtw(r(1), r(2))),
    ("extr x1, x2, x3, #17", |a| a.extr(X, r(1), r(2), r(3), 17)),
    ("udiv w1, w2, w3", |a| a.udiv(W, r(1), r(2), r(3))),
    ("sdiv x1, x2, x3", |a| a.sdiv(X, r(1), r(2), r(3))),
    ("lsl w1, w2, w3", |a| a.lslv(W, r(1), r(2), r(3))),
    ("ror x1, x2, x3", |a| a.rorv(X, r(1), r(2), r(3))),
    ("rbit w1, w2", |a| a.rbit(W, r(1), r(2))),
    ("rev16 w1, w2", |a| a.rev16(W, r(1), r(2))),
    ("rev w1, w2", |a| a.rev(W, r(1), r(2))),
    ("rev x1, x2", |a| a.rev(X, r(1), r(2))),
    ("clz w1, w2", |a| a.clz(W, r(1), r(2))),
    ("mul w1, w2, w3", |a| a.mul(W, r(1), r(2), r(3))),
    ("msub x1, x2, x3, x4", |a| a.msub(X, r(1), r(2), r(3), r(4))),
    ("smull x1, w2, w3", |a| a.smull(r(1), r(2), r(3))),
    ("umull x1, w2, w3", |a| a.umull(r(1), r(2), r(3))),
    ("smulh x1, x2, x3", |a| a.smulh(r(1), r(2), r(3))),
    ("umulh x1, x2, x3", |a| a.umulh(r(1), r(2), r(3))),
    ("csel w1, w2, w3, eq", |a| a.csel(W, r(1), r(2), r(3), Cond::Eq)),
    ("csinc x1, x2, x3, lt", |a| a.csinc(X, r(1), r(2), r(3), Cond::Lt)),
    ("cset w1, hs", |a| a.cset(W, r(1), Cond::Hs)),
    ("csetm x1, vs", |a| a.csetm(X, r(1), Cond::Vs)),
    ("cinc w1, w2, ne", |a| a.cinc(W, r(1), r(2), Cond::Ne)),
    ("csneg w1, w2, w3, gt", |a| a.csneg(W, r(1), r(2), r(3), Cond::Gt)),
    ("ccmp w1, w2, #4, ne", |a| a.ccmp(W, r(1), r(2), 4, Cond::Ne)),
    ("ccmp x1, #31, #15, eq", |a| a.ccmp_imm(X, r(1), 31, 15, Cond::Eq)),
    ("ccmn w1, #1, #0, le", |a| a.ccmn_imm(W, r(1), 1, 0, Cond::Le)),
    ("cfinv", |a| a.cfinv()),
    ("axflag", |a| a.axflag()),
    ("xaflag", |a| a.xaflag()),
    ("rmif x3, #9, #2", |a| a.rmif(r(3), 9, 2)),
    ("setf8 w4", |a| a.setf8(r(4))),
    ("setf16 w5", |a| a.setf16(r(5))),
    ("ldr w1, [x2, #8]", |a| a.ldr(Width::W, r(1), r(2), 8)),
    ("ldr x1, [sp, #32760]", |a| a.ldr(Width::X, r(1), SP, 32760)),
    ("ldrb w1, [x2, #4095]", |a| a.ldr(Width::B, r(1), r(2), 4095)),
    ("ldrh w1, [x2, #2]", |a| a.ldr(Width::H, r(1), r(2), 2)),
    ("ldur w1, [x2, #-4]", |a| a.ldr(Width::W, r(1), r(2), -4)),
    ("ldur x1, [x2, #3]", |a| a.ldr(Width::X, r(1), r(2), 3)),
    ("strb w1, [x2]", |a| a.str(Width::B, r(1), r(2), 0)),
    ("str x1, [x2, #16]", |a| a.str(Width::X, r(1), r(2), 16)),
    ("sturh w1, [x2, #-2]", |a| a.str(Width::H, r(1), r(2), -2)),
    ("ldrsb x1, [x2, #1]", |a| a.ldrs(Width::B, X, r(1), r(2), 1)),
    ("ldrsh w1, [x2, #2]", |a| a.ldrs(Width::H, W, r(1), r(2), 2)),
    ("ldrsw x1, [x2, #4]", |a| a.ldrs(Width::W, X, r(1), r(2), 4)),
    ("str x1, [sp, #-16]!", |a| a.ldst_index(false, Width::X, r(1), SP, -16, true)),
    ("ldr x1, [sp], #16", |a| a.ldst_index(true, Width::X, r(1), SP, 16, false)),
    ("ldr w1, [x2, w3, uxtw]", |a| a.ldst_reg(true, Width::W, false, false, r(1), r(2), r(3), Extend::Uxtw, false)),
    ("ldrb w1, [x2, w3, uxtw]", |a| a.ldst_reg(true, Width::B, false, false, r(1), r(2), r(3), Extend::Uxtw, false)),
    ("strh w1, [x2, x3, lsl #1]", |a| a.ldst_reg(false, Width::H, false, false, r(1), r(2), r(3), Extend::Uxtx, true)),
    ("ldrsh x1, [x2, w3, sxtw]", |a| a.ldst_reg(true, Width::H, true, true, r(1), r(2), r(3), Extend::Sxtw, false)),
    ("ldp x1, x2, [x3, #16]", |a| a.ldp(X, r(1), r(2), r(3), 16)),
    ("stp w1, w2, [x3, #-8]", |a| a.stp(W, r(1), r(2), r(3), -8)),
    ("stp x29, x30, [sp, #-16]!", |a| a.stp_pre(X, r(29), r(30), SP, -16)),
    ("ldp x29, x30, [sp], #16", |a| a.ldp_post(X, r(29), r(30), SP, 16)),
    ("ldar w1, [x2]", |a| a.ldar(Width::W, r(1), r(2))),
    ("stlr x1, [x2]", |a| a.stlr(Width::X, r(1), r(2))),
    ("ldaxrb w1, [x2]", |a| a.ldaxr(Width::B, r(1), r(2))),
    ("stlxr w3, w1, [x2]", |a| a.stlxr(Width::W, r(3), r(1), r(2))),
    ("casal w1, w2, [x3]", |a| a.casal(Width::W, r(1), r(2), r(3))),
    ("casalh w1, w2, [x3]", |a| a.casal(Width::H, r(1), r(2), r(3))),
    ("ldaddal w1, w2, [x3]", |a| a.ldaddal(Width::W, r(1), r(2), r(3))),
    ("ldclralb w1, w2, [x3]", |a| a.ldclral(Width::B, r(1), r(2), r(3))),
    ("ldeoral x1, x2, [x3]", |a| a.ldeoral(Width::X, r(1), r(2), r(3))),
    ("ldsetalh w1, w2, [x3]", |a| a.ldsetal(Width::H, r(1), r(2), r(3))),
    ("swpal w1, w2, [x3]", |a| a.swpal(Width::W, r(1), r(2), r(3))),
    ("1: b 1b", |a| {
        let l = a.label();
        a.bind(l);
        a.b(l)
    }),
    ("b 1f\nnop\n1: nop", |a| {
        let l = a.label();
        a.b(l);
        a.nop();
        a.bind(l);
        a.nop()
    }),
    ("1: nop\nbl 1b", |a| {
        let l = a.label();
        a.bind(l);
        a.nop();
        a.bl(l)
    }),
    ("b.ne 1f\nnop\nnop\n1: nop", |a| {
        let l = a.label();
        a.b_cond(Cond::Ne, l);
        a.nop();
        a.nop();
        a.bind(l);
        a.nop()
    }),
    ("1: cbz w1, 1b", |a| {
        let l = a.label();
        a.bind(l);
        a.cbz(W, r(1), l)
    }),
    ("cbnz x2, 1f\n1: nop", |a| {
        let l = a.label();
        a.cbnz(X, r(2), l);
        a.bind(l);
        a.nop()
    }),
    ("tbz w1, #3, 1f\n1: nop", |a| {
        let l = a.label();
        a.tbz(r(1), 3, l);
        a.bind(l);
        a.nop()
    }),
    ("1: tbnz x1, #40, 1b", |a| {
        let l = a.label();
        a.bind(l);
        a.tbnz(r(1), 40, l)
    }),
    ("adr x1, 1f\nnop\n1: nop", |a| {
        let l = a.label();
        a.adr(r(1), l);
        a.nop();
        a.bind(l);
        a.nop()
    }),
    ("br x16", |a| a.br(r(16))),
    ("blr x17", |a| a.blr(r(17))),
    ("ret", |a| a.ret()),
    ("eret", |a| a.eret()),
    ("svc #0x80", |a| a.svc(0x80)),
    ("hvc #7", |a| a.hvc(7)),
    ("brk #1", |a| a.brk(1)),
    ("isb", |a| a.isb()),
    ("dmb ish", |a| a.dmb(0xb)),
    ("dsb sy", |a| a.dsb(0xf)),
    ("mrs x1, esr_el1", |a| a.mrs(r(1), sysreg::ESR_EL1)),
    ("msr vbar_el1, x2", |a| a.msr(sysreg::VBAR_EL1, r(2))),
    ("mrs x3, nzcv", |a| a.mrs(r(3), sysreg::NZCV)),
    ("msr tpidr_el0, x4", |a| a.msr(sysreg::TPIDR_EL0, r(4))),
    ("tlbi vae1, x5", |a| a.tlbi(0b000_0111_001, r(5))),
];

fn assemble(text: &str) -> Vec<u32> {
    let dir = std::env::temp_dir().join(format!("a64-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("t.s");
    let obj = dir.join("t.o");
    std::fs::write(&src, format!(".arch armv8.6-a+lse+flagm\n.text\n{text}\n")).unwrap();
    let out = Command::new("/usr/bin/clang").args(["-c", "-arch", "arm64", "-o"]).arg(&obj).arg(&src).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = Command::new("xcrun").args(["llvm-objdump", "-d", "--no-leading-addr"]).arg(&obj).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let words = text
        .lines()
        .filter_map(|l| {
            let first = l.split_whitespace().next()?;
            if first.len() == 8 && first.chars().all(|c| c.is_ascii_hexdigit()) {
                u32::from_str_radix(first, 16).ok()
            } else {
                None
            }
        })
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    words
}

#[test]
fn encodings_match_clang() {
    let text: Vec<&str> = CASES.iter().map(|c| c.0).collect();
    let expected = assemble(&text.join("\n"));
    let mut got = Vec::new();
    let mut spans = Vec::new();
    for (name, f) in CASES {
        let mut a = Asm::new();
        f(&mut a);
        let words = a.finish();
        spans.push((*name, got.len(), words.len()));
        got.extend(words);
    }
    assert_eq!(got.len(), expected.len());
    let mut bad = Vec::new();
    for (name, start, len) in spans {
        if got[start..start + len] != expected[start..start + len] {
            bad.push(format!("{name:?}: got {:08x?} want {:08x?}", &got[start..start + len], &expected[start..start + len]));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn bitmask_immediates_round_trip() {
    fn decode(enc: u32) -> u64 {
        let n = (enc >> 12) & 1;
        let immr = (enc >> 6) & 0x3f;
        let imms = enc & 0x3f;
        let len = 31 - ((n << 6) | (!imms & 0x3f)).leading_zeros();
        let esize = 1u32 << len;
        let levels = esize - 1;
        let s = imms & levels;
        let rr = immr & levels;
        let welem = if s + 1 == 64 { u64::MAX } else { (1u64 << (s + 1)) - 1 };
        let emask = if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 };
        let elem = ((welem >> rr) | (welem << ((esize - rr) % esize))) & emask;
        (0..64).step_by(esize as usize).fold(0, |v, i| v | elem << i)
    }
    let mut checked = 0;
    for esize in [2u32, 4, 8, 16, 32, 64] {
        let emask = if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 };
        for ones in 1..esize {
            let base = (1u64 << ones) - 1;
            for rot in 0..esize {
                let elem = ((base << rot) | (base >> ((esize - rot) % esize))) & emask;
                let v = (0..64).step_by(esize as usize).fold(0, |v, i| v | elem << i);
                let enc = encode_bitmask(v, X).unwrap();
                assert_eq!(decode(enc), v, "{v:#x}");
                checked += 1;
            }
        }
    }
    assert!(checked > 5000);
    assert_eq!(encode_bitmask(0, X), None);
    assert_eq!(encode_bitmask(0x1234, X), None);
    assert_eq!(encode_bitmask(0xffff_ffff, W), None);
}

#[test]
fn mov_imm_picks_short_sequences() {
    let cases = [
        (W, 0u64, 1),
        (W, 0xffff_ffff, 1),
        (X, 0x1234, 1),
        (X, 0xffff_ffff_ffff_1234, 1),
        (X, 0x00ff_00ff_00ff_00ff, 1),
        (X, 0x1234_5678_9abc_def0, 4),
        (W, 0x8000_0001, 1),
    ];
    for (sz, v, len) in cases {
        let mut a = Asm::new();
        a.mov_imm(sz, r(0), v);
        assert_eq!(a.code.len(), len, "{v:#x}");
    }
}

//! Runs SingleStepTests MOO files against the interpreter.
//!
//! Usage: `cputest [-v N] [--only HEX] <file.MOO.gz | directory>...`

mod moo;

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use moo::{R_CR0, R_CR3, R_CS, R_DR6, R_DR7, R_EAX, R_EFLAGS, R_EIP, REG_NAMES, Regs};
use x86::bus::Bus;
use x86::cpu::{Cpu, Segment};
use x86::interp::{Exit, step};

const ADDR_MASK: u32 = 0x00ff_ffff;
const EFLAGS_MASK: u32 = 0x0003_7fd5;

struct TestBus {
    mem: Vec<u8>,
    touched: Vec<u32>,
}

impl Bus for TestBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.mem[(addr & ADDR_MASK) as usize]
    }

    fn write8(&mut self, addr: u32, value: u8) {
        let a = addr & ADDR_MASK;
        self.touched.push(a);
        self.mem[a as usize] = value;
    }

    fn io_read(&mut self, _port: u16, size: u8) -> u32 {
        match size {
            1 => 0xff,
            2 => 0xffff,
            _ => 0xffff_ffff,
        }
    }

    fn io_write(&mut self, _port: u16, _size: u8, _value: u32) {}
}

impl TestBus {
    fn clear(&mut self) {
        for a in self.touched.drain(..) {
            self.mem[a as usize] = 0;
        }
    }
}

fn load_cpu(regs: &Regs) -> Cpu {
    let mut cpu = Cpu::new();
    let get = |i: usize| regs[i].unwrap_or(0);
    cpu.cr0 = get(R_CR0);
    cpu.cr3 = get(R_CR3);
    for r in 0..8 {
        // MOO order is eax ebx ecx edx esi edi ebp esp; x86 order is eax ecx edx ebx esp ebp esi edi.
        const MAP: [usize; 8] = [0, 3, 1, 2, 6, 7, 5, 4];
        cpu.regs[MAP[r]] = get(R_EAX + r);
    }
    // MOO order: cs ds es fs gs ss; x86 order: es cs ss ds fs gs.
    const SEG_MAP: [usize; 6] = [1, 3, 0, 4, 5, 2];
    for (i, &seg) in SEG_MAP.iter().enumerate() {
        cpu.segs[seg] = Segment::real(get(R_CS + i) as u16);
    }
    cpu.segs[1].attr |= x86::cpu::attr::CODE;
    cpu.eip = get(R_EIP);
    cpu.eflags = get(R_EFLAGS) & EFLAGS_MASK | 2;
    cpu.dr[6] = get(R_DR6);
    cpu.dr[7] = get(R_DR7);
    cpu
}

fn cpu_regs(cpu: &Cpu) -> [u32; 20] {
    let s = |i: usize| cpu.segs[i].selector as u32;
    let r = &cpu.regs;
    [
        cpu.cr0, cpu.cr3, r[0], r[3], r[1], r[2], r[6], r[7], r[5], r[4], s(1), s(3), s(0), s(4), s(5), s(2), cpu.eip,
        cpu.eflags, cpu.dr[6], cpu.dr[7],
    ]
}

/// Flags the SDM leaves undefined for an opcode (file name without 66/67 prefixes).
fn undefined_flags(op: &str) -> u32 {
    use x86::cpu::flags::*;
    let base = op.split('.').next().unwrap_or(op);
    let group = op.split('.').nth(1);
    match (base, group) {
        ("0FA3" | "0FAB" | "0FB3" | "0FBB" | "0FBA", _) => OF | SF | ZF | AF | PF,
        ("0FAF" | "69" | "6B", _) | ("F6" | "F7", Some("4" | "5")) => SF | ZF | AF | PF,
        ("F6" | "F7", Some("6" | "7")) => ARITH,
        ("0FBC" | "0FBD", _) => CF | OF | SF | AF | PF,
        ("0FA4" | "0FA5" | "0FAC" | "0FAD", _) => OF | AF | CF,
        ("C0" | "C1" | "D0" | "D1" | "D2" | "D3", _) => OF | AF,
        ("D4" | "D5", _) => OF | AF | CF,
        ("27" | "2F", _) => OF,
        ("37" | "3F", _) => OF | SF | ZF | PF,
        _ => 0,
    }
}

struct Outcome {
    pass: bool,
    detail: String,
}

fn run_test(bus: &mut TestBus, file_masks: &Regs, undefined: u32, t: &moo::Test) -> Outcome {
    bus.clear();
    for &(a, v) in &t.initial.ram {
        bus.write8(a, v);
    }
    let mut cpu = load_cpu(&t.initial.regs);
    let mut exit = Exit::Continue;
    for _ in 0..4096 {
        exit = step(&mut cpu, bus);
        if exit != Exit::Continue {
            break;
        }
    }
    let mut detail = String::new();
    if exit != Exit::Halted {
        detail += &format!("  did not halt: {exit:?}\n");
    }
    let got = cpu_regs(&cpu);
    for i in 0..20 {
        let Some(want) = t.fin.regs[i].or(t.initial.regs[i]) else { continue };
        let mut m = t.fin.masks[i].or(file_masks[i]).unwrap_or(u32::MAX);
        if (10..16).contains(&i) {
            m &= 0xffff;
        }
        if i == R_EFLAGS {
            m &= EFLAGS_MASK & !undefined;
        }
        if i == R_DR6 || i == R_DR7 || i == R_CR0 {
            continue;
        }
        if got[i] & m != want & m {
            detail += &format!("  {:>6}: got {:#010x} want {:#010x} (mask {m:#x})\n", REG_NAMES[i], got[i], want);
        }
    }
    let flag_addr = t.exception.map(|(_, a)| a & ADDR_MASK);
    for &(a, v) in &t.fin.ram {
        let g = bus.read8(a);
        let m = match flag_addr {
            Some(f) if a == f => !undefined as u8,
            Some(f) if a == f + 1 => !(undefined >> 8) as u8,
            _ => 0xff,
        };
        if g & m != v & m {
            detail += &format!("  mem[{a:#08x}]: got {g:#04x} want {v:#04x}\n");
        }
    }
    Outcome { pass: detail.is_empty(), detail }
}

/// Tests that use the 386-only "scaled base without index" SIB forms, which
/// later CPUs (and we) do not emulate.
fn invalid_sib(t: &moo::Test) -> bool {
    let insn = x86::decode::decode::<()>(false, |i| t.bytes.get(i as usize).copied().ok_or(()));
    match insn {
        Ok(i) => i.addr32 && i.modrm >> 6 != 3 && i.modrm & 7 == 4 && i.mem.is_some_and(|m| m.index == x86::decode::NO_REG && m.scale != 0),
        Err(_) => false,
    }
}

/// 16-bit SHLD/SHRD with a masked count above 16 leaves the result undefined.
fn undefined_double_shift(t: &moo::Test) -> bool {
    let Ok(i) = x86::decode::decode::<()>(false, |n| t.bytes.get(n as usize).copied().ok_or(())) else { return false };
    if i.op32 || !matches!(i.op, 0x1a4 | 0x1a5 | 0x1ac | 0x1ad) {
        return false;
    }
    let count = if i.op & 1 == 0 { i.imm } else { t.initial.regs[moo::R_EAX + 2].unwrap_or(0) & 0xff };
    count & 0x1f > 16
}

fn collect(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(p)
                .expect("read dir")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.to_string_lossy().ends_with(".MOO.gz") || p.to_string_lossy().ends_with(".MOO"))
                .collect();
            entries.sort();
            out.extend(entries);
        } else {
            out.push(p.clone());
        }
    }
    out
}

fn load_revocations(files: &[PathBuf]) -> HashSet<String> {
    let mut set = HashSet::new();
    let dirs: HashSet<_> = files.iter().filter_map(|f| f.parent()?.parent().map(Path::to_path_buf)).collect();
    for d in dirs {
        if let Ok(s) = std::fs::read_to_string(d.join("revocation_list.txt")) {
            set.extend(s.lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()));
        }
    }
    set
}

fn main() {
    let mut verbose = 0usize;
    let mut only: Option<String> = None;
    let mut paths = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-v" => verbose = args.next().and_then(|v| v.parse().ok()).unwrap_or(3),
            "--only" => only = args.next().map(|s| s.to_uppercase()),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    let files = collect(&paths);
    let revoked = load_revocations(&files);
    let mut bus = TestBus { mem: vec![0; (ADDR_MASK + 1) as usize], touched: Vec::new() };
    let (mut total, mut passed) = (0usize, 0usize);
    let mut failing_files = Vec::new();

    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().replace(".MOO.gz", "").replace(".MOO", "");
        if only.as_ref().is_some_and(|o| !name.starts_with(o.as_str())) {
            continue;
        }
        let raw = std::fs::read(&path).expect("read test file");
        let data = if path.extension().is_some_and(|e| e == "gz") {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out).expect("gunzip");
            out
        } else {
            raw
        };
        let file = match moo::parse(&data) {
            Ok(f) => f,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        let undefined = undefined_flags(name.trim_start_matches("67").trim_start_matches("66"));
        let (mut fp, mut ft, mut shown) = (0, 0, 0);
        for t in &file.tests {
            if revoked.contains(&moo::hash_hex(&t.hash)) || invalid_sib(t) || undefined_double_shift(t) {
                continue;
            }
            ft += 1;
            let o = run_test(&mut bus, &file.masks, undefined, t);
            if o.pass {
                fp += 1;
            } else if shown < verbose {
                shown += 1;
                let bytes: Vec<String> = t.bytes.iter().map(|b| format!("{b:02x}")).collect();
                println!("{name} #{} `{}` [{}] exc={:?}\n{}", t.index, t.name, bytes.join(" "), t.exception, o.detail);
                let init: Vec<String> = t.initial.regs.iter().zip(REG_NAMES).filter_map(|(v, n)| v.map(|v| format!("{n}={v:x}"))).collect();
                println!("  initial: {}", init.join(" "));
            }
        }
        total += ft;
        passed += fp;
        if fp != ft {
            failing_files.push(format!("{name}: {fp}/{ft}"));
        }
    }
    for f in &failing_files {
        println!("FAIL {f}");
    }
    println!("{passed}/{total} tests passed, {} files with failures", failing_files.len());
}

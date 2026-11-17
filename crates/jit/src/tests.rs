use super::*;
use x86::cpu::{Segment, attr, cr0};

const RAM: usize = 4 << 20;

/// Hypervisor.framework allows one VM per process.
static VM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct FlatBus {
    ram: GuestMemory,
}

impl Bus for FlatBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.ram.as_slice().get(addr as usize).copied().unwrap_or(0xff)
    }

    fn write8(&mut self, addr: u32, value: u8) {
        if let Some(b) = self.ram.as_mut_slice().get_mut(addr as usize) {
            *b = value;
        }
    }

    fn io_read(&mut self, _port: u16, _size: u8) -> u32 {
        0xffff_ffff
    }

    fn io_write(&mut self, _port: u16, _size: u8, _value: u32) {}
}

fn flat_cpu(eip: u32, esp: u32) -> Cpu {
    let mut cpu = Cpu::new();
    cpu.cr0 = cr0::PE | cr0::ET;
    let data = Segment {
        selector: 0x10,
        attr: attr::P | attr::S | attr::RW | attr::ACCESSED | attr::DB | attr::G,
        base: 0,
        limit: 0xffff_ffff,
    };
    cpu.segs = [data; 6];
    cpu.segs[CS] = Segment { selector: 0x08, attr: data.attr | attr::CODE, ..data };
    cpu.eip = eip;
    cpu.regs[4] = esp;
    cpu
}

fn bus_with(code_at: u32, code: &[u8]) -> FlatBus {
    let mut bus = FlatBus { ram: GuestMemory::new(RAM).unwrap() };
    bus.ram.as_mut_slice()[code_at as usize..code_at as usize + code.len()].copy_from_slice(code);
    bus
}

fn run_interp(cpu: &mut Cpu, bus: &mut FlatBus) {
    for _ in 0..100_000 {
        if interp::step(cpu, bus) != interp::Exit::Continue || cpu.halted {
            return;
        }
    }
    panic!("interpreter did not halt");
}

fn run_jit(cpu: &mut Cpu, bus: &mut FlatBus) -> Stats {
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus.ram.as_ptr(), RAM).unwrap() };
    let kicker = jit.kicker();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let kicks = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(1));
            kicker.kick();
        }
    });
    let _join = scopeguard(move || {
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = kicks.join();
    });
    for _ in 0..1000 {
        if !supported(cpu) || cpu.halted {
            break;
        }
        match jit.run(cpu, bus).unwrap() {
            RunExit::Interp(_) if cpu.halted => break,
            RunExit::Interp(e) if std::env::var_os("DBG").is_some() => eprintln!("exit {e:?} eip {:#x} cr2 {:#x} esp {:#x} cs {:#x}", cpu.eip, cpu.cr2, cpu.regs[4], cpu.segs[1].selector),
            _ => {}
        }
    }
    assert!(cpu.halted, "JIT run did not reach HLT; eip {:#x} {:?}", cpu.eip, jit.stats);
    jit.stats
}

/// Runs `f` when dropped, also while unwinding.
fn scopeguard(f: impl FnOnce()) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    Guard(Some(f))
}

const ARITH_MASK: u32 = x86::cpu::flags::ARITH | x86::cpu::flags::IF;

fn compare(code: &[u8], setup: impl Fn(&mut Cpu)) -> Stats {
    let mut a = flat_cpu(0x1000, 0x8000);
    setup(&mut a);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, code);
    let mut bus_b = bus_with(0x1000, code);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs, "registers");
    assert_eq!(a.eip, b.eip);
    assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "flags {:#x} vs {:#x}", a.eflags, b.eflags);
    assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "memory differs");
    stats
}

#[test]
fn loop_with_memory_and_stack() {
    let code = [
        0xb8, 0x05, 0, 0, 0, // mov eax, 5
        0xbb, 0x07, 0, 0, 0, // mov ebx, 7
        0x01, 0xd8, // add eax, ebx
        0xb9, 0x0a, 0, 0, 0, // mov ecx, 10
        0x01, 0xca, // loop: add edx, ecx
        0x49, // dec ecx
        0x75, 0xfb, // jnz loop
        0x50, // push eax
        0x5e, // pop esi
        0x89, 0x35, 0x00, 0x20, 0, 0, // mov [0x2000], esi
        0x8b, 0x3d, 0x00, 0x20, 0, 0, // mov edi, [0x2000]
        0x83, 0xff, 0x0c, // cmp edi, 12
        0x0f, 0x94, 0xc0, // sete al
        0xf4, // hlt
    ];
    let stats = compare(&code, |_| {});
    assert!(stats.blocks >= 3);
    assert_eq!(stats.interp_exits, 1);
}

#[test]
fn flags_from_8bit_and_carry_chains() {
    let code = [
        0xb0, 0xf0, // mov al, 0xf0
        0x04, 0x20, // add al, 0x20
        0x0f, 0x92, 0xc3, // setb bl
        0x80, 0xfc, 0x01, // cmp ah, 1
        0x0f, 0x97, 0xc7, // seta bh
        0xb9, 0xff, 0xff, 0xff, 0xff, // mov ecx, -1
        0x83, 0xc1, 0x01, // add ecx, 1
        0x11, 0xca, // adc edx, ecx
        0x19, 0xf6, // sbb esi, esi
        0x41, // inc ecx (keeps CF)
        0xd1, 0xe7, // shl edi, 1
        0xc1, 0xef, 0x03, // shr edi, 3
        0x0f, 0xaf, 0xc1, // imul eax, ecx
        0x84, 0xc0, // test al, al
        0xf4,
    ];
    compare(&code, |c| {
        c.regs[7] = 0x8000_0001;
        c.regs[2] = 0x7fff_ffff;
    });
}

#[test]
fn call_and_return() {
    let code = [
        0xe8, 0x06, 0, 0, 0, // call +6
        0x89, 0xc3, // mov ebx, eax
        0xf4, // hlt
        0x90, 0x90, 0x90, // padding
        0xb8, 0x2a, 0, 0, 0, // mov eax, 42
        0xc3, // ret
    ];
    compare(&code, |_| {});
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }

    /// A general register other than ESP.
    fn reg(&mut self) -> u8 {
        let r = self.below(7) as u8;
        if r >= 4 { r + 1 } else { r }
    }
}

fn modrm_rr(reg: u8, rm: u8) -> u8 {
    0xc0 | reg << 3 | rm
}

fn random_insn(rng: &mut Rng, out: &mut Vec<u8>) {
    let mem = |out: &mut Vec<u8>, reg: u8, slot: u32| {
        out.push(reg << 3 | 5);
        out.extend((0x3000 + slot * 4).to_le_bytes());
    };
    match rng.below(38) {
        36 | 37 => match rng.below(9) {
            0 => out.extend([0x0f, 0xc8 + rng.reg()]),
            1 => {
                let kind = [0, 1, 4, 5, 6, 7][rng.below(6) as usize];
                match rng.below(3) {
                    0 => out.extend([0xc0, modrm_rr(kind, rng.below(8) as u8), rng.below(40) as u8]),
                    1 => out.extend([0xd0, modrm_rr(kind, rng.below(8) as u8)]),
                    _ => out.extend([0xd2, modrm_rr(kind, rng.below(8) as u8)]),
                }
            }
            2 => {
                let kind = [0, 1, 4, 5, 6, 7][rng.below(6) as usize];
                match rng.below(3) {
                    0 => out.extend([0x66, 0xc1, modrm_rr(kind, rng.reg()), rng.below(40) as u8]),
                    1 => out.extend([0x66, 0xd1, modrm_rr(kind, rng.reg())]),
                    _ => out.extend([0x66, 0xd3, modrm_rr(kind, rng.reg())]),
                }
            }
            3 => {
                let r = rng.reg();
                out.extend([0x9c, 0x58 + r]);
            }
            4 => {
                let r = rng.reg();
                out.extend([0x81, modrm_rr(4, r)]);
                out.extend(0x0cd5u32.to_le_bytes());
                out.extend([0x50 + r, 0x9d]);
            }
            5 => {
                if rng.below(2) == 0 {
                    out.extend([0x0f, 0xba, modrm_rr(4 + rng.below(4) as u8, rng.reg()), rng.next() as u8]);
                } else {
                    out.extend([0x66, 0x0f, 0xba, modrm_rr(4 + rng.below(4) as u8, rng.reg()), rng.next() as u8]);
                }
            }
            6 => {
                out.extend([0x0f, 0xba, (4 + rng.below(4) as u8) << 3 | 5]);
                out.extend((0x3000 + rng.below(4) * 4).to_le_bytes());
                out.push(rng.next() as u8);
            }
            7 => out.push([0xf5, 0xf8, 0xf9][rng.below(3) as usize]),
            _ => {
                out.extend([0x0f, 0x18, (rng.below(4) as u8) << 3 | 5]);
                out.extend((0x3000u32).to_le_bytes());
            }
        },
        34 | 35 => match rng.below(10) {
            0 => out.extend([0xc1, modrm_rr(rng.below(2) as u8, rng.reg()), rng.below(40) as u8]),
            1 => out.extend([0xd1, modrm_rr(rng.below(2) as u8, rng.reg())]),
            2 => out.extend([0xd3, modrm_rr(rng.below(2) as u8, rng.reg())]),
            3 => {
                if rng.below(2) == 0 {
                    out.extend([0xf6, modrm_rr(2 + rng.below(2) as u8, rng.below(8) as u8)]);
                } else {
                    out.extend([0x66, 0xf7, modrm_rr(2 + rng.below(2) as u8, rng.reg())]);
                }
            }
            4 => {
                if rng.below(2) == 0 {
                    out.extend([0x66, 0x40 + rng.reg() + 8 * rng.below(2) as u8]);
                } else {
                    out.extend([0x66, 0xff, modrm_rr(rng.below(2) as u8, rng.reg())]);
                }
            }
            5 | 6 => {
                let op = if rng.below(2) == 0 { 0xc1 } else { 0xb1 };
                match rng.below(3) {
                    0 => out.extend([0x0f, op, modrm_rr(rng.reg(), rng.reg())]),
                    1 => out.extend([0x0f, op - 1, modrm_rr(rng.below(8) as u8, rng.below(8) as u8)]),
                    _ => {
                        out.extend([0x0f, op]);
                        mem(out, rng.reg(), rng.below(4));
                    }
                }
            }
            7 => {
                out.extend([0x0f, 0xc7, 1 << 3 | 5]);
                out.extend((0x3000 + rng.below(2) * 8).to_le_bytes());
            }
            8 => {
                let op = [0xa4, 0xa5, 0xac, 0xad][rng.below(4) as usize];
                out.extend([0x0f, op]);
                if rng.below(2) == 0 {
                    out.push(modrm_rr(rng.reg(), rng.reg()));
                } else {
                    mem(out, rng.reg(), rng.below(4));
                }
                if op & 1 == 0 {
                    out.push(rng.below(40) as u8);
                }
            }
            _ => {
                out.extend([0x0f, 0x2b, (rng.below(8) as u8) << 3 | 5]);
                out.extend((0x3000 + rng.below(4) * 16).to_le_bytes());
            }
        },
        30 => {
            out.extend([0xbe]);
            out.extend((0x3000 + rng.below(16)).to_le_bytes());
            out.extend([0xbf]);
            out.extend((0x3040 + rng.below(16)).to_le_bytes());
            out.extend([0xb9]);
            out.extend(rng.below(6).to_le_bytes());
            let prefix = [0xf3, 0xf2, 0x90][rng.below(3) as usize];
            let op = [0xa4, 0xa5, 0xaa, 0xab, 0xa6, 0xa7, 0xae, 0xaf, 0xac][rng.below(9) as usize];
            out.extend([prefix, op]);
        }
        31 => {
            out.extend([0x31, 0xd2]);
            out.extend([0xbb]);
            out.extend((rng.next() | 1).to_le_bytes());
            out.extend([0xf7, 0xf3]);
        }
        32 => {
            out.extend([0x99, 0xbb]);
            out.extend(((rng.next() >> 1) | 1).to_le_bytes());
            out.extend([0xf7, 0xfb]);
        }
        33 => match rng.below(4) {
            0 => out.push([0xfc, 0xfd, 0xfc][rng.below(3) as usize]),
            1 => out.extend([0xfe, modrm_rr(rng.below(2) as u8, rng.below(8) as u8)]),
            2 => {
                out.extend([0x66, 0xf7, modrm_rr(0, rng.reg())]);
                out.extend((rng.next() as u16).to_le_bytes());
            }
            _ => {
                out.extend([0xf0, 0x01]);
                out.push(rng.reg() << 3 | 5);
                out.extend((0x3000 + rng.below(4) * 4).to_le_bytes());
            }
        },
        20 => {
            let ops = [0x01, 0x03, 0x09, 0x0b, 0x11, 0x13, 0x19, 0x1b, 0x21, 0x23, 0x29, 0x2b, 0x31, 0x33, 0x39, 0x3b, 0x85, 0x89, 0x8b, 0x87];
            out.extend([0x66, ops[rng.below(ops.len() as u32) as usize], modrm_rr(rng.reg(), rng.reg())]);
        }
        21 => {
            let op = [0, 1, 4, 5, 6, 7][rng.below(6) as usize];
            if rng.below(2) == 0 {
                out.extend([0x66, 0x81, modrm_rr(op, rng.reg())]);
                out.extend((rng.next() as u16).to_le_bytes());
            } else {
                out.extend([0x66, 0x83, modrm_rr(op, rng.reg()), rng.next() as u8]);
            }
        }
        22 => {
            out.extend([0x66, 0xb8 + rng.reg()]);
            out.extend((rng.next() as u16).to_le_bytes());
        }
        23 => out.extend([0x89, 0xe5, 0xc9]),
        24 => {
            out.push([0xa1, 0xa3, 0xa0, 0xa2][rng.below(4) as usize]);
            out.extend((0x3000 + rng.below(4) * 4).to_le_bytes());
        }
        25 => out.extend([0xd3, modrm_rr([4, 5, 7][rng.below(3) as usize], rng.reg())]),
        26 => out.extend([[0x86, 0x87][rng.below(2) as usize], modrm_rr(rng.reg(), rng.reg())]),
        27 => match rng.below(3) {
            0 => out.extend([0xf7, modrm_rr(4, rng.reg())]),
            1 => out.extend([0x6b, modrm_rr(rng.reg(), rng.reg()), rng.next() as u8]),
            _ => {
                out.extend([0x69, modrm_rr(rng.reg(), rng.reg())]);
                out.extend(rng.next().to_le_bytes());
            }
        },
        28 => out.push([0x98, 0x99, 0xfa, 0xfb][rng.below(4) as usize]),
        29 => out.extend([0xe3, 0x01, 0x40 + rng.reg()]),
        0 | 1 => {
            let ops = [0x01, 0x03, 0x09, 0x0b, 0x11, 0x13, 0x19, 0x1b, 0x21, 0x23, 0x29, 0x2b, 0x31, 0x33, 0x39, 0x3b];
            out.push(ops[rng.below(16) as usize]);
            out.push(modrm_rr(rng.reg(), rng.reg()));
        }
        2 => {
            let op = rng.below(8) as u8;
            if rng.below(2) == 0 {
                out.extend([0x81, modrm_rr(op, rng.reg())]);
                out.extend(rng.next().to_le_bytes());
            } else {
                out.extend([0x83, modrm_rr(op, rng.reg()), rng.next() as u8]);
            }
        }
        3 => {
            let ops = [0x01, 0x03, 0x29, 0x2b, 0x31, 0x39, 0x89, 0x8b];
            out.push(ops[rng.below(8) as usize]);
            mem(out, rng.reg(), rng.below(4));
        }
        4 => {
            let ops = [0x00, 0x02, 0x08, 0x0a, 0x10, 0x12, 0x18, 0x1a, 0x20, 0x22, 0x28, 0x2a, 0x30, 0x32, 0x38, 0x3a, 0x84, 0x88, 0x8a];
            out.push(ops[rng.below(ops.len() as u32) as usize]);
            out.push(modrm_rr(rng.below(8) as u8, rng.below(8) as u8));
        }
        5 => {
            let op = rng.below(8) as u8;
            out.extend([0x80, modrm_rr(op, rng.below(8) as u8), rng.next() as u8]);
        }
        6 => out.push(if rng.below(2) == 0 { 0x40 } else { 0x48 } + rng.reg()),
        7 => {
            let kind = [4, 5, 7][rng.below(3) as usize];
            if rng.below(2) == 0 {
                out.extend([0xc1, modrm_rr(kind, rng.reg()), rng.below(40) as u8]);
            } else {
                out.extend([0xd1, modrm_rr(kind, rng.reg())]);
            }
        }
        8 => {
            out.extend([0x85, modrm_rr(rng.reg(), rng.reg())]);
        }
        9 => {
            out.extend([0x0f, 0xaf, modrm_rr(rng.reg(), rng.reg())]);
        }
        10 => {
            let op = [0xb6, 0xb7, 0xbe, 0xbf][rng.below(4) as usize];
            out.extend([0x0f, op, modrm_rr(rng.reg(), rng.below(8) as u8)]);
        }
        11 => {
            let cc = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13, 14, 15][rng.below(14) as usize];
            if rng.below(2) == 0 {
                out.extend([0x0f, 0x90 + cc, modrm_rr(0, rng.below(8) as u8)]);
            } else {
                out.extend([0x0f, 0x40 + cc, modrm_rr(rng.reg(), rng.reg())]);
            }
        }
        12 => out.extend([0xf7, modrm_rr(2 + rng.below(2) as u8, rng.reg())]),
        13 => {
            let r = rng.reg();
            out.extend([0x50 + rng.reg(), 0x58 + r]);
        }
        14 => {
            let cc = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13, 14, 15][rng.below(14) as usize];
            out.extend([0x70 + cc, 0x01, 0x40 + rng.reg()]);
        }
        15 => {
            out.extend([0x8d, 0x44 | rng.reg() << 3, rng.reg() << 3 | rng.reg(), rng.next() as u8]);
        }
        16 => {
            out.push(0xb8 + rng.reg());
            out.extend(rng.next().to_le_bytes());
        }
        17 => out.extend([0xa8, rng.next() as u8]),
        18 => {
            out.push(0xa9);
            out.extend(rng.next().to_le_bytes());
        }
        _ => {
            out.extend([0xf7, modrm_rr(0, rng.reg())]);
            out.extend(rng.next().to_le_bytes());
        }
    }
}

#[test]
fn random_programs_match_the_interpreter() {
    let mut rng = Rng(0x86_5109);
    let mut interp_exits = 0;
    let mut translated = 0;
    for round in 0..300 {
        let mut code = Vec::new();
        for _ in 0..40 {
            random_insn(&mut rng, &mut code);
        }
        code.push(0xf4);
        let regs: Vec<u32> = (0..8).map(|_| rng.next()).collect();
        let flags = rng.next() & ARITH_MASK;
        let xmm: Vec<u128> = (0..8).map(|_| (rng.next() as u128) << 96 | (rng.next() as u128) << 32 | rng.next() as u128).collect();
        let result = std::panic::catch_unwind(|| {
            compare(&code, |c| {
                for (i, v) in regs.iter().enumerate() {
                    if i != 4 {
                        c.regs[i] = *v;
                    }
                }
                c.eflags |= flags;
                c.cr4 |= x86::cpu::cr4::OSFXSR;
                c.xmm.copy_from_slice(&xmm);
            })
        });
        match result {
            Ok(stats) => {
                interp_exits += stats.interp_exits;
                translated += stats.translated_insns;
            }
            Err(e) => {
                let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
                panic!("round {round} differs: {:?}\ncode: {}", e.downcast_ref::<String>(), hex.join(" "));
            }
        }
    }
    // Besides each final HLT, only string instructions running with DF=1 use the interpreter.
    assert!(interp_exits < 360, "{interp_exits} interpreter exits");
    assert!(translated > 300 * 40);
}

#[test]
fn user_mode_code_matches_the_interpreter() {
    let code = [
        0xb8, 0x05, 0, 0, 0, // mov eax, 5
        0x01, 0xc0, // add eax, eax
        0x50, // push eax
        0x5b, // pop ebx
        0xfa, // cli: #GP at CPL 3 (IOPL 0), handled by the interpreter
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    for seg in &mut a.segs {
        seg.selector |= 3;
        seg.attr |= 3 << 5;
    }
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    for _ in 0..5 {
        interp::step(&mut a, &mut bus_a);
    }
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus_b.ram.as_ptr(), RAM).unwrap() };
    assert!(supported(&b));
    let exit = jit.run(&mut b, &mut bus_b).unwrap();
    assert!(matches!(exit, RunExit::Interp(_)));
    assert_eq!(a.regs, b.regs);
    assert_eq!(a.eip, b.eip);
    assert_eq!(jit.stats.translated_insns, 4);
}

#[test]
fn instruction_across_a_page_end() {
    let mut code = vec![0x90; 0xffd];
    code.extend([0xb8, 0x78, 0x56, 0x34, 0x12, 0x40, 0xf4]);
    let stats = compare(&code, |_| {});
    assert_eq!(stats.interp_exits, 1, "only the final HLT should be interpreted");

    let mut cpu = flat_cpu(0x1000, 0x8000);
    let mut bus = bus_with(0x1000, &code);
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus.ram.as_ptr(), RAM).unwrap() };
    for round in 0..2 {
        cpu.eip = 0x1000;
        cpu.halted = false;
        while !cpu.halted {
            jit.run(&mut cpu, &mut bus).unwrap();
        }
        assert_eq!(cpu.regs[0], if round == 0 { 0x1234_5679 } else { 0x9934_5679 });
        bus.ram.as_mut_slice()[0x2001] = 0x99;
    }
}

#[test]
fn code_rewritten_every_iteration_matches_the_interpreter() {
    let code = [
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0x88, 0x0d, 0x0d, 0x10, 0, 0, // loop: mov [0x100d], cl (imm of the next mov)
        0xb8, 0, 0, 0, 0, // mov eax, imm32
        0x01, 0xc3, // add ebx, eax
        0x49, // dec ecx
        0x75, 0xf0, // jnz loop
        0xf4,
    ];
    let stats = compare(&code, |c| c.regs[3] = 0);
    assert!(stats.stale_blocks > 0, "the page should have switched to self-checking blocks: {stats:?}");
}

#[test]
fn atomics_match_the_interpreter_when_the_compare_succeeds() {
    let programs: [&[u8]; 6] = [
        &[0xa1, 0, 0x30, 0, 0, 0xf0, 0x0f, 0xb1, 0x0d, 0, 0x30, 0, 0, 0xf4],
        &[0xa0, 0, 0x30, 0, 0, 0xf0, 0x0f, 0xb0, 0x2d, 0, 0x30, 0, 0, 0xf4],
        &[0x8b, 0x05, 0, 0x30, 0, 0, 0x8b, 0x15, 4, 0x30, 0, 0, 0xf0, 0x0f, 0xc7, 0x0d, 0, 0x30, 0, 0, 0xf4],
        &[0xf0, 0x0f, 0xc1, 0x1d, 0, 0x30, 0, 0, 0xf4],
        &[0x89, 0xc1, 0x0f, 0xb1, 0xc8, 0xf4],
        &[0x0f, 0xb1, 0xc0, 0xf4],
    ];
    for code in programs {
        compare(code, |c| {
            c.regs[1] = 0x1111_2222;
            c.regs[3] = 0x3333_4444;
            c.regs[5] = 0x5555_6666;
        });
    }
}

/// Identity-maps the first 4 MiB through a page table at 0x11000 (or one PSE
/// page) and turns paging on, so shadow faults go through the in-VM walk.
fn paged(cpu: &mut Cpu, bus: &mut FlatBus, big: bool) {
    let mem = bus.ram.as_mut_slice();
    let pde = if big { 0x87u32 } else { 0x11007 };
    mem[0x10000..0x10004].copy_from_slice(&pde.to_le_bytes());
    for i in 0..1024u32 {
        let pte = (i << 12) | 7;
        let at = 0x11000 + i as usize * 4;
        mem[at..at + 4].copy_from_slice(&pte.to_le_bytes());
    }
    cpu.cr3 = 0x10000;
    cpu.cr4 |= x86::cpu::cr4::PSE | x86::cpu::cr4::OSFXSR;
    cpu.cr0 |= x86::cpu::cr0::PG | x86::cpu::cr0::WP;
}

#[test]
fn random_programs_match_the_interpreter_with_paging() {
    let mut rng = Rng(0x5ad0_77);
    let mut fills = 0;
    for round in 0..200 {
        let mut code = Vec::new();
        for _ in 0..40 {
            random_insn(&mut rng, &mut code);
        }
        code.push(0xf4);
        let regs: Vec<u32> = (0..8).map(|_| rng.next()).collect();
        let big = round % 2 == 1;
        let mut a = flat_cpu(0x1000, 0x8000);
        for (i, v) in regs.iter().enumerate() {
            if i != 4 {
                a.regs[i] = *v;
            }
        }
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        paged(&mut a, &mut bus_a, big);
        paged(&mut b, &mut bus_b, big);
        run_interp(&mut a, &mut bus_a);
        if std::env::var_os("DBG").is_some() {
            eprintln!("round {round} code at 0x4d: {:02x?} interp regs {:08x?}", &code[0x40..0x58], a.regs);
        }
        let stats = run_jit(&mut b, &mut bus_b);
        fills += stats.vm_fills;
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "round {round} flags");
        let (ma, mb) = (bus_a.ram.as_slice(), bus_b.ram.as_slice());
        if let Some(i) = (0..RAM).find(|&i| ma[i] != mb[i]) {
            panic!("round {round}: memory differs at {i:#x}: interp {:#04x} jit {:#04x}\ncode: {}", ma[i], mb[i], hex.join(" "));
        }
    }
    assert!(fills > 0, "the in-VM walk never ran");
}

#[test]
fn untranslatable_instruction_across_a_page_end() {
    let mut code = vec![0x90; 0xfff];
    code.extend([0x0f, 0xa2, 0x40, 0xf4]);
    compare(&code, |_| {});
}

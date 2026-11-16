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
    for _ in 0..1000 {
        if !supported(cpu) || cpu.halted {
            break;
        }
        match jit.run(cpu, bus).unwrap() {
            RunExit::Interp(_) if cpu.halted => break,
            _ => {}
        }
    }
    assert!(cpu.halted, "JIT run did not reach HLT; eip {:#x}", cpu.eip);
    jit.stats
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
    match rng.below(30) {
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
        let result = std::panic::catch_unwind(|| {
            compare(&code, |c| {
                for (i, v) in regs.iter().enumerate() {
                    if i != 4 {
                        c.regs[i] = *v;
                    }
                }
                c.eflags |= flags;
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
    assert_eq!(interp_exits, 300, "only the final HLT should need the interpreter");
    assert!(translated > 300 * 40);
}

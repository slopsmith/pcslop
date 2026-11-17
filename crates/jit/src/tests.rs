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
    run_jit_with(cpu, bus, |_| {}, |_| {})
}

fn run_jit_with(cpu: &mut Cpu, bus: &mut FlatBus, before: impl FnOnce(&mut Jit), after: impl FnOnce(&mut Jit)) -> Stats {
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus.ram.as_ptr(), RAM).unwrap() };
    jit.vm_fill = true;
    before(&mut jit);
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
    after(&mut jit);
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

#[test]
fn rep_ret_runs_translated() {
    let code = [
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0xe8, 0x04, 0, 0, 0, // call +4
        0x49, // dec ecx
        0x75, 0xf8, // jnz -8
        0xf4, // hlt
        0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, // inc eax, past the interpreter's tail
        0xf3, 0xc3, // repz ret
    ];
    let stats = compare(&code, |_| {});
    assert!(stats.interp_exits < 8, "glibc returns with repz ret, so each one leaving translated code is slow: {stats:?}");
}

#[test]
fn one_operand_mul_and_imul_run_translated() {
    let mut code = vec![
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0xc7, 0x05, 0x00, 0x30, 0, 0, 0x07, 0xf0, 0xff, 0xff, // mov dword [0x3000], -4089
        0xb8, 0x39, 0x30, 0, 0, // loop: mov eax, 12345
        0xf7, 0x2d, 0x00, 0x30, 0, 0, // imul dword [0x3000]
        0x01, 0xd6, // add esi, edx
        0xf7, 0xe9, // imul ecx
        0x01, 0xc7, // add edi, eax
        0xf7, 0x25, 0x00, 0x30, 0, 0, // mul dword [0x3000]
        0x01, 0xd3, // add ebx, edx
    ];
    code.extend([0x90; 12]); // past the interpreter's tail
    code.extend([0x49, 0x75, 0xd8, 0xf4]); // dec ecx; jnz loop; hlt
    let stats = compare(&code, |_| {});
    assert!(stats.interp_exits < 8, "one-operand IMUL is common in XP and Linux and must not leave translated code: {stats:?}");
}

#[test]
fn popf_changes_to_ac_and_id_reach_the_cpu() {
    let code = [
        0x9c, // pushfd
        0x81, 0x34, 0x24, 0x00, 0x00, 0x24, 0x00, // xor dword [esp], AC | ID
        0x9d, // popfd
        0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // run on in translated code
        0xf4,
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    run_interp(&mut a, &mut bus_a);
    run_jit(&mut b, &mut bus_b);
    assert_eq!(a.eflags, b.eflags);
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
    match rng.below(40) {
        38 | 39 => match rng.below(7) {
            0 => out.extend([[0x06, 0x0e, 0x1e][rng.below(3) as usize], 0x58 + rng.reg()]),
            1 => out.extend([0x0f, [0xa0, 0xa8][rng.below(2) as usize], 0x58 + rng.reg()]),
            2 => {
                out.extend([0x50 + rng.reg(), 0x8f]);
                if rng.below(2) == 0 {
                    out.push(modrm_rr(0, rng.reg()));
                } else {
                    mem(out, 0, rng.below(4));
                }
            }
            3 => out.extend([0x60, 0x83, 0xc0 + rng.reg(), rng.next() as u8, 0x61]),
            4 => out.push(0x90 + rng.reg()),
            5 => out.extend([0xf7, modrm_rr(4 + rng.below(2) as u8, rng.below(8) as u8)]),
            _ => {
                out.extend([0x0f, 0xbc + rng.below(2) as u8]);
                if rng.below(2) == 0 {
                    out.push(modrm_rr(rng.reg(), rng.reg()));
                } else {
                    mem(out, rng.reg(), rng.below(4));
                }
            }
        },
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
fn rewritten_x87_code_leaves_its_self_checking_block_cleanly() {
    let code = [
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0xd9, 0xee, // fldz
        0x88, 0x0d, 0x0f, 0x10, 0, 0, // loop: mov [0x100f], cl (imm of the next mov)
        0xb8, 0, 0, 0, 0, // mov eax, imm32
        0x01, 0xc3, // add ebx, eax
        0xdb, 0x05, 0x00, 0x30, 0x00, 0x00, // fild dword [0x3000]
        0xdd, 0xd9, // fstp st(1)
        0xff, 0x05, 0x00, 0x30, 0x00, 0x00, // inc dword [0x3000]
        0x49, // dec ecx
        0x75, 0xe2, // jnz loop
        0xf4,
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert!(stats.stale_blocks > 0, "the page should have switched to self-checking blocks: {stats:?}");
    assert_eq!(a.regs, b.regs);
    assert_eq!(a.fpu, b.fpu, "a stale self-checking block must not lose or misplace cached x87 values");
    assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "memory differs");
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

fn random_mmx(rng: &mut Rng, out: &mut Vec<u8>) {
    const REG_OPS: [u8; 39] = [
        0xfc, 0xfd, 0xfe, 0xd4, 0xf8, 0xf9, 0xfa, 0xfb, 0xec, 0xed, 0xdc, 0xdd, 0xe8, 0xe9, 0xd8, 0xd9, 0x74, 0x75, 0x76, 0x64,
        0x65, 0x66, 0xd5, 0xe0, 0xe3, 0xda, 0xde, 0xea, 0xee, 0x60, 0x61, 0x62, 0x63, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0xe5,
    ];
    const MORE_OPS: [u8; 7] = [0xe4, 0xf5, 0xdb, 0xdf, 0xeb, 0xef, 0x6f];
    let mm = rng.below(8) as u8;
    match rng.below(6) {
        0 | 1 => {
            let all: Vec<u8> = REG_OPS.iter().chain(MORE_OPS.iter()).copied().collect();
            let op = all[rng.below(all.len() as u32) as usize];
            out.extend([0x0f, op, modrm_rr(mm, rng.below(8) as u8)]);
        }
        2 => {
            let all: Vec<u8> = REG_OPS.iter().chain(MORE_OPS.iter()).copied().collect();
            let op = all[rng.below(all.len() as u32) as usize];
            out.extend([0x0f, op, mm << 3 | 5]);
            out.extend((0x3000 + rng.below(8) * 8).to_le_bytes());
        }
        3 => match rng.below(4) {
            0 => out.extend([0x0f, 0x6e, modrm_rr(mm, rng.reg())]),
            1 => out.extend([0x0f, 0x7e, modrm_rr(mm, rng.reg())]),
            2 => {
                out.extend([0x0f, 0x7f, mm << 3 | 5]);
                out.extend((0x3000 + rng.below(8) * 8).to_le_bytes());
            }
            _ => out.extend([0x0f, 0x7f, modrm_rr(mm, rng.below(8) as u8)]),
        },
        _ => {
            let (op, sub) = match rng.below(3) {
                0 => (0x71, [2, 4, 6][rng.below(3) as usize]),
                1 => (0x72, [2, 4, 6][rng.below(3) as usize]),
                _ => (0x73, [2, 6][rng.below(2) as usize]),
            };
            out.extend([0x0f, op, modrm_rr(sub, mm), rng.below(70) as u8]);
        }
    }
}

#[test]
fn random_mmx_programs_match_the_interpreter() {
    let mut rng = Rng(0x3d_f00d);
    for round in 0..300 {
        let mut code = Vec::new();
        for _ in 0..30 {
            random_mmx(&mut rng, &mut code);
        }
        code.push(0xf4);
        let regs: Vec<u32> = (0..8).map(|_| rng.next()).collect();
        let mms: Vec<u64> = (0..8).map(|_| (rng.next() as u64) << 32 | rng.next() as u64).collect();
        let mem: Vec<u8> = (0..64).map(|_| rng.next() as u8).collect();
        let mut a = flat_cpu(0x1000, 0x8000);
        for (i, v) in regs.iter().enumerate() {
            if i != 4 {
                a.regs[i] = *v;
            }
        }
        for (i, v) in mms.iter().enumerate() {
            a.fpu.set_mm(i, *v);
        }
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        bus_a.ram.as_mut_slice()[0x3000..0x3040].copy_from_slice(&mem);
        bus_b.ram.as_mut_slice()[0x3000..0x3040].copy_from_slice(&mem);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        run_interp(&mut a, &mut bus_a);
        let stats = run_jit(&mut b, &mut bus_b);
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.fpu, b.fpu, "round {round} MMX state\ncode: {}", hex.join(" "));
        assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "round {round} memory\ncode: {}", hex.join(" "));
        assert!(stats.interp_exits <= 1, "round {round}: {} interpreter exits\ncode: {}", stats.interp_exits, hex.join(" "));
    }
}

/// A ModRM byte and disp32 for `[disp32]` with the given reg field.
fn mem_abs(out: &mut Vec<u8>, op: u8, reg: u8, addr: u32) {
    out.extend([op, reg << 3 | 5]);
    out.extend(addr.to_le_bytes());
}

fn random_x87(rng: &mut Rng, out: &mut Vec<u8>) {
    let i = rng.below(8) as u8;
    let k = rng.below(8);
    match rng.below(24) {
        0 => mem_abs(out, 0xdd, 0, 0x3000 + 8 * k),
        1 => mem_abs(out, 0xd9, 0, 0x3100 + 4 * k),
        2 => mem_abs(out, 0xdb, 0, 0x3200 + 4 * k),
        3 => mem_abs(out, [0xdf, 0xdf][k as usize % 2], [0, 5][k as usize % 2], 0x3300 + 8 * k),
        4 | 5 => {
            let op = [0xd8, 0xdc, 0xde][rng.below(3) as usize];
            let reg = [0, 1, 4, 5, 6, 7][rng.below(6) as usize];
            out.extend([op, 0xc0 | reg << 3 | i]);
        }
        6 => mem_abs(out, [0xdc, 0xd8, 0xda][rng.below(3) as usize], rng.below(8) as u8, 0x3000 + 8 * k),
        7 => out.extend([0xd9, 0xc8 | i]),
        8 => out.extend([0xd9, 0xc0 | i]),
        9 => out.extend([0xdd, [0xd0, 0xd8][rng.below(2) as usize] | i]),
        10 => out.extend([0xd9, [0xe0, 0xe1, 0xee, 0xe8, 0xfa, 0xfc, 0xe4][rng.below(7) as usize]]),
        11 => out.extend([0xd8, 0xd0 | i]),
        12 => out.extend([0xd8, 0xd8 | i]),
        13 => out.extend([[0xde, 0xd9], [0xda, 0xe9]][rng.below(2) as usize]),
        14 => out.extend([[0xdb, 0xdf][rng.below(2) as usize], [0xe8, 0xf0][rng.below(2) as usize] | i]),
        15 => out.extend([0xdf, 0xe0]),
        16 => out.extend([0xdd, 0xe0 | i]),
        17 => mem_abs(out, 0xdd, [2, 3][rng.below(2) as usize], 0x3400 + 8 * k),
        18 => mem_abs(out, 0xd9, [2, 3][rng.below(2) as usize], 0x3480 + 4 * k),
        19 => {
            let (op, reg) = [(0xdb, 2), (0xdb, 3), (0xdf, 2), (0xdf, 3), (0xdf, 7)][rng.below(5) as usize];
            mem_abs(out, op, reg, 0x34c0 + 8 * k);
        }
        20 => mem_abs(out, 0xd9, 5, 0x3500 + 2 * (k % 5)),
        21 => mem_abs(out, [0xd9, 0xdd][rng.below(2) as usize], 7, 0x3520 + 2 * k),
        22 => match rng.below(4) {
            0 => out.extend([0x0f, 0x6f, modrm_rr(rng.below(8) as u8, rng.below(8) as u8)]),
            1 => out.extend([0x0f, 0x77]),
            _ => out.push(0x9b),
        },
        _ => mem_abs(out, [0xd8, 0xdc][rng.below(2) as usize], [2, 3][rng.below(2) as usize], 0x3100 + 4 * k),
    }
}

fn x87_memory(rng: &mut Rng) -> Vec<u8> {
    let mut mem = vec![0u8; 0x600];
    let pick = |rng: &mut Rng| -> f64 {
        match rng.below(10) {
            0 => 0.0,
            1 => -0.0,
            2 => 1.0e300,
            3 => 1.0 / 3.0,
            4 => (rng.next() as i32) as f64 * 1.0e-6,
            _ => (rng.below(2001) as f64 - 1000.0) / 8.0,
        }
    };
    for k in 0..8 {
        let d = pick(rng);
        mem[8 * k..8 * k + 8].copy_from_slice(&d.to_le_bytes());
        let f = pick(rng) as f32;
        mem[0x100 + 4 * k..0x104 + 4 * k].copy_from_slice(&f.to_le_bytes());
        let n = rng.next() as i32 >> rng.below(31);
        mem[0x200 + 4 * k..0x204 + 4 * k].copy_from_slice(&n.to_le_bytes());
        let q = ((rng.next() as u64) << 32 | rng.next() as u64) as i64 >> rng.below(63);
        mem[0x300 + 8 * k..0x308 + 8 * k].copy_from_slice(&q.to_le_bytes());
    }
    for (j, cw) in [0x37fu16, 0x27f, 0xf7f, 0x77f, 0xb7f].iter().enumerate() {
        mem[0x500 + 2 * j..0x502 + 2 * j].copy_from_slice(&cw.to_le_bytes());
    }
    mem
}

#[test]
fn random_x87_programs_match_the_interpreter() {
    let mut rng = Rng(0x87_87);
    let (mut exits, mut insns) = (0, 0);
    for round in 0..400 {
        let mut code = Vec::new();
        for k in 0..4 {
            mem_abs(&mut code, 0xdd, 0, 0x3000 + 8 * k);
        }
        let n = 30;
        for _ in 0..n {
            random_x87(&mut rng, &mut code);
        }
        code.push(0xf4);
        let mem = x87_memory(&mut rng);
        let mut a = flat_cpu(0x1000, 0x8000);
        if round % 2 == 1 {
            a.fpu.cw = 0x27f;
        }
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        bus_a.ram.as_mut_slice()[0x3000..0x3600].copy_from_slice(&mem);
        bus_b.ram.as_mut_slice()[0x3000..0x3600].copy_from_slice(&mem);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        run_interp(&mut a, &mut bus_a);
        let stats = run_jit(&mut b, &mut bus_b);
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "round {round} flags\ncode: {}", hex.join(" "));
        assert_eq!(a.fpu, b.fpu, "round {round} x87 state\ncode: {}", hex.join(" "));
        assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "round {round} memory\ncode: {}", hex.join(" "));
        exits += stats.interp_exits;
        insns += n + 4;
    }
    assert!(exits * 4 < insns, "{exits} interpreter exits for {insns} x87 instructions");
}

fn sse_op(out: &mut Vec<u8>, pfx: u8, op: u8, reg: u8, rm: Option<u8>, addr: u32) {
    if pfx != 0 {
        out.push(pfx);
    }
    out.extend([0x0f, op]);
    match rm {
        Some(rm) => out.push(modrm_rr(reg, rm)),
        None => {
            out.push(reg << 3 | 5);
            out.extend(addr.to_le_bytes());
        }
    }
}

fn random_sse(rng: &mut Rng, out: &mut Vec<u8>) {
    let reg = rng.below(8) as u8;
    let rm = if rng.below(3) == 0 { None } else { Some(rng.below(8) as u8) };
    let slot = 0x3000 + 16 * rng.below(16);
    let any_pfx = [0, 0x66, 0xf3, 0xf2][rng.below(4) as usize];
    let packed_pfx = [0, 0x66][rng.below(2) as usize];
    const INT_OPS: [u8; 46] = [
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x74, 0x75, 0x76, 0xd4, 0xd5, 0xd8, 0xd9,
        0xda, 0xdc, 0xdd, 0xde, 0xe0, 0xe3, 0xe4, 0xe5, 0xe8, 0xe9, 0xea, 0xec, 0xed, 0xee, 0xf4, 0xf5, 0xf6, 0xf8, 0xf9, 0xfa, 0xfb,
        0xfc, 0xfd, 0xfe, 0xdb,
    ];
    match rng.below(24) {
        20 | 21 => sse_op(out, 0x66, INT_OPS[rng.below(INT_OPS.len() as u32) as usize], reg, rm, slot),
        22 => {
            let (op, sub) = [(0x71, 2), (0x71, 4), (0x71, 6), (0x72, 2), (0x72, 4), (0x72, 6), (0x73, 2), (0x73, 3), (0x73, 6), (0x73, 7)]
                [rng.below(10) as usize];
            out.extend([0x66, 0x0f, op, modrm_rr(sub, reg), rng.below(70) as u8]);
        }
        23 => match rng.below(3) {
            0 => {
                sse_op(out, 0x66, 0xc4, reg, rm.map(|_| rng.reg()), slot + 2 * rng.below(8));
                out.push(rng.next() as u8);
            }
            1 => out.extend([0x66, 0x0f, 0xc5, modrm_rr(rng.reg(), reg), rng.next() as u8]),
            _ => out.extend([0x66, 0x0f, 0xd7, modrm_rr(rng.reg(), reg)]),
        },
        0 => sse_op(out, packed_pfx, [0x10, 0x11, 0x28, 0x29][rng.below(4) as usize], reg, rm, slot),
        1 => sse_op(out, [0xf3, 0xf2][rng.below(2) as usize], [0x10, 0x11][rng.below(2) as usize], reg, rm, slot + 4 * rng.below(3)),
        2 => {
            let op = [0x12, 0x13, 0x16, 0x17][rng.below(4) as usize];
            let rm = if op & 1 == 1 || packed_pfx == 0x66 { None } else { rm };
            sse_op(out, packed_pfx, op, reg, rm, slot + 8 * rng.below(2));
        }
        3 => sse_op(out, packed_pfx, [0x14, 0x15, 0x54, 0x55, 0x56, 0x57][rng.below(6) as usize], reg, rm, slot),
        4 => sse_op(out, 0x66, [0xdb, 0xdf, 0xeb, 0xef][rng.below(4) as usize], reg, rm, slot),
        5 => {
            if rng.below(2) == 0 {
                sse_op(out, packed_pfx, 0xc6, reg, rm, slot);
            } else {
                sse_op(out, 0x66, 0x70, reg, rm, slot);
            }
            out.push(rng.next() as u8);
        }
        6 => match rng.below(4) {
            0 => sse_op(out, 0x66, 0x6e, reg, rm.map(|_| rng.reg()), slot + 4 * rng.below(4)),
            1 => sse_op(out, 0x66, 0x7e, reg, rm.map(|_| rng.reg()), slot + 4 * rng.below(4)),
            2 => sse_op(out, 0xf3, 0x7e, reg, rm, slot + 8 * rng.below(2)),
            _ => sse_op(out, 0x66, 0xd6, reg, rm, slot + 8 * rng.below(2)),
        },
        7 => sse_op(out, [0x66, 0xf3][rng.below(2) as usize], [0x6f, 0x7f][rng.below(2) as usize], reg, rm, slot),
        8..=11 => sse_op(out, any_pfx, [0x58, 0x59, 0x5c, 0x5e, 0x5d, 0x5f, 0x51][rng.below(7) as usize], reg, rm, slot),
        12 | 13 => {
            sse_op(out, any_pfx, 0xc2, reg, rm, slot);
            out.push(rng.below(8) as u8);
        }
        14 => sse_op(out, packed_pfx, [0x2e, 0x2f][rng.below(2) as usize], reg, rm, slot),
        15 => sse_op(out, [0xf3, 0xf2][rng.below(2) as usize], 0x2a, reg, rm.map(|_| rng.reg()), slot + 4 * rng.below(4)),
        16 => sse_op(out, [0xf3, 0xf2][rng.below(2) as usize], [0x2c, 0x2d][rng.below(2) as usize], rng.reg(), rm, slot),
        17 => sse_op(out, [0xf3, 0xf2][rng.below(2) as usize], 0x5a, reg, rm, slot),
        18 => {
            out.extend([0x0f, 0xae, 2 << 3 | 5]);
            out.extend((0x3100 + 4 * rng.below(6)).to_le_bytes());
        }
        _ => sse_op(out, any_pfx, [0x58, 0x59][rng.below(2) as usize], reg, rm, slot),
    }
}

fn sse_value(rng: &mut Rng) -> u32 {
    match rng.below(12) {
        0 => 0,
        1 => 0x8000_0000,
        2 => 0x7f80_0000,
        3 => 0xff80_0000,
        4 => 0x7fc0_0000 | rng.below(0x1000),
        5 => 0x7f80_0001 | rng.below(0x1000),
        6 => rng.below(0x80_0000) | (rng.below(2) << 31),
        7 => rng.next(),
        _ => (rng.next() & 0x87ff_ffff) | 0x3800_0000,
    }
}

#[test]
fn random_sse_programs_match_the_interpreter() {
    let mut rng = Rng(0x55e2);
    let (mut exits, mut insns) = (0, 0);
    for round in 0..400 {
        let mut code = Vec::new();
        let n = 30;
        for _ in 0..n {
            random_sse(&mut rng, &mut code);
        }
        code.push(0xf4);
        let mut mem = vec![0u8; 0x200];
        for k in 0..64 {
            let v = if rng.below(3) == 0 {
                (rng.next() as u64) << 32 | rng.next() as u64
            } else {
                sse_value(&mut rng) as u64
            };
            mem[4 * k..4 * k + 4].copy_from_slice(&(v as u32).to_le_bytes());
        }
        for (j, csr) in [0x1f80u32, 0x3f80, 0x5f80, 0x7f80, 0x9f80, 0x1fc0].iter().enumerate() {
            mem[0x100 + 4 * j..0x104 + 4 * j].copy_from_slice(&csr.to_le_bytes());
        }
        let mut a = flat_cpu(0x1000, 0x8000);
        a.cr4 |= x86::cpu::cr4::OSFXSR;
        for i in 0..8 {
            a.xmm[i] = (0..4).fold(0u128, |v, k| v | (sse_value(&mut rng) as u128) << (32 * k));
            if i != 4 {
                a.regs[i] = rng.next() >> rng.below(32);
            }
        }
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        bus_a.ram.as_mut_slice()[0x3000..0x3200].copy_from_slice(&mem);
        bus_b.ram.as_mut_slice()[0x3000..0x3200].copy_from_slice(&mem);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        run_interp(&mut a, &mut bus_a);
        let stats = run_jit(&mut b, &mut bus_b);
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "round {round} flags\ncode: {}", hex.join(" "));
        for i in 0..8 {
            assert_eq!(a.xmm[i], b.xmm[i], "round {round} xmm{i}\ncode: {}", hex.join(" "));
        }
        let de = x86::fpu::exc::DE as u32;
        assert_eq!(a.mxcsr | de, b.mxcsr | de, "round {round} mxcsr\ncode: {}", hex.join(" "));
        assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "round {round} memory\ncode: {}", hex.join(" "));
        exits += stats.interp_exits;
        insns += n;
    }
    assert!(exits * 3 < insns, "{exits} interpreter exits for {insns} SSE instructions");
}

#[test]
fn sysenter_sysexit_and_fs_loads_match_the_interpreter() {
    let user = [
        0x40, // inc eax
        0x89, 0xe2, // mov edx, esp
        0x0f, 0x34, // sysenter
        0x50, // push eax
        0x0f, 0xa2, // cpuid: an exit back to the host with FS = 0x3b
        0x58, // pop eax
        0x64, 0x8b, 0x1d, 0, 0, 0, 0, // mov ebx, fs:[0]
        0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // keeps the interpreter out of the kernel
        0xeb, 0xe0, // jmp 0x1000
    ];
    let kernel = [
        0xfe, 0x05, 0x3c, 0x40, 0, 0, // inc byte [0x403c]: the 0x3b descriptor's base moves by 64 KiB
        0x89, 0xd5, // mov ebp, edx
        0x6a, 0x30, // push 0x30
        0x0f, 0xa1, // pop fs
        0x64, 0x8b, 0x0d, 0, 0, 0, 0, // mov ecx, fs:[0]
        0x01, 0xce, // add esi, ecx
        0xb9, 0x23, 0, 0, 0, // mov ecx, 0x23
        0x8e, 0xd9, // mov ds, cx
        0x8e, 0xc1, // mov es, cx
        0x0f, 0x20, 0xd7, // mov edi, cr2
        0x83, 0xf8, 0x10, // cmp eax, 16: later rounds run with no exits in between
        0x74, 0x0e, // je the final pop fs
        0x6a, 0x3b, // push 0x3b
        0x0f, 0xa1, // pop fs
        0xba, 0x05, 0x10, 0, 0, // mov edx, 0x1005
        0x89, 0xe9, // mov ecx, ebp
        0xfb, // sti
        0x0f, 0x35, // sysexit
        0x6a, 0x3b, // push 0x3b
        0x0f, 0xa1, // pop fs
        0xf4, // hlt
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut bus_a = bus_with(0x1000, &user);
    let mem = bus_a.ram.as_mut_slice();
    mem[0x2000..0x2000 + kernel.len()].copy_from_slice(&kernel);
    let gdt: [u64; 8] = [
        0,
        0x00cf_9b00_0000_ffff,
        0x00cf_9300_0000_ffff,
        0x00cf_fb00_0000_ffff,
        0x00cf_f300_0000_ffff,
        0,
        0x0040_9300_5000_0fff,
        0x0040_f300_5100_0fff,
    ];
    for (i, d) in gdt.iter().enumerate() {
        mem[0x4000 + 8 * i..0x4008 + 8 * i].copy_from_slice(&d.to_le_bytes());
    }
    mem[0x5000..0x5004].copy_from_slice(&0x1111_1111u32.to_le_bytes());
    for k in 0..32usize {
        let at = 0x5100 + (k << 16);
        mem[at..at + 4].copy_from_slice(&(0x2222_0000 + k as u32).to_le_bytes());
    }
    paged(&mut a, &mut bus_a, false);
    a.gdtr = x86::cpu::DescriptorTable { base: 0x4000, limit: 0x3f };
    a.sysenter_cs = 8;
    a.sysenter_esp = 0x9000;
    a.sysenter_eip = 0x2000;
    for (seg, sel) in [(0, 0x23), (1, 0x1b), (2, 0x23), (3, 0x23)] {
        a.segs[seg].selector = sel;
        a.segs[seg].attr |= 3 << 5;
    }
    assert!(interp::load_segment(&mut a, &mut bus_a, 4, 0x3b));
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap() };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!(a.eip, b.eip);
    assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK);
    assert_eq!(a.segs, b.segs);
    assert!(stats.interp_exits <= 17, "system calls left translated code: {stats:?}");
}

fn ring_test_machine(user: &[u8], kernel: &[u8]) -> (Cpu, FlatBus) {
    let mut cpu = flat_cpu(0x2000, 0x9000);
    let mut bus = bus_with(0x1000, user);
    let mem = bus.ram.as_mut_slice();
    mem[0x2000..0x2000 + kernel.len()].copy_from_slice(kernel);
    let gdt: [u64; 8] = [
        0,
        0x00cf_9b00_0000_ffff,
        0x00cf_9300_0000_ffff,
        0x00cf_fb00_0000_ffff,
        0x00cf_f300_0000_ffff,
        0,
        0x0040_9300_5000_0fff,
        0x0040_f300_5100_0fff,
    ];
    for (i, d) in gdt.iter().enumerate() {
        mem[0x4000 + 8 * i..0x4008 + 8 * i].copy_from_slice(&d.to_le_bytes());
    }
    paged(&mut cpu, &mut bus, false);
    cpu.gdtr = x86::cpu::DescriptorTable { base: 0x4000, limit: 0x3f };
    cpu.sysenter_cs = 8;
    cpu.sysenter_esp = 0x9000;
    cpu.sysenter_eip = 0x2100;
    for (seg, sel) in [(0, 0x23), (2, 0x10), (3, 0x23)] {
        cpu.segs[seg].selector = sel;
    }
    cpu.segs[0].attr |= 3 << 5;
    cpu.segs[3].attr |= 3 << 5;
    assert!(interp::load_segment(&mut cpu, &mut bus, 4, 0x3b));
    assert!(interp::load_segment(&mut cpu, &mut bus, 5, 0));
    (cpu, bus)
}

#[test]
fn iret_within_the_kernel_and_to_user_mode_matches_the_interpreter() {
    let user = [
        0x40, // inc eax
        0x89, 0xe2, // mov edx, esp
        0x0f, 0x34, // sysenter
    ];
    let mut kernel = vec![
        0x9c, // pushfd
        0x6a, 0x08, // push 0x08
        0x68, 0x0b, 0x20, 0, 0, // push 0x200b
        0xcf, // iretd, to the next instruction at CPL 0
        0x90, 0x90, // padding to 0x200b
        0x6a, 0x23, // push 0x23
        0x68, 0x00, 0x80, 0, 0, // push 0x8000
        0x68, 0x02, 0x02, 0, 0, // push 0x202 (IF)
        0x6a, 0x1b, // push 0x1b
        0x68, 0x00, 0x10, 0, 0, // push 0x1000
        0xcf, // iretd, to user mode
    ];
    kernel.resize(0x100, 0x90);
    kernel.extend([
        0x83, 0xf8, 0x03, // cmp eax, 3
        0x75, 0x01, // jne over the hlt
        0xf4, // hlt
        0xbc, 0x00, 0x90, 0, 0, // mov esp, 0x9000
        0xe9, 0xfb, 0xfe, 0xff, 0xff, // jmp 0x200b
    ]);
    let (mut a, mut bus_a) = ring_test_machine(&user, &kernel);
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap() };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!(a.eip, b.eip);
    assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK);
    assert_eq!(a.segs, b.segs);
    assert!(stats.interp_exits <= 1, "IRET left translated code: {stats:?}");
}

#[test]
fn x87_results_survive_a_store_into_the_running_code() {
    let code = [
        0xdd, 0x05, 0x00, 0x30, 0, 0, // fld qword [0x3000]
        0xdc, 0x05, 0x08, 0x30, 0, 0, // fadd qword [0x3008]
        0xd9, 0xc0, // fld st(0)
        0xde, 0xc9, // fmulp
        0xc6, 0x05, 0x1a, 0x10, 0, 0, 0x90, // mov byte [0x101a], 0x90: a store into this page
        0x90, 0xf4, // nop, hlt
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut bus_a = bus_with(0x1000, &code);
    bus_a.ram.as_mut_slice()[0x3000..0x3008].copy_from_slice(&1.5f64.to_le_bytes());
    bus_a.ram.as_mut_slice()[0x3008..0x3010].copy_from_slice(&2.25f64.to_le_bytes());
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap() };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert!(stats.smc_invalidations > 0, "the store did not hit the running block: {stats:?}");
    assert_eq!(a.fpu, b.fpu);
}

#[test]
fn code_mapped_at_two_linear_addresses_pushes_each_return_address() {
    let code = [
        0xe8, 0xfb, 0x0f, 0x00, 0x00, // call 0x2000
        0x89, 0xc3, // mov ebx, eax
        0xe8, 0xf4, 0x3f, 0x00, 0x00, // call 0x5000
        0xf4,
    ];
    let thunk = [0xe8, 0x00, 0x00, 0x00, 0x00, 0x58, 0xc3]; // call next; pop eax; ret
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut bus_a = bus_with(0x1000, &code);
    bus_a.ram.as_mut_slice()[0x2000..0x2000 + thunk.len()].copy_from_slice(&thunk);
    paged(&mut a, &mut bus_a, false);
    bus_a.ram.as_mut_slice()[0x11014..0x11018].copy_from_slice(&0x2007u32.to_le_bytes());
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap() };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    run_jit(&mut b, &mut bus_b);
    assert_eq!((a.regs[3], a.regs[0]), (0x2005, 0x5005));
    assert_eq!(b.regs, a.regs);
}

#[test]
fn rdtsc_reads_the_host_clock_without_leaving_translated_code() {
    let mut code = vec![
        0x0f, 0x31, // rdtsc
        0x89, 0xc6, // mov esi, eax
        0x89, 0xd7, // mov edi, edx
    ];
    code.extend([0x40; 16]); // inc eax, past the interpreter's tail
    code.extend([0x0f, 0x31, 0xf4]); // rdtsc; hlt
    let mut cpu = flat_cpu(0x1000, 0x8000);
    cpu.start_host_tsc();
    let mut bus = bus_with(0x1000, &code);
    let before = cpu.read_tsc();
    let stats = run_jit(&mut cpu, &mut bus);
    let after = cpu.read_tsc();
    let first = (cpu.regs[7] as u64) << 32 | cpu.regs[6] as u64;
    let second = (cpu.regs[2] as u64) << 32 | cpu.regs[0] as u64;
    assert!(before <= first && first <= second && second <= after, "the guest TSC must agree with the host clock and never run backwards: {before} {first} {second} {after}");
    assert_eq!(stats.interp_exits, 1, "Linux reads the TSC constantly for scheduling, so RDTSC must not exit: {stats:?}");
}

#[test]
fn framebuffer_writes_skip_the_host_and_are_reported_once() {
    let code = [
        0xbf, 0x00, 0x00, 0x00, 0xe0, // mov edi, 0xe0000000
        0xb8, 0x44, 0x33, 0x22, 0x11, // mov eax, 0x11223344
        0x89, 0x07, // mov [edi], eax
        0x89, 0x87, 0x00, 0x30, 0x00, 0x00, // mov [edi+0x3000], eax
        0x8b, 0x1f, // mov ebx, [edi]
        0xf4,
    ];
    let len = 1 << 20;
    let layout = std::alloc::Layout::from_size_align(len, 16 << 10).unwrap();
    let fb = unsafe { std::alloc::alloc_zeroed(layout) };
    let _free = scopeguard(move || unsafe { std::alloc::dealloc(fb, layout) });
    let mut cpu = flat_cpu(0x1000, 0x8000);
    let mut bus = bus_with(0x1000, &code);
    let mut reports = Vec::new();
    let stats = run_jit_with(
        &mut cpu,
        &mut bus,
        |j| unsafe { j.set_framebuffer(Some((0xe000_0000, fb, len))).unwrap() },
        |j| {
            reports.push(j.take_framebuffer_written().unwrap());
            reports.push(j.take_framebuffer_written().unwrap());
        },
    );
    let pixels = unsafe { std::slice::from_raw_parts(fb, len) };
    assert_eq!(&pixels[..4], &[0x44, 0x33, 0x22, 0x11]);
    assert_eq!(&pixels[0x3000..0x3004], &[0x44, 0x33, 0x22, 0x11]);
    assert_eq!(cpu.regs[3], 0x1122_3344);
    assert_eq!(stats.mmio_exits, 0, "framebuffer stores must not leave the VM, or drawing crawls: {stats:?}");
    assert_eq!(reports, [true, false], "the display must redraw after writes, and only then");
}

#[test]
fn a_block_compiled_at_the_end_of_the_code_cache_still_reaches_the_runtime() {
    let mut code = Vec::new();
    for _ in 0..31 {
        code.extend([0xd9, 0x03, 0xd9, 0x5b, 0x04]); // fld dword [ebx]; fstp dword [ebx+4]
    }
    code.extend([0xff, 0x43, 0x08, 0xf4]); // inc dword [ebx+8]; hlt
    let mut cpu = flat_cpu(0x1000, 0x8000);
    cpu.regs[3] = 0x3000;
    let mut bus = bus_with(0x1000, &code);
    run_jit_with(&mut cpu, &mut bus, |j| j.code_next = crate::layout::CODE_CACHE_END - 16, |_| {});
    assert_eq!(bus.ram.as_slice()[0x3008], 1, "a long block near the end of the cache must translate and run");
}

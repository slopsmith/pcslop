use std::process::ExitCode;
use std::time::Duration;

use pc::{Board, Config};
use x86::cpu::Cpu;
use x86::interp::{Exit, step};

struct Args {
    bios: String,
    ram_mb: u32,
    max_instructions: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { bios: String::new(), ram_mb: 256, max_instructions: None };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bios" => args.bios = it.next().ok_or("--bios needs a path")?,
            "--ram" => args.ram_mb = it.next().and_then(|v| v.parse().ok()).ok_or("--ram needs a size in MiB")?,
            "--max-instructions" => {
                args.max_instructions = Some(it.next().and_then(|v| v.parse().ok()).ok_or("--max-instructions needs a number")?)
            }
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    if args.bios.is_empty() {
        return Err("usage: slopper --bios <bios.bin> [--ram MiB] [--max-instructions N]".into());
    }
    Ok(args)
}

fn dump(cpu: &Cpu) {
    let r = &cpu.regs;
    eprintln!(
        "eax={:08x} ecx={:08x} edx={:08x} ebx={:08x} esp={:08x} ebp={:08x} esi={:08x} edi={:08x}",
        r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]
    );
    let s = &cpu.segs;
    eprintln!(
        "eip={:08x} eflags={:08x} cs={:04x} ss={:04x} ds={:04x} es={:04x} cr0={:08x}",
        cpu.eip, cpu.eflags, s[1].selector, s[2].selector, s[3].selector, s[0].selector, cpu.cr0
    );
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let bios = match std::fs::read(&args.bios) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {}: {e}", args.bios);
            return ExitCode::FAILURE;
        }
    };
    let mut board = Board::new(Config { ram_bytes: args.ram_mb << 20, bios });
    let mut cpu = Cpu::new();
    let started = std::time::Instant::now();
    let mut executed: u64 = 0;

    loop {
        for _ in 0..1024 {
            match step(&mut cpu, &mut board) {
                Exit::Continue => executed += 1,
                Exit::Halted => {
                    let wait = board.next_event_ns();
                    std::thread::sleep(Duration::from_nanos(wait.min(1_000_000)));
                    board.tick();
                    if !cpu.flag(x86::cpu::flags::IF) && !board.pic.intr() {
                        eprintln!("halted with interrupts disabled after {executed} instructions");
                        dump(&cpu);
                        return ExitCode::SUCCESS;
                    }
                    break;
                }
                Exit::Shutdown => {
                    eprintln!("triple fault after {executed} instructions; resetting");
                    dump(&cpu);
                    cpu.reset();
                    board.reset();
                    break;
                }
                Exit::Unimplemented { eip, what } => {
                    eprintln!("unimplemented at {eip:#x}: {what} after {executed} instructions");
                    dump(&cpu);
                    return ExitCode::FAILURE;
                }
            }
        }
        board.tick();
        cpu.tsc = board.now_ns() * 28 / 10;
        if board.reset_requested {
            cpu.reset();
            board.reset();
        }
        if args.max_instructions.is_some_and(|m| executed >= m) {
            let secs = started.elapsed().as_secs_f64();
            eprintln!("stopped after {executed} instructions ({:.1} MIPS)", executed as f64 / secs / 1e6);
            dump(&cpu);
            return ExitCode::SUCCESS;
        }
    }
}

mod keys;

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pc::{Board, Config};
use x86::cpu::Cpu;
use x86::interp::{Exit, step};

struct Args {
    bios: String,
    vgabios: Option<String>,
    cdrom: Option<String>,
    hda: Option<String>,
    ram_mb: u32,
    max_instructions: Option<u64>,
    headless: bool,
    jit: bool,
    screenshots: Vec<(f64, PathBuf)>,
}

const USAGE: &str = "usage: slopper --bios <bios.bin> [--vgabios <vgabios.bin>] [--cdrom <iso>] [--hda <img>] \
[--ram MiB] [--headless] [--jit] [--screenshot SECONDS:PATH]... [--max-instructions N]";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        bios: String::new(),
        vgabios: None,
        cdrom: None,
        hda: None,
        ram_mb: 256,
        max_instructions: None,
        headless: false,
        jit: false,
        screenshots: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--bios" => args.bios = value()?,
            "--vgabios" => args.vgabios = Some(value()?),
            "--cdrom" => args.cdrom = Some(value()?),
            "--hda" => args.hda = Some(value()?),
            "--ram" => args.ram_mb = value()?.parse().map_err(|_| "--ram needs a size in MiB")?,
            "--max-instructions" => {
                args.max_instructions = Some(value()?.parse().map_err(|_| "--max-instructions needs a number")?)
            }
            "--headless" => args.headless = true,
            "--jit" => args.jit = true,
            "--screenshot" => {
                let v = value()?;
                let (secs, path) = v.split_once(':').ok_or("--screenshot needs SECONDS:PATH")?;
                let secs = secs.parse().map_err(|_| "--screenshot needs SECONDS:PATH")?;
                args.screenshots.push((secs, PathBuf::from(path)));
            }
            _ => return Err(format!("unknown argument {a}\n{USAGE}")),
        }
    }
    if args.bios.is_empty() {
        return Err(USAGE.into());
    }
    Ok(args)
}

enum Event {
    Key(u16, bool),
    Type(String),
    Mouse(i32, i32, u8),
    Screenshot(PathBuf),
    Quit,
}

#[derive(Default)]
struct Frame {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    seq: u64,
}

struct Shared {
    frame: Mutex<Frame>,
    done: AtomicBool,
    instructions: AtomicU64,
}

fn dump(cpu: &Cpu) {
    let r = &cpu.regs;
    eprintln!(
        "eax={:08x} ecx={:08x} edx={:08x} ebx={:08x} esp={:08x} ebp={:08x} esi={:08x} edi={:08x}",
        r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]
    );
    let s = &cpu.segs;
    eprintln!(
        "eip={:08x} eflags={:08x} cs={:04x} ss={:04x} ds={:04x} es={:04x} cr0={:08x} cr3={:08x}",
        cpu.eip, cpu.eflags, s[1].selector, s[2].selector, s[3].selector, s[0].selector, cpu.cr0, cpu.cr3
    );
}

fn save_png(path: &PathBuf, f: &Frame) -> Result<(), String> {
    if f.width == 0 {
        return Err("no picture to save yet".into());
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), f.width, f.height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    let rgb: Vec<u8> = f.pixels.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
    w.write_image_data(&rgb).map_err(|e| e.to_string())
}

/// Keys from `type` commands, released into the guest at a human pace.
#[derive(Default)]
struct KeyQueue {
    pending: VecDeque<(u16, bool)>,
    next: Option<Instant>,
}

impl KeyQueue {
    fn push_char(&mut self, c: char) {
        let Some((code, shift)) = keys::char_key(c) else { return };
        if shift {
            self.pending.push_back((0x2a, true));
        }
        self.pending.push_back((code, true));
        self.pending.push_back((code, false));
        if shift {
            self.pending.push_back((0x2a, false));
        }
    }

    fn poll(&mut self, board: &mut Board) {
        let now = Instant::now();
        if self.next.is_some_and(|t| now < t) {
            return;
        }
        if let Some((code, down)) = self.pending.pop_front() {
            board.key(code, down);
            self.next = Some(now + Duration::from_millis(25));
        }
    }
}

fn emulate(mut args: Args, board_cfg: Config, events: Receiver<Event>, shared: Arc<Shared>) -> ExitCode {
    let mut board = Board::new(board_cfg);
    if let Some(path) = &args.cdrom
        && let Err(e) = board.attach_cdrom(path)
    {
        eprintln!("cannot open {path}: {e}");
        return ExitCode::FAILURE;
    }
    if let Some(path) = &args.hda
        && let Err(e) = board.attach_disk(path)
    {
        eprintln!("cannot open {path}: {e}");
        return ExitCode::FAILURE;
    }
    let mut jit = if args.jit {
        let (ptr, len) = board.ram_region();
        match unsafe { jit::Jit::new(ptr, len) } {
            Ok(j) => Some(j),
            Err(e) => {
                eprintln!("cannot start the VM: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    if let Some(j) = &jit {
        let kicker = j.kicker();
        let shared = shared.clone();
        std::thread::spawn(move || {
            while !shared.done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
                kicker.kick();
            }
        });
    }
    let mut cpu = Cpu::new();
    let started = Instant::now();
    let mut executed: u64 = 0;
    let mut keyq = KeyQueue::default();
    let mut next_frame = Instant::now();
    let mut scratch = Vec::new();

    let code = 'run: loop {
        let mut outcome = None;
        let irq_due = cpu.flag(x86::cpu::flags::IF) && board.pic.intr();
        if let Some(j) = jit.as_mut()
            && jit::supported(&cpu)
            && !cpu.halted
            && !irq_due
        {
            match j.run(&mut cpu, &mut board) {
                Ok(jit::RunExit::Kicked) => {}
                Ok(jit::RunExit::Interp(e)) => outcome = Some(e),
                Err(e) => {
                    eprintln!("VM error: {e}");
                    dump(&cpu);
                    break 'run ExitCode::FAILURE;
                }
            }
        } else {
            for _ in 0..1024 {
                let e = step(&mut cpu, &mut board);
                if e != Exit::Continue {
                    outcome = Some(e);
                    break;
                }
                executed += 1;
                if jit.is_some() && jit::supported(&cpu) {
                    break;
                }
            }
        }
        match outcome {
            Some(Exit::Halted) => {
                let wait = board.next_event_ns().min(1_000_000);
                std::thread::sleep(Duration::from_nanos(wait));
            }
            Some(Exit::Shutdown) => {
                eprintln!("triple fault after {executed} instructions; resetting");
                dump(&cpu);
                cpu.reset();
                board.reset();
            }
            Some(Exit::Unimplemented { eip, what }) => {
                eprintln!("unimplemented at {eip:#x}: {what} after {executed} instructions");
                dump(&cpu);
                break 'run ExitCode::FAILURE;
            }
            _ => {}
        }
        let jit_insns = jit.as_mut().map_or(0, |j| j.instructions());
        shared.instructions.store(executed + jit_insns, Ordering::Relaxed);
        board.pic.set_irq(13, cpu.ferr_pending);
        board.tick();
        cpu.tsc = board.now_ns() * 28 / 10;
        if board.reset_requested {
            cpu.reset();
            board.reset();
        }
        keyq.poll(&mut board);
        while let Ok(ev) = events.try_recv() {
            match ev {
                Event::Key(code, down) => board.key(code, down),
                Event::Type(text) => text.chars().for_each(|c| keyq.push_char(c)),
                Event::Mouse(dx, dy, b) => board.mouse_motion(dx, dy, b),
                Event::Screenshot(path) => {
                    let f = shared.frame.lock().unwrap();
                    match save_png(&path, &f) {
                        Ok(()) => eprintln!("saved {}", path.display()),
                        Err(e) => eprintln!("screenshot failed: {e}"),
                    }
                }
                Event::Quit => break 'run ExitCode::SUCCESS,
            }
        }
        if let Some(i) = args.screenshots.iter().position(|(t, _)| started.elapsed().as_secs_f64() >= *t) {
            let (_, path) = args.screenshots.remove(i);
            if let Some((w, h)) = board.render(&mut scratch) {
                let f = Frame { pixels: scratch.clone(), width: w, height: h, seq: 0 };
                match save_png(&path, &f) {
                    Ok(()) => eprintln!("saved {}", path.display()),
                    Err(e) => eprintln!("screenshot failed: {e}"),
                }
            }
        }
        if Instant::now() >= next_frame {
            next_frame = Instant::now() + Duration::from_millis(16);
            if let Some((w, h)) = board.render(&mut scratch) {
                let mut f = shared.frame.lock().unwrap();
                std::mem::swap(&mut f.pixels, &mut scratch);
                f.width = w;
                f.height = h;
                f.seq += 1;
            }
        }
        if args.max_instructions.is_some_and(|m| executed + jit_insns >= m) {
            break ExitCode::SUCCESS;
        }
    };
    let secs = started.elapsed().as_secs_f64();
    let jit_insns = jit.as_mut().map_or(0, |j| j.instructions());
    let total = executed + jit_insns;
    let eip = cpu.eip;
    if let Ok(phys) = x86::interp::translate(&mut cpu, &mut board, eip, false) {
        let bytes: Vec<String> = (0..64).map(|i| format!("{:02x}", x86::bus::Bus::read8(&mut board, phys - 32 + i))).collect();
        eprintln!("code at eip: {}", bytes.join(" "));
    }
    eprintln!(
        "ran {total} instructions ({jit_insns} translated) in {secs:.1}s ({:.1} MIPS)",
        total as f64 / secs / 1e6
    );
    if let Some(j) = &jit {
        eprintln!("{:?}", j.stats);
        if let Some(p) = &j.interp_profile {
            let mut v: Vec<_> = p.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = v.iter().take(40).map(|(k, n)| format!("{k:04x}:{n}")).collect();
            eprintln!("interpreter exits by opcode: {}", top.join(" "));
        }
    }
    dump(&cpu);
    shared.done.store(true, Ordering::Relaxed);
    code
}

fn stdin_commands(tx: Sender<Event>) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let ev = match cmd {
            "" => continue,
            "shot" => Event::Screenshot(PathBuf::from(if rest.is_empty() { "screen.png" } else { rest })),
            "type" => Event::Type(rest.to_string()),
            "key" => {
                let mut evs = Vec::new();
                let names: Vec<&str> = rest.split('+').collect();
                let codes: Option<Vec<u16>> = names.iter().map(|n| keys::by_name(n)).collect();
                let Some(codes) = codes else {
                    eprintln!("unknown key in {rest}");
                    continue;
                };
                evs.extend(codes.iter().map(|&c| Event::Key(c, true)));
                evs.extend(codes.iter().rev().map(|&c| Event::Key(c, false)));
                for e in evs {
                    let _ = tx.send(e);
                }
                continue;
            }
            "quit" => Event::Quit,
            _ => {
                eprintln!("commands: shot [path] | type <text> | key <name>[+name...] | quit");
                continue;
            }
        };
        if tx.send(ev).is_err() {
            break;
        }
    }
}

#[cfg(feature = "window")]
fn window_loop(tx: Sender<Event>, shared: Arc<Shared>) {
    use minifb::{KeyRepeat, MouseMode, Window, WindowOptions};
    let opts = WindowOptions { resize: true, scale_mode: minifb::ScaleMode::AspectRatioStretch, ..Default::default() };
    let mut window = match Window::new("86slopper", 720, 400, opts) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("cannot open window: {e}");
            return;
        }
    };
    window.set_target_fps(60);
    let mut seq = 0;
    let mut buf = Vec::new();
    let (mut w, mut h) = (720usize, 400usize);
    let mut last_mouse: Option<(f32, f32)> = None;
    let mut rate_at = (Instant::now(), 0u64);
    while window.is_open() && !shared.done.load(Ordering::Relaxed) {
        let elapsed = rate_at.0.elapsed();
        if elapsed >= Duration::from_millis(500) {
            let n = shared.instructions.load(Ordering::Relaxed);
            let mips = n.saturating_sub(rate_at.1) as f64 / elapsed.as_secs_f64() / 1e6;
            window.set_title(&format!("86slopper | {mips:.1} MIPS"));
            rate_at = (Instant::now(), n);
        }
        {
            let f = shared.frame.lock().unwrap();
            if f.seq != seq && f.width > 0 {
                seq = f.seq;
                buf.clone_from(&f.pixels);
                (w, h) = (f.width as usize, f.height as usize);
            }
        }
        if buf.len() == w * h && !buf.is_empty() {
            let _ = window.update_with_buffer(&buf, w, h);
        } else {
            window.update();
        }
        for k in window.get_keys_pressed(KeyRepeat::No) {
            if let Some(c) = keys::from_minifb(k) {
                let _ = tx.send(Event::Key(c, true));
            }
        }
        for k in window.get_keys_released() {
            if let Some(c) = keys::from_minifb(k) {
                let _ = tx.send(Event::Key(c, false));
            }
        }
        if let Some((x, y)) = window.get_mouse_pos(MouseMode::Pass) {
            let buttons = window.get_mouse_down(minifb::MouseButton::Left) as u8
                | (window.get_mouse_down(minifb::MouseButton::Right) as u8) << 1
                | (window.get_mouse_down(minifb::MouseButton::Middle) as u8) << 2;
            if let Some((lx, ly)) = last_mouse
                && (lx != x || ly != y || buttons != 0)
            {
                let _ = tx.send(Event::Mouse((x - lx) as i32, (ly - y) as i32, buttons));
            }
            last_mouse = Some((x, y));
        }
    }
    let _ = tx.send(Event::Quit);
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let read = |p: &str| std::fs::read(p).map_err(|e| format!("cannot read {p}: {e}"));
    let bios = match read(&args.bios) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let vgabios = match args.vgabios.as_deref().map(read).transpose() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let cfg = Config { ram_bytes: args.ram_mb << 20, bios, vgabios };
    let shared = Arc::new(Shared {
        frame: Mutex::new(Frame::default()),
        done: AtomicBool::new(false),
        instructions: AtomicU64::new(0),
    });
    let (tx, rx) = channel();
    let headless = args.headless || !cfg!(feature = "window");
    let emu_shared = shared.clone();
    let emu = std::thread::spawn(move || emulate(args, cfg, rx, emu_shared));

    let stdin_tx = tx.clone();
    std::thread::spawn(move || stdin_commands(stdin_tx));
    #[cfg(feature = "window")]
    if !headless {
        window_loop(tx, shared);
    }
    #[cfg(not(feature = "window"))]
    let _ = (tx, shared);
    let _ = headless;
    emu.join().unwrap_or(ExitCode::FAILURE)
}

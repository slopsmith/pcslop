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
    exit_on_text: Option<String>,
    script: Option<PathBuf>,
    control: Option<PathBuf>,
}

const USAGE: &str = "usage: slopper --bios <bios.bin> [--vgabios <vgabios.bin>] [--cdrom <iso>] [--hda <img>] \
[--ram MiB] [--headless] [--jit] [--screenshot SECONDS:PATH]... [--exit-on-text TEXT] [--script FILE] [--control SOCKET] [--max-instructions N]";

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
        exit_on_text: None,
        script: None,
        control: None,
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
            "--exit-on-text" => args.exit_on_text = Some(value()?),
            "--script" => args.script = Some(PathBuf::from(value()?)),
            "--control" => args.control = Some(PathBuf::from(value()?)),
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
    /// A control-socket query answered by the emulator thread.
    Query(String, Sender<String>),
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

/// The 80x25 text-mode screen as 25 newline-separated rows, with non-ASCII
/// characters shown as spaces.
fn text_screen(board: &mut Board) -> String {
    (0..25)
        .map(|row| {
            let line: String = (0..80)
                .map(|col| {
                    let b = x86::bus::Bus::read8(board, 0xb8000 + 2 * (row * 80 + col));
                    if b.is_ascii_graphic() || b == b' ' { b as char } else { ' ' }
                })
                .collect();
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
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

/// Steps from a `--script` file, run against the text-mode screen:
/// `wait TEXT`, `key NAME[+NAME]`, `type TEXT`, `sleep SECONDS`, `shot PATH`, `quit`.
struct Script {
    steps: VecDeque<String>,
    sleep_until: Option<Instant>,
}

enum ScriptAction {
    Idle,
    Event(Event),
    Quit,
}

impl Script {
    fn load(path: &PathBuf) -> std::io::Result<Script> {
        let text = std::fs::read_to_string(path)?;
        let steps = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect();
        Ok(Script { steps, sleep_until: None })
    }

    fn poll(&mut self, board: &mut Board, keyq: &mut KeyQueue) -> ScriptAction {
        if self.sleep_until.is_some_and(|t| Instant::now() < t) || !keyq.pending.is_empty() {
            return ScriptAction::Idle;
        }
        self.sleep_until = None;
        let Some(step) = self.steps.front().cloned() else { return ScriptAction::Idle };
        let (cmd, rest) = step.split_once(' ').unwrap_or((step.as_str(), ""));
        let action = match cmd {
            "wait" => {
                if !text_screen(board).contains(rest) {
                    return ScriptAction::Idle;
                }
                eprintln!("script: saw {rest:?}");
                ScriptAction::Idle
            }
            "key" => {
                for name in rest.split('+') {
                    if let Some(c) = keys::by_name(name) {
                        keyq.pending.push_back((c, true));
                    }
                }
                for name in rest.split('+').rev() {
                    if let Some(c) = keys::by_name(name) {
                        keyq.pending.push_back((c, false));
                    }
                }
                ScriptAction::Idle
            }
            "type" => {
                rest.chars().for_each(|c| keyq.push_char(c));
                ScriptAction::Idle
            }
            "sleep" => {
                self.sleep_until = Some(Instant::now() + Duration::from_secs_f64(rest.parse().unwrap_or(1.0)));
                ScriptAction::Idle
            }
            "shot" => ScriptAction::Event(Event::Screenshot(PathBuf::from(rest))),
            "quit" => ScriptAction::Quit,
            other => {
                eprintln!("script: unknown step {other:?}");
                ScriptAction::Idle
            }
        };
        self.steps.pop_front();
        action
    }
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
                std::thread::sleep(Duration::from_micros(500));
                kicker.kick();
            }
        });
    }
    let mut cpu = Cpu::new();
    let started = Instant::now();
    let mut executed: u64 = 0;
    let mut keyq = KeyQueue::default();
    let mut paused = false;
    let mut last_status = (Instant::now(), 0u64);
    let mut time = HostTime::default();
    let mut last_time = HostTime::default();
    let mut last_stats = jit::Stats::default();
    let mut script = match args.script.as_ref().map(Script::load).transpose() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read script: {e}");
            return ExitCode::FAILURE;
        }
    };
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
            let t = Instant::now();
            let r = j.run(&mut cpu, &mut board);
            time.jit += t.elapsed();
            match r {
                Ok(jit::RunExit::Kicked) => {}
                Ok(jit::RunExit::Interp(e)) => outcome = Some(e),
                Err(e) => {
                    eprintln!("VM error: {e}");
                    dump(&cpu);
                    break 'run ExitCode::FAILURE;
                }
            }
        } else {
            let t = Instant::now();
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
            time.interp += t.elapsed();
        }
        match outcome {
            Some(Exit::Halted) => {
                let wait = board.next_event_ns().min(1_000_000);
                let t = Instant::now();
                std::thread::sleep(Duration::from_nanos(wait));
                time.halted += t.elapsed();
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
        if let Some(sc) = script.as_mut() {
            match sc.poll(&mut board, &mut keyq) {
                ScriptAction::Idle => {}
                ScriptAction::Event(Event::Screenshot(path)) => {
                    if let Some((w, h)) = board.render(&mut scratch) {
                        let f = Frame { pixels: scratch.clone(), width: w, height: h, seq: 0 };
                        match save_png(&path, &f) {
                            Ok(()) => eprintln!("saved {}", path.display()),
                            Err(e) => eprintln!("screenshot failed: {e}"),
                        }
                    }
                }
                ScriptAction::Event(_) => {}
                ScriptAction::Quit => break ExitCode::SUCCESS,
            }
        }
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
                Event::Query(q, reply) => {
                    let (cmd, rest) = q.split_once(' ').unwrap_or((q.as_str(), ""));
                    let answer = match cmd {
                        "screen" => text_screen(&mut board),
                        "regs" => format_regs(&cpu),
                        "status" => {
                            let jit_insns = jit.as_mut().map_or(0, |j| j.instructions());
                            let total = executed + jit_insns;
                            let now = Instant::now();
                            let (t, n) = last_status;
                            let span = now.duration_since(t).as_secs_f64();
                            let mips = total.saturating_sub(n) as f64 / span / 1e6;
                            last_status = (now, total);
                            let pct = |a: Duration, b: Duration| 100.0 * (a - b).as_secs_f64() / span;
                            let host = format!(
                                "host: jit {:.0}% interp {:.0}% halted {:.0}% over {span:.1}s",
                                pct(time.jit, last_time.jit),
                                pct(time.interp, last_time.interp),
                                pct(time.halted, last_time.halted)
                            );
                            last_time = time;
                            let mode = if jit.is_some() && jit::supported(&cpu) { "jit" } else { "interp" };
                            let mut out = format!(
                                "time {:.1}s instructions {total} ({jit_insns} translated) mips {mips:.1} mode {mode} cpl {} halted {} paused {paused}\n{}",
                                started.elapsed().as_secs_f64(),
                                cpu.cpl(),
                                cpu.halted,
                                format_regs(&cpu)
                            );
                            out += &format!("\n{host} timer backlog {}", board.timer_backlog());
                            if let Some(j) = jit.as_mut() {
                                let (a, b) = (j.stats, last_stats);
                                let r = |x: u64, y: u64| (x - y) as f64 / span;
                                out += &format!(
                                    "\nexits/s: interp {:.0} miss {:.0} fault {:.0} mmio {:.0} | translations/s {:.0} smc/s {:.0} flushes/s {:.0} guest #PF/s {:.0}",
                                    r(a.interp_exits, b.interp_exits),
                                    r(a.miss_exits, b.miss_exits),
                                    r(a.shadow_fills + a.page_faults, b.shadow_fills + b.page_faults),
                                    r(a.mmio_exits, b.mmio_exits),
                                    r(a.blocks, b.blocks),
                                    r(a.smc_invalidations, b.smc_invalidations),
                                    r(a.flushes, b.flushes),
                                    r(a.page_faults, b.page_faults),
                                );
                                last_stats = a;
                                out += &format!("\n{}\n{:?}", j.debug_state(), j.stats);
                            }
                            out
                        }
                        "profile" => match (jit.as_mut(), rest) {
                            (None, _) => "the JIT is off".into(),
                            (Some(j), "on" | "off") => {
                                j.set_profiling(rest == "on");
                                format!("profiling {rest}")
                            }
                            (Some(j), _) => j.profile_report(),
                        },
                        "pause" => {
                            paused = true;
                            "paused".into()
                        }
                        "resume" => {
                            paused = false;
                            "resumed".into()
                        }
                        "mem" => {
                            let mut it = rest.split_whitespace();
                            let addr = it.next().and_then(|a| u32::from_str_radix(a.trim_start_matches("0x"), 16).ok());
                            let len = it.next().and_then(|l| l.parse::<u32>().ok()).unwrap_or(64).min(4096);
                            match addr {
                                Some(a) => (0..len)
                                    .map(|i| match x86::interp::translate(&mut cpu, &mut board, a + i, false) {
                                        Ok(p) => format!("{:02x}", x86::bus::Bus::read8(&mut board, p)),
                                        Err(_) => "??".into(),
                                    })
                                    .collect::<Vec<_>>()
                                    .chunks(16)
                                    .enumerate()
                                    .map(|(i, c)| format!("{:08x}: {}", a + 16 * i as u32, c.join(" ")))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                None => "usage: mem HEXADDR [LEN]".into(),
                            }
                        }
                        _ => format!("unknown query {cmd:?}"),
                    };
                    let _ = reply.send(answer);
                }
                Event::Quit => break 'run ExitCode::SUCCESS,
            }
        }
        if paused {
            std::thread::sleep(Duration::from_millis(5));
            continue;
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
        if let Some(text) = &args.exit_on_text
            && Instant::now() >= next_frame
            && text_screen(&mut board).contains(text.as_str())
        {
            eprintln!("found {text:?} after {:.2}s", started.elapsed().as_secs_f64());
            break ExitCode::SUCCESS;
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
    if let Some(addr) = std::env::var("SLOPPER_DUMP_STR").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
        let mut text = String::new();
        for i in 0..256 {
            let Ok(p) = x86::interp::translate(&mut cpu, &mut board, addr + i, false) else { break };
            let b = x86::bus::Bus::read8(&mut board, p);
            if b == 0 {
                break;
            }
            text.push(if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' });
        }
        eprintln!("string at {addr:#x}: {text}");
    }
    eprintln!(
        "ran {total} instructions ({jit_insns} translated) in {secs:.1}s ({:.1} MIPS)",
        total as f64 / secs / 1e6
    );
    if let Some(j) = &jit {
        eprintln!("{:?}", j.stats);
        if j.interp_profile.is_some() {
            eprintln!("{}", j.profile_report());
        }
    }
    dump(&cpu);
    shared.done.store(true, Ordering::Relaxed);
    code
}

/// Host time spent running translated code, interpreting, and sleeping in HLT.
#[derive(Default, Clone, Copy)]
struct HostTime {
    jit: Duration,
    interp: Duration,
    halted: Duration,
}

fn format_regs(cpu: &Cpu) -> String {
    let r = &cpu.regs;
    let s = &cpu.segs;
    format!(
        "eax={:08x} ecx={:08x} edx={:08x} ebx={:08x} esp={:08x} ebp={:08x} esi={:08x} edi={:08x}\n\
         eip={:08x} eflags={:08x} cs={:04x} ss={:04x} ds={:04x} es={:04x} fs={:04x} cr0={:08x} cr3={:08x}",
        r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7],
        cpu.eip, cpu.eflags, s[1].selector, s[2].selector, s[3].selector, s[0].selector, s[4].selector, cpu.cr0, cpu.cr3
    )
}

/// Serves the control socket: one command per line, the answer followed by a
/// line containing a single `.`. Input commands are the same as on stdin.
fn control_server(path: PathBuf, tx: Sender<Event>) {
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {}: {e}", path.display());
            return;
        }
    };
    eprintln!("control socket at {}", path.display());
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut out = match conn.try_clone() {
                Ok(c) => c,
                Err(_) => return,
            };
            for line in std::io::BufReader::new(conn).lines() {
                let Ok(line) = line else { break };
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let Some(answer) = run_command(&line, &tx) else { break };
                if writeln!(out, "{answer}\n.").is_err() {
                    break;
                }
            }
        });
    }
}

/// Runs one stdin or control-socket command and returns its answer, or None
/// once the emulator has stopped.
fn run_command(line: &str, tx: &Sender<Event>) -> Option<String> {
    let cmd = line.split_whitespace().next().unwrap_or("");
    if matches!(cmd, "screen" | "regs" | "status" | "pause" | "resume" | "mem" | "profile") {
        let (rtx, rrx) = channel();
        tx.send(Event::Query(line.to_string(), rtx)).ok()?;
        return Some(rrx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| "no answer".into()));
    }
    Some(match command_event(line) {
        Ok(evs) => {
            for e in evs {
                tx.send(e).ok()?;
            }
            "ok".into()
        }
        Err(e) => e,
    })
}

/// Parses an input command (`shot`, `type`, `key`, `mouse`, `quit`) into events.
fn command_event(line: &str) -> Result<Vec<Event>, String> {
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    Ok(match cmd {
        "shot" => vec![Event::Screenshot(PathBuf::from(if rest.is_empty() { "screen.png" } else { rest }))],
        "type" => vec![Event::Type(rest.to_string())],
        "key" => {
            let codes: Option<Vec<u16>> = rest.split('+').map(keys::by_name).collect();
            let codes = codes.ok_or_else(|| format!("unknown key in {rest}"))?;
            let mut evs: Vec<Event> = codes.iter().map(|&c| Event::Key(c, true)).collect();
            evs.extend(codes.iter().rev().map(|&c| Event::Key(c, false)));
            evs
        }
        "mouse" => {
            let v: Vec<i32> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            match v.as_slice() {
                [dx, dy] => vec![Event::Mouse(*dx, *dy, 0)],
                [dx, dy, b] => vec![Event::Mouse(*dx, *dy, *b as u8)],
                _ => return Err("usage: mouse DX DY [BUTTONS]".into()),
            }
        }
        "quit" => vec![Event::Quit],
        _ => {
            return Err("commands: screen | status | regs | mem HEXADDR [LEN] | profile [on|off] | pause | resume | shot PATH | type TEXT | key NAME[+NAME] | mouse DX DY [BUTTONS] | quit".into());
        }
    })
}

fn stdin_commands(tx: Sender<Event>) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(answer) = run_command(line, &tx) else { break };
        println!("{answer}");
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
    let control_path = args.control.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("86slopper-{}.sock", std::process::id())));
    let emu_shared = shared.clone();
    let emu = std::thread::spawn(move || emulate(args, cfg, rx, emu_shared));

    let stdin_tx = tx.clone();
    std::thread::spawn(move || stdin_commands(stdin_tx));
    let ctl_tx = tx.clone();
    let ctl_path = control_path.clone();
    std::thread::spawn(move || control_server(ctl_path, ctl_tx));
    #[cfg(feature = "window")]
    if !headless {
        window_loop(tx, shared);
    }
    #[cfg(not(feature = "window"))]
    let _ = (tx, shared);
    let _ = headless;
    let code = emu.join().unwrap_or(ExitCode::FAILURE);
    let _ = std::fs::remove_file(&control_path);
    code
}

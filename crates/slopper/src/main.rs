mod audio;
mod capture;
mod host_power;
mod keys;
mod savestate;
mod vnc;

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pc::battery::{Battery, Power};
use pc::{Board, Config};
use x86::cpu::Cpu;
use x86::interp::{Exit, step};

struct Args {
    bios: String,
    vgabios: Option<String>,
    board: Option<pc::Chipset>,
    radeon: bool,
    vga_rom: Option<String>,
    gpu_trace: Option<String>,
    gpu_3d_metal: bool,
    cdrom: Option<String>,
    hda: Option<String>,
    serial: Option<String>,
    ram_mb: u32,
    max_instructions: Option<u64>,
    headless: bool,
    jit: bool,
    screenshots: Vec<(f64, PathBuf)>,
    exit_on_text: Option<String>,
    script: Option<PathBuf>,
    control: Option<PathBuf>,
    vnc: Option<String>,
    audio: bool,
    /// The user asked for sound explicitly, so failing to start it is an error.
    audio_asked: bool,
    audio_wav: Option<PathBuf>,
    audio_silent: bool,
    audio_latency: u32,
    net: Option<net::user::UserNet>,
    net_asked: bool,
    battery: BatterySource,
    load: Option<savestate::Snapshot>,
    capture_mode: capture::CaptureMode,
}

#[derive(Clone, Copy)]
enum BatterySource {
    Host,
    Fixed(Power),
}

const BATTERY_USAGE: &str = "host | off | PERCENT[:ac]";

/// `host` mirrors the Mac, `off` has no battery on AC power, and `PERCENT`
/// is a fixed level, discharging unless followed by `:ac`.
fn parse_battery(spec: &str) -> Result<BatterySource, String> {
    let bad = || format!("battery must be {BATTERY_USAGE}, not {spec:?}");
    match spec {
        "host" => Ok(BatterySource::Host),
        "off" => Ok(BatterySource::Fixed(Power::default())),
        _ => {
            let (level, ac) = match spec.strip_suffix(":ac") {
                Some(level) => (level, true),
                None => (spec, false),
            };
            let percent: u8 = level.parse().ok().filter(|&p| p <= 100).ok_or_else(bad)?;
            Ok(BatterySource::Fixed(Power { ac_online: ac, battery: Some(Battery::fake(percent, ac)) }))
        }
    }
}

fn describe_power(p: &Power) -> String {
    let ac = if p.ac_online { "AC online" } else { "AC offline" };
    let Some(b) = p.battery else {
        return format!("{ac}, no battery");
    };
    let direction = if b.charging { "charging" } else if p.ac_online { "idle" } else { "discharging" };
    let rate = b.rate_ma.map_or("unknown rate".into(), |r| format!("{r} mA"));
    format!(
        "{ac}, battery {}% {direction} at {rate}: {}/{} mAh (design {} mAh), {} mV (design {} mV)",
        b.percent(),
        b.remaining_mah,
        b.full_mah,
        b.design_mah,
        b.present_mv,
        b.design_mv
    )
}

/// An x86 PC for Windows XP and Linux guests, on Hypervisor.framework.
#[derive(clap::Parser)]
#[command(name = "slopper", version)]
struct Cli {
    /// SeaBIOS image.
    #[arg(long, value_name = "FILE")]
    bios: String,
    /// VGA BIOS for the stdvga display.
    #[arg(long, value_name = "FILE")]
    vgabios: Option<String>,
    /// Chipset: fx (440FX) or bx (440BX with AGP).
    #[arg(long, value_name = "fx|bx", value_parser = parse_board)]
    board: Option<pc::Chipset>,
    /// Display card: stdvga or radeon (a Radeon 9700 on the 440BX's AGP bus).
    #[arg(long, value_name = "stdvga|radeon", default_value = "stdvga", value_parser = ["stdvga", "radeon"])]
    gpu: String,
    /// The Radeon's video BIOS, which you supply.
    #[arg(long, value_name = "FILE")]
    vga_rom: Option<String>,
    /// Log the Radeon's register accesses to FILE.
    #[arg(long, value_name = "FILE")]
    gpu_trace: Option<String>,
    /// Where the Radeon's 3D engine draws: metal (the default) or cpu, the
    /// reference. SLOPPER_R3D sets the default.
    #[arg(long = "gpu-3d", value_name = "metal|cpu", value_parser = ["metal", "cpu"])]
    gpu_3d: Option<String>,
    /// CD-ROM image on the secondary master.
    #[arg(long, value_name = "ISO")]
    cdrom: Option<String>,
    /// Raw disk image on the primary master.
    #[arg(long, value_name = "IMG")]
    hda: Option<String>,
    /// Write COM1 to FILE, or to stderr with `-`.
    #[arg(long, value_name = "FILE|-")]
    serial: Option<String>,
    /// Guest RAM in MiB [default: 256, or the snapshot's with --load].
    #[arg(long = "ram", value_name = "MiB")]
    ram_mb: Option<u32>,
    /// Stop after N instructions.
    #[arg(long, value_name = "N")]
    max_instructions: Option<u64>,
    /// Run without a window.
    #[arg(long)]
    headless: bool,
    /// Interpret every instruction instead of running translated code in the VM.
    #[arg(long)]
    no_jit: bool,
    #[arg(long, hide = true)]
    jit: bool,
    /// Save a screenshot at SECONDS after start; may repeat.
    #[arg(long = "screenshot", value_name = "SECONDS:PATH", value_parser = parse_screenshot)]
    screenshots: Vec<(f64, PathBuf)>,
    /// Exit successfully once TEXT appears on a text-mode screen.
    #[arg(long, value_name = "TEXT")]
    exit_on_text: Option<String>,
    /// Run the input steps in FILE.
    #[arg(long, value_name = "FILE")]
    script: Option<PathBuf>,
    /// Listen for control commands on this Unix socket.
    #[arg(long, value_name = "SOCKET")]
    control: Option<PathBuf>,
    /// Serve the screen over VNC.
    #[arg(long, value_name = "[ADDR:]PORT", value_parser = vnc::parse_addr)]
    vnc: Option<String>,
    /// Leave out the AC'97 sound card.
    #[arg(long, conflicts_with_all = ["audio_wav", "audio_silent"])]
    no_audio: bool,
    #[arg(long, hide = true, value_parser = ["ac97"])]
    audio: Option<String>,
    /// Also record what the guest plays to a WAV file.
    #[arg(long, value_name = "PATH")]
    audio_wav: Option<PathBuf>,
    /// Keep the sound card in real time but play nothing on the Mac.
    #[arg(long)]
    audio_silent: bool,
    /// How far ahead of the speaker the sound card queues audio.
    #[arg(long, value_name = "MS", default_value_t = audio::LATENCY_MS, value_parser = clap::value_parser!(u32).range(10..=300))]
    audio_latency: u32,
    /// Leave out the network card.
    #[arg(long, conflicts_with = "net")]
    no_net: bool,
    /// User-mode network options, e.g. user,hostfwd=tcp::2222-:22.
    #[arg(long, value_name = "user[,hostfwd=...]", value_parser = net::user::UserNet::parse)]
    net: Option<net::user::UserNet>,
    /// Battery the guest sees: host mirrors the Mac, or a fixed PERCENT[:ac].
    #[arg(long, value_name = BATTERY_USAGE, value_parser = parse_battery, conflicts_with = "no_battery")]
    battery: Option<BatterySource>,
    /// No battery: the guest runs on AC power.
    #[arg(long)]
    no_battery: bool,
    /// Start from the snapshot in DIR (made with the control socket's save) instead of booting.
    #[arg(long, value_name = "DIR", conflicts_with = "hda")]
    load: Option<PathBuf>,
    /// What captures the mouse besides Ctrl+Option+G: key (nothing else), click
    /// (a click in the window) or auto (a click while the guest has no tablet driver).
    #[arg(long, value_name = "key|click|auto", default_value = "key", value_parser = capture::parse_mode)]
    capture_mouse: capture::CaptureMode,
}

fn parse_board(v: &str) -> Result<pc::Chipset, String> {
    match v {
        "fx" => Ok(pc::Chipset::I440fx),
        "bx" => Ok(pc::Chipset::I440bx),
        _ => Err("expected fx or bx".into()),
    }
}

fn parse_screenshot(v: &str) -> Result<(f64, PathBuf), String> {
    let (secs, path) = v.split_once(':').ok_or("expected SECONDS:PATH")?;
    Ok((secs.parse().map_err(|_| "expected SECONDS:PATH")?, PathBuf::from(path)))
}

fn parse_args() -> Result<Args, String> {
    let cli = <Cli as clap::Parser>::parse();
    let radeon = cli.gpu == "radeon";
    if radeon {
        if cli.board == Some(pc::Chipset::I440fx) {
            return Err("--gpu radeon needs the 440BX's AGP bus, so it cannot go with --board fx".into());
        }
        if cli.vga_rom.is_none() {
            return Err("--gpu radeon needs --vga-rom with the card's video BIOS".into());
        }
    } else if cli.vga_rom.is_some() || cli.gpu_trace.is_some() {
        return Err("--vga-rom and --gpu-trace go with --gpu radeon".into());
    }
    let net_asked = cli.net.is_some();
    let load = cli.load.as_deref().map(savestate::read).transpose()?;
    let ram_mb = cli.ram_mb.or(load.as_ref().map(|s| s.machine.ram_bytes >> 20)).unwrap_or(256);
    Ok(Args {
        bios: cli.bios,
        vgabios: cli.vgabios,
        board: cli.board,
        radeon,
        vga_rom: cli.vga_rom,
        gpu_trace: cli.gpu_trace,
        gpu_3d_metal: cli.gpu_3d.or_else(|| std::env::var("SLOPPER_R3D").ok()).as_deref() != Some("cpu"),
        cdrom: cli.cdrom,
        hda: cli.hda,
        serial: cli.serial,
        ram_mb,
        max_instructions: cli.max_instructions,
        headless: cli.headless,
        jit: !cli.no_jit,
        screenshots: cli.screenshots,
        exit_on_text: cli.exit_on_text,
        script: cli.script,
        control: cli.control,
        vnc: cli.vnc,
        audio: !cli.no_audio,
        audio_asked: cli.audio.is_some() || cli.audio_wav.is_some() || cli.audio_silent,
        audio_wav: cli.audio_wav,
        audio_silent: cli.audio_silent,
        audio_latency: cli.audio_latency,
        net: (!cli.no_net).then(|| cli.net.unwrap_or_default()),
        net_asked,
        battery: if cli.no_battery {
            BatterySource::Fixed(Power::default())
        } else {
            cli.battery.unwrap_or(BatterySource::Host)
        },
        load,
        capture_mode: cli.capture_mouse,
    })
}

enum Event {
    Key(u16, bool),
    Type(String),
    /// Bytes for the guest to receive on COM1.
    Serial(Vec<u8>),
    /// PS/2 mouse motion (positive dy is up), wheel detents and buttons.
    Mouse(i32, i32, i32, u8),
    /// Host pointer: absolute position (0..=`usb::MAX_COORD` on each axis),
    /// relative motion for PS/2, buttons and wheel detents.
    Pointer { x: u16, y: u16, dx: i32, dy: i32, buttons: u8, wheel: i32 },
    /// Tablet position in guest screen pixels, buttons and wheel detents.
    Tablet(u32, u32, u8, i32),
    Screenshot(PathBuf),
    /// A control-socket query answered by the emulator thread.
    Query(String, Sender<String>),
    PowerButton,
    HostPower(Power),
    Battery(BatterySource),
    Quit,
}

#[derive(Default)]
struct Frame {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    seq: u64,
}

#[derive(Default)]
struct Shared {
    frame: Mutex<Frame>,
    done: AtomicBool,
    instructions: AtomicU64,
    /// The guest gets relative PS/2 motion only and tablet input is dropped.
    mouse_captured: AtomicBool,
    tablet_in_use: AtomicBool,
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
    if let Some(path) = args.cdrom.as_ref().filter(|_| args.load.is_none())
        && let Err(e) = board.attach_cdrom(path)
    {
        eprintln!("cannot open {path}: {e}");
        return ExitCode::FAILURE;
    }
    if args.audio {
        match audio::HostAudio::new(!args.audio_silent, args.audio_wav.as_deref(), args.audio_latency) {
            Ok(out) => board.add_ac97(Box::new(out)),
            Err(e) if args.audio_asked => {
                eprintln!("cannot start audio: {e}");
                return ExitCode::FAILURE;
            }
            Err(e) => eprintln!("no sound card: {e}"),
        }
    }
    if let Some(path) = &args.hda
        && let Err(e) = board.attach_disk(path)
    {
        eprintln!("cannot open {path}: {e}");
        return ExitCode::FAILURE;
    }
    if let Some(path) = &args.serial {
        let out: Box<dyn std::io::Write + Send> = if path == "-" {
            Box::new(std::io::stderr())
        } else {
            match std::fs::File::create(path) {
                Ok(f) => Box::new(f),
                Err(e) => {
                    eprintln!("cannot open {path}: {e}");
                    return ExitCode::FAILURE;
                }
            }
        };
        board.com1.set_output(out);
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
    let start_net = {
        let emu = std::thread::current();
        let kicker = jit.as_ref().map(|j| j.kicker());
        move |cfg: &net::user::UserNet| {
            let emu = emu.clone();
            let wake = move || {
                emu.unpark();
                if let Some(k) = &kicker {
                    k.kick();
                }
            };
            net::start_user_network(cfg, Box::new(wake))
        }
    };
    if std::env::var_os("SLOPPER_AUDIO_SYNC").is_none() {
        let emu = std::thread::current();
        let kicker = jit.as_ref().map(|j| j.kicker());
        board.start_audio_thread(Box::new(move || {
            emu.unpark();
            if let Some(k) = &kicker {
                k.kick();
            }
        }));
    }
    if let Some(cfg) = &args.net {
        match start_net(cfg) {
            Ok(backend) => board.attach_nic(net::DEFAULT_MAC, backend),
            Err(e) if args.net_asked => {
                eprintln!("cannot start the network: {e}");
                return ExitCode::FAILURE;
            }
            Err(e) => eprintln!("no network card: {e}"),
        }
    }
    let mut cpu = Cpu::new();
    cpu.start_host_tsc();
    let mut media = savestate::Media { hda: args.hda.as_ref().map(PathBuf::from) };
    if let Some(snap) = args.load.take() {
        match savestate::restore(&snap, &mut cpu, &mut board, args.cdrom.as_deref().map(Path::new)) {
            Ok((m, msg)) => {
                eprintln!("{msg}");
                media = m;
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let started = Instant::now();
    let mut executed: u64 = 0;
    let mut keyq = KeyQueue::default();
    let mut mirror_host = matches!(args.battery, BatterySource::Host);
    let mut paused = false;
    let mut last_status = (Instant::now(), 0u64);
    let mut time = HostTime::default();
    let mut last_time = HostTime::default();
    let mut last_stats = jit::Stats::default();
    let mut last_apic = pc::apic::Stats::default();
    let mut last_nic = net::rtl8139::Stats::default();
    let mut script = match args.script.as_ref().map(Script::load).transpose() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read script: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut next_frame = Instant::now();
    let mut scratch = Vec::new();
    // Sleeps of the emulation thread that `stall` asks for: how long, how often, and the next one.
    let mut stall: Option<(Duration, Duration, Instant)> = None;

    let code = 'run: loop {
        let mut outcome = None;
        let irq_due = cpu.flag(x86::cpu::flags::IF) && x86::bus::Bus::intr(&mut board);
        if let Some(j) = jit.as_mut()
            && jit::supported(&cpu)
            && !cpu.halted
            && !irq_due
        {
            let t = Instant::now();
            let fb = board.framebuffer();
            let r = unsafe { j.set_framebuffer(fb) }
                .and_then(|_| j.set_shadowed_rom(board.shadowed_rom()))
                .and_then(|_| j.run(&mut cpu, &mut board));
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
                std::thread::park_timeout(Duration::from_nanos(wait));
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
        board.set_irq(13, cpu.ferr_pending);
        board.tick();
        if let Some((len, every, next)) = stall.as_mut()
            && Instant::now() >= *next
        {
            std::thread::sleep(*len);
            *next = Instant::now() + *every;
        }
        if board.reset_requested {
            cpu.reset();
            board.reset();
        }
        if board.power_off_requested() {
            eprintln!("guest powered off");
            break ExitCode::SUCCESS;
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
        shared.tablet_in_use.store(board.tablet_in_use(), Ordering::Relaxed);
        while let Ok(ev) = events.try_recv() {
            match ev {
                Event::Key(code, down) => board.key(code, down),
                Event::Type(text) => text.chars().for_each(|c| keyq.push_char(c)),
                Event::Serial(bytes) => board.serial_input(&bytes),
                Event::Mouse(dx, dy, wheel, b) => board.mouse_motion(dx, dy, wheel, b),
                // A guest with a USB tablet driver gets absolute positions only:
                // feeding the same motion through PS/2 as well would move the
                // cursor twice and send every click twice.
                Event::Pointer { x, y, dx, dy, buttons, wheel } => {
                    if board.tablet_in_use() && !shared.mouse_captured.load(Ordering::Relaxed) {
                        board.tablet(x, y, buttons, wheel);
                    } else {
                        board.mouse_motion(dx, dy, wheel, buttons);
                    }
                }
                Event::Tablet(..) if shared.mouse_captured.load(Ordering::Relaxed) => {}
                Event::Tablet(px, py, buttons, wheel) => {
                    let (w, h) = {
                        let f = shared.frame.lock().unwrap();
                        (f.width, f.height)
                    };
                    board.tablet(pixel_to_tablet(px, w), pixel_to_tablet(py, h), buttons, wheel);
                }
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
                        "cdrom" if rest.trim().is_empty() => board.cdrom().unwrap_or("empty").to_string(),
                        "cdrom" => match board.change_cdrom(Some(rest.trim())) {
                            Ok(()) => "ok".into(),
                            Err(e) => format!("cannot open {}: {e}", rest.trim()),
                        },
                        "eject" => {
                            let _ = board.change_cdrom(None);
                            "ok".into()
                        }
                        "regs" => format_regs(&cpu),
                        "battery" => {
                            let source = if mirror_host { "mirroring the host" } else { "fixed" };
                            format!("{} ({source})", describe_power(&board.power()))
                        }
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
                            let [m, sl] = board.pic.registers();
                            out += &format!(
                                "\n{host} timer backlog {} pic irr/isr/imr {:02x}/{:02x}/{:02x} {:02x}/{:02x}/{:02x}",
                                board.timer_backlog(),
                                m.0,
                                m.1,
                                m.2,
                                sl.0,
                                sl.1,
                                sl.2
                            );
                            if let Some(a) = board.describe_audio() {
                                out += &format!("\n{a}");
                            }
                            if let Some(g) = board.describe_gpu() {
                                out += &format!("\n{g}");
                            }
                            if let Some(l) = &board.lapic {
                                let (a, b) = (l.stats, last_apic);
                                let r = |x: u64, y: u64| (x - y) as f64 / span;
                                out += &format!(
                                    "\napic/s: reads {:.0} writes {:.0} tpr reads {:.0} tpr writes {:.0} eoi {:.0} | ppr {:02x}",
                                    r(a.reads, b.reads),
                                    r(a.writes, b.writes),
                                    r(a.tpr_reads, b.tpr_reads),
                                    r(a.tpr_writes, b.tpr_writes),
                                    r(a.eois, b.eois),
                                    l.ppr()
                                );
                                last_apic = a;
                            }
                            if let Some(a) = board.nic_stats() {
                                let b = last_nic;
                                let frames = (a.tx_frames + a.rx_frames - b.tx_frames - b.rx_frames).max(1);
                                out += &format!(
                                    "\nnic/s: tx {:.0} rx {:.0} dropped {:.0} | register accesses per frame {:.1}",
                                    (a.tx_frames - b.tx_frames) as f64 / span,
                                    (a.rx_frames - b.rx_frames) as f64 / span,
                                    (a.rx_dropped - b.rx_dropped) as f64 / span,
                                    (a.reads + a.writes - b.reads - b.writes) as f64 / frames as f64,
                                );
                                last_nic = a;
                            }
                            if let Some(j) = jit.as_mut() {
                                let (a, b) = (j.stats, last_stats);
                                let r = |x: u64, y: u64| (x - y) as f64 / span;
                                out += &format!(
                                    "\nexits/s: interp {:.0} miss {:.0} fault {:.0} mmio {:.0} | in-VM tpr/s {:.0} | translations/s {:.0} smc/s {:.0} flushes/s {:.0} guest #PF/s {:.0}",
                                    r(a.interp_exits, b.interp_exits),
                                    r(a.miss_exits, b.miss_exits),
                                    r(a.shadow_fills + a.page_faults, b.shadow_fills + b.page_faults),
                                    r(a.mmio_exits, b.mmio_exits),
                                    r(a.tpr_fast, b.tpr_fast),
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
                        "save" if !rest.is_empty() => savestate::save(Path::new(rest), &cpu, &mut board, &media).unwrap_or_else(|e| e),
                        "load" if !rest.is_empty() => match savestate::read(Path::new(rest)).and_then(|s| savestate::restore(&s, &mut cpu, &mut board, None)) {
                            Ok((m, mut msg)) => {
                                media = m;
                                if let Some(j) = jit.as_mut() {
                                    j.forget_guest();
                                }
                                keyq = KeyQueue::default();
                                if let Some(cfg) = &args.net {
                                    board.replace_nic_backend(|| start_net(cfg).map_err(|e| eprintln!("cannot restart the network: {e}")).ok());
                                    msg += ", network card on a fresh user-mode network";
                                }
                                eprintln!("{msg}");
                                msg
                            }
                            Err(e) => e,
                        },
                        "save" | "load" => format!("usage: {cmd} DIR"),
                        "capture" => {
                            let c = &shared.mouse_captured;
                            let now = c.load(Ordering::Relaxed);
                            let set = match rest.trim() {
                                "" => Some(now),
                                "on" => Some(true),
                                "off" => Some(false),
                                "toggle" => Some(!now),
                                _ => None,
                            };
                            if let Some(v) = set {
                                c.store(v, Ordering::Relaxed);
                            }
                            if set.is_none() {
                                "usage: capture [on|off|toggle]".into()
                            } else if c.load(Ordering::Relaxed) {
                                "mouse captured: PS/2 motion only, tablet input dropped".into()
                            } else {
                                "mouse released".into()
                            }
                        }
                        "stall" => {
                            let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                            match v.as_slice() {
                                [0] => {
                                    stall = None;
                                    "not stalling".into()
                                }
                                &[ms, every] if ms > 0 => {
                                    stall = Some((Duration::from_millis(ms), Duration::from_millis(every), Instant::now()));
                                    format!("stalling {ms} ms every {every} ms")
                                }
                                _ => "usage: stall MS EVERY_MS | stall 0".into(),
                            }
                        }
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
                Event::PowerButton => board.power_button(),
                Event::HostPower(p) => {
                    if mirror_host {
                        board.set_power(p);
                    }
                }
                Event::Battery(BatterySource::Host) => mirror_host = true,
                Event::Battery(BatterySource::Fixed(p)) => {
                    mirror_host = false;
                    board.set_power(p);
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
            if jit.as_mut().is_some_and(|j| j.take_framebuffer_written().unwrap_or(true)) {
                board.framebuffer_written();
            }
            if board.display_changed()
                && let Some((w, h)) = board.render(&mut scratch)
            {
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
        if j.profiling() {
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

/// Tablet coordinate for the middle of pixel `p` on an axis `size` pixels long.
fn pixel_to_tablet(p: u32, size: u32) -> u16 {
    let size = size.max(1) as u64;
    let range = usb::MAX_COORD as u64 + 1;
    ((2 * p as u64 + 1) * range / (2 * size)).min(usb::MAX_COORD as u64) as u16
}

fn format_regs(cpu: &Cpu) -> String {
    let r = &cpu.regs;
    let s = &cpu.segs;
    format!(
        "eax={:08x} ecx={:08x} edx={:08x} ebx={:08x} esp={:08x} ebp={:08x} esi={:08x} edi={:08x}\n\
         eip={:08x} eflags={:08x} cs={:04x} ss={:04x} ds={:04x} es={:04x} fs={:04x} cr0={:08x} cr3={:08x}\n\
         fcw={:04x} fsw={:04x} ftags={:02x} mxcsr={:08x} cr4={:08x}",
        r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7],
        cpu.eip, cpu.eflags, s[1].selector, s[2].selector, s[3].selector, s[0].selector, s[4].selector, cpu.cr0, cpu.cr3,
        cpu.fpu.cw, cpu.fpu.sw, cpu.fpu.tags, cpu.mxcsr, cpu.cr4
    )
}

/// The inode of the control socket this process bound.
static CONTROL_INODE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

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
    if let Ok(m) = std::fs::metadata(&path) {
        let _ = CONTROL_INODE.set(std::os::unix::fs::MetadataExt::ino(&m));
    }
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
    let battery_query = cmd == "battery" && line.split_whitespace().nth(1).is_none();
    if battery_query || matches!(cmd, "screen" | "regs" | "status" | "capture" | "pause" | "resume" | "mem" | "profile" | "cdrom" | "eject" | "save" | "load" | "stall") {
        let (rtx, rrx) = channel();
        tx.send(Event::Query(line.to_string(), rtx)).ok()?;
        let wait = Duration::from_secs(if matches!(cmd, "save" | "load") { 120 } else { 10 });
        return Some(rrx.recv_timeout(wait).unwrap_or_else(|_| "no answer".into()));
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

/// Parses an input command (`shot`, `type`, `serial`, `key`, `mouse`, `tablet`, `power`, `quit`) into events.
fn command_event(line: &str) -> Result<Vec<Event>, String> {
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    Ok(match cmd {
        "shot" => vec![Event::Screenshot(PathBuf::from(if rest.is_empty() { "screen.png" } else { rest }))],
        "type" => vec![Event::Type(rest.to_string())],
        "serial" => vec![Event::Serial(format!("{rest}\r").into_bytes())],
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
                [dx, dy] => vec![Event::Mouse(*dx, *dy, 0, 0)],
                [dx, dy, b] => vec![Event::Mouse(*dx, *dy, 0, *b as u8)],
                [dx, dy, b, w] => vec![Event::Mouse(*dx, *dy, *w, *b as u8)],
                _ => return Err("usage: mouse DX DY [BUTTONS [WHEEL]]".into()),
            }
        }
        "tablet" => {
            let v: Vec<i64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            match v.as_slice() {
                &[x, y, ref more @ ..] if x >= 0 && y >= 0 && more.len() <= 2 => {
                    let buttons = more.first().copied().unwrap_or(0) as u8;
                    let wheel = more.get(1).copied().unwrap_or(0) as i32;
                    vec![Event::Tablet(x as u32, y as u32, buttons, wheel)]
                }
                _ => return Err("usage: tablet X Y [BUTTONS [WHEEL]] (guest screen pixels)".into()),
            }
        }
        "power" => vec![Event::PowerButton],
        "battery" => vec![Event::Battery(parse_battery(rest.trim())?)],
        "quit" => vec![Event::Quit],
        _ => {
            return Err("commands: screen | status | regs | mem HEXADDR [LEN] | profile [on|off] | pause | resume | save DIR | load DIR | shot PATH | type TEXT | serial LINE | key NAME[+NAME] | mouse DX DY [BUTTONS [WHEEL]] | tablet X Y [BUTTONS [WHEEL]] | capture [on|off|toggle] | power | battery [host|off|PERCENT[:ac]] | cdrom [ISO] | eject | stall MS EVERY_MS | quit\n\
                 mouse sends relative PS/2 motion; tablet moves the USB tablet to guest screen pixel (X, Y);\n\
                 capture sends the window's mouse as PS/2 motion only and drops tablet input;\n\
                 stall sleeps the emulation thread for MS every EVERY_MS, to test devices under load".into());
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
const RELEASE_HINT: &str = "mouse captured, Ctrl+Option+G releases";

#[cfg(feature = "window")]
fn window_loop(tx: Sender<Event>, shared: Arc<Shared>, capture_mode: capture::CaptureMode) {
    use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
    let open = |w: usize, h: usize| {
        let opts = WindowOptions { resize: true, scale_mode: minifb::ScaleMode::AspectRatioStretch, ..Default::default() };
        let (ww, wh) = window_size_for(w, h);
        let mut window = Window::new("86slopper", ww, wh, opts).map_err(|e| eprintln!("cannot open window: {e}")).ok()?;
        window.set_target_fps(60);
        Some(window)
    };
    let Some(mut window) = open(720, 400) else { return };
    let mut window_for = (720usize, 400usize);
    let mut seq = 0;
    let mut buf = Vec::new();
    let (mut w, mut h) = (720usize, 400usize);
    let mut last_mouse: Option<(f32, f32, u8)> = None;
    let mut wheel_acc = 0f32;
    let mut rate_at = (Instant::now(), 0u64);
    let mut mips = 0.0;
    let mut grab = capture::Grab::default();
    let mut motion = capture::Accumulator::default();
    let mut swallow_g = false;
    let mut swallow_click = false;
    let mut last_buttons = 0u8;
    while window.is_open() && !shared.done.load(Ordering::Relaxed) {
        let elapsed = rate_at.0.elapsed();
        let mut retitle = false;
        if elapsed >= Duration::from_millis(500) {
            let n = shared.instructions.load(Ordering::Relaxed);
            mips = n.saturating_sub(rate_at.1) as f64 / elapsed.as_secs_f64() / 1e6;
            rate_at = (Instant::now(), n);
            retitle = true;
        }
        {
            let f = shared.frame.lock().unwrap();
            if f.seq != seq && f.width > 0 {
                seq = f.seq;
                buf.clone_from(&f.pixels);
                (w, h) = (f.width as usize, f.height as usize);
            }
        }
        if (w, h) != window_for && w >= 320 && h >= 200 {
            window_for = (w, h);
            let (ww, wh) = window_size_for(w, h);
            let at = window.get_position();
            capture::set_content_size(window.get_window_handle(), ww as f64, wh as f64);
            window.set_position(at.0, at.1);
            last_mouse = None;
        }
        if buf.len() == w * h && !buf.is_empty() {
            let _ = window.update_with_buffer(&buf, w, h);
        } else {
            window.update();
        }

        let held = |a: Key, b: Key| window.is_key_down(a) || window.is_key_down(b);
        let hot_mods = held(Key::LeftCtrl, Key::RightCtrl) && held(Key::LeftAlt, Key::RightAlt);
        let mut toggle = false;
        for k in window.get_keys_pressed(KeyRepeat::No) {
            if k == Key::G && hot_mods {
                swallow_g = true;
                toggle = true;
            } else if let Some(c) = keys::from_minifb(k) {
                let _ = tx.send(Event::Key(c, true));
            }
        }
        for k in window.get_keys_released() {
            if k == Key::G && std::mem::take(&mut swallow_g) {
                continue;
            }
            if let Some(c) = keys::from_minifb(k) {
                let _ = tx.send(Event::Key(c, false));
            }
        }

        let buttons = window.get_mouse_down(MouseButton::Left) as u8
            | (window.get_mouse_down(MouseButton::Right) as u8) << 1
            | (window.get_mouse_down(MouseButton::Middle) as u8) << 2;
        let clicked = buttons & 1 != 0 && last_buttons & 1 == 0;
        last_buttons = buttons;
        let focused = capture::app_active();
        let mut want = shared.mouse_captured.load(Ordering::Relaxed);
        if toggle {
            want = !want;
        }
        let click_captures = match capture_mode {
            capture::CaptureMode::Key => false,
            capture::CaptureMode::Click => true,
            capture::CaptureMode::Auto => !shared.tablet_in_use.load(Ordering::Relaxed),
        };
        if !want && clicked && click_captures && focused && window.get_mouse_pos(MouseMode::Discard).is_some() {
            want = true;
            swallow_click = true;
        }
        if !focused {
            want = false;
        }
        shared.mouse_captured.store(want, Ordering::Relaxed);
        if want != grab.is_on() {
            if want {
                let (x, y) = window.get_position();
                let (ww, wh) = window.get_size();
                grab.engage((x as f64 + ww as f64 / 2.0, y as f64 + TITLE_BAR + wh as f64 / 2.0));
                motion.clear();
            } else {
                grab.release();
                last_mouse = None;
            }
            retitle = true;
        }
        if retitle {
            let state = if grab.is_on() { format!(" | {RELEASE_HINT}") } else { String::new() };
            window.set_title(&format!("86slopper | {mips:.1} MIPS{state}"));
        }

        wheel_acc += window.get_scroll_wheel().map_or(0.0, |(_, sy)| sy);
        let wheel = wheel_acc.trunc();
        wheel_acc -= wheel;
        if grab.is_on() {
            if swallow_click && buttons & 1 == 0 {
                swallow_click = false;
            }
            let buttons = if swallow_click { buttons & !1 } else { buttons };
            let (hx, hy) = grab.take_motion();
            let (dx, dy) = motion.add(hx, hy);
            if dx != 0 || dy != 0 || wheel != 0.0 || Some(buttons) != last_mouse.map(|m| m.2) {
                let _ = tx.send(Event::Mouse(dx, -dy, wheel as i32, buttons));
            }
            last_mouse = Some((0.0, 0.0, buttons));
            continue;
        }
        if let Some((x, y)) = window.get_unscaled_mouse_pos(MouseMode::Discard) {
            if let Some((lx, ly, lb)) = last_mouse
                && (lx != x || ly != y || lb != buttons || buttons != 0 || wheel != 0.0)
            {
                let (ax, ay) = picture_position(x, y, window.get_size(), (w, h));
                let _ = tx.send(Event::Pointer {
                    x: ax,
                    y: ay,
                    dx: (x - lx) as i32,
                    dy: (ly - y) as i32,
                    buttons,
                    wheel: wheel as i32,
                });
            }
            last_mouse = Some((x, y, buttons));
        }
    }
    let _ = tx.send(Event::Quit);
}

/// Height of a standard macOS title bar, in points.
#[cfg(feature = "window")]
const TITLE_BAR: f64 = 28.0;

/// A window size for a `w` x `h` picture: the picture's own size, scaled
/// down to fit the main display if it is larger.
#[cfg(feature = "window")]
fn window_size_for(w: usize, h: usize) -> (usize, usize) {
    #[repr(C)]
    struct Rect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayBounds(display: u32) -> Rect;
    }
    let screen = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    let scale = (screen.w * 0.95 / w as f64).min(screen.h * 0.9 / h as f64).min(1.0);
    ((w as f64 * scale) as usize, (h as f64 * scale) as usize)
}

/// Maps a point in the window to tablet coordinates in the picture, which
/// `ScaleMode::AspectRatioStretch` letterboxes into the middle of the window.
#[cfg(feature = "window")]
fn picture_position(x: f32, y: f32, window: (usize, usize), picture: (usize, usize)) -> (u16, u16) {
    let (ww, wh) = (window.0.max(1) as f32, window.1.max(1) as f32);
    let (pw, ph) = (picture.0.max(1) as f32, picture.1.max(1) as f32);
    let scale = (ww / pw).min(wh / ph);
    let (ox, oy) = ((ww - pw * scale) / 2.0, (wh - ph * scale) / 2.0);
    let to_tablet = |v: f32, origin: f32, len: f32| {
        let f = ((v - origin) / (len * scale)).clamp(0.0, 1.0);
        (f * (usb::MAX_COORD as f32 + 1.0)).min(usb::MAX_COORD as f32) as u16
    };
    (to_tablet(x, ox, pw), to_tablet(y, oy, ph))
}

const HOST_POWER_INTERVAL: Duration = Duration::from_secs(2);

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
    let gpu = match (&args.radeon, &args.vga_rom) {
        (true, Some(path)) => {
            let rom = match read(path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            };
            let trace = match args.gpu_trace.as_deref().map(std::fs::File::create).transpose() {
                Ok(f) => f.map(|f| Box::new(std::io::LineWriter::new(f)) as Box<dyn std::io::Write + Send>),
                Err(e) => {
                    eprintln!("cannot create the GPU trace: {e}");
                    return ExitCode::FAILURE;
                }
            };
            pc::Gpu::Radeon { rom, trace, metal: args.gpu_3d_metal }
        }
        _ => pc::Gpu::StdVga,
    };
    let chipset = args.board.unwrap_or(if args.radeon { pc::Chipset::I440bx } else { pc::Chipset::I440fx });
    let power = match args.battery {
        BatterySource::Host => host_power::read(),
        BatterySource::Fixed(p) => p,
    };
    let cfg = Config { ram_bytes: args.ram_mb << 20, bios, vgabios, apic: true, chipset, gpu, power };
    let shared = Arc::new(Shared::default());
    let (tx, rx) = channel();
    let headless = args.headless || !cfg!(feature = "window");
    let control_path = args.control.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("86slopper-{}.sock", std::process::id())));
    if let Some(addr) = &args.vnc {
        let listener = match std::net::TcpListener::bind(addr) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("cannot listen for VNC on {addr}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let password = std::env::var(vnc::PASSWORD_VAR).ok().filter(|p| !p.is_empty());
        let local = listener.local_addr().is_ok_and(|a| a.ip().is_loopback());
        match &password {
            Some(p) if p.len() > 8 => eprintln!("vnc: only the first 8 bytes of {} count", vnc::PASSWORD_VAR),
            None if !local => eprintln!("vnc: warning: anyone who can reach {addr} gets the VM; set {}", vnc::PASSWORD_VAR),
            _ => {}
        }
        eprintln!("VNC server on {addr}{}", if password.is_some() { " (password set)" } else { "" });
        let (tx, shared) = (tx.clone(), shared.clone());
        std::thread::spawn(move || vnc::serve(listener, tx, shared, password));
    }
    host_power::poll(HOST_POWER_INTERVAL, tx.clone(), Event::HostPower);
    let capture_mode = args.capture_mode;
    let emu_shared = shared.clone();
    let emu = std::thread::spawn(move || emulate(args, cfg, rx, emu_shared));

    let stdin_tx = tx.clone();
    std::thread::spawn(move || stdin_commands(stdin_tx));
    let ctl_tx = tx.clone();
    let ctl_path = control_path.clone();
    std::thread::spawn(move || control_server(ctl_path, ctl_tx));
    #[cfg(feature = "window")]
    if !headless {
        window_loop(tx, shared, capture_mode);
    }
    #[cfg(not(feature = "window"))]
    let _ = (tx, shared);
    let _ = headless;
    let code = emu.join().unwrap_or(ExitCode::FAILURE);
    if std::fs::metadata(&control_path).is_ok_and(|m| CONTROL_INODE.get() == Some(&std::os::unix::fs::MetadataExt::ino(&m))) {
        let _ = std::fs::remove_file(&control_path);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(spec: &str) -> Power {
        match parse_battery(spec) {
            Ok(BatterySource::Fixed(p)) => p,
            _ => panic!("{spec} should be a fixed battery"),
        }
    }

    #[test]
    fn a_bare_percentage_is_a_discharging_battery() {
        let p = fixed("42");
        assert!(!p.ac_online);
        assert_eq!(p.battery.unwrap().percent(), 42);
    }

    #[test]
    fn an_ac_suffix_charges_the_battery() {
        let p = fixed("42:ac");
        assert!(p.ac_online && p.battery.unwrap().charging);
    }

    #[test]
    fn off_means_no_battery_on_ac() {
        assert_eq!(fixed("off"), Power::default());
    }

    #[test]
    fn battery_levels_above_100_or_garbage_are_rejected() {
        for spec in ["101", "-1", "", "42:dc", "full"] {
            assert!(parse_battery(spec).is_err(), "{spec:?} was accepted");
        }
        assert!(matches!(parse_battery("host"), Ok(BatterySource::Host)));
    }

    #[test]
    fn pixels_map_to_tablet_coordinates() {
        assert_eq!(pixel_to_tablet(0, 800), 20);
        assert_eq!(pixel_to_tablet(400, 800), 0x4014);
        assert_eq!(pixel_to_tablet(799, 800), 0x7feb);
        assert_eq!(pixel_to_tablet(5000, 800), usb::MAX_COORD);
        // Windows scales the tablet's 0..=0x7fff to 0..=0xffff and then to pixels.
        for p in [0, 1, 319, 640, 1023] {
            assert_eq!(pixel_to_tablet(p, 1024) as u32 * 2 * 1024 / 0x10000, p);
        }
    }

    #[cfg(feature = "window")]
    #[test]
    fn letterboxed_picture_position() {
        assert_eq!(picture_position(0.0, 0.0, (800, 600), (800, 600)), (0, 0));
        assert_eq!(picture_position(400.0, 300.0, (800, 600), (800, 600)), (0x4000, 0x4000));
        // A 4:3 picture in a wide window has bars left and right.
        assert_eq!(picture_position(200.0, 0.0, (1200, 600), (800, 600)), (0, 0));
        assert_eq!(picture_position(600.0, 300.0, (1200, 600), (800, 600)), (0x4000, 0x4000));
        assert_eq!(picture_position(1199.0, 650.0, (1200, 600), (800, 600)), (usb::MAX_COORD, usb::MAX_COORD));
    }
}

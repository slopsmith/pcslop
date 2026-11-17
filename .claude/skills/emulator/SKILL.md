---
name: emulator
description: How to build, run, drive and debug the 86slopper emulator from an agent session - background runs with scripts/vm, the control socket, guest disk images, booting Ubuntu, measuring and profiling. Use it before running any guest, testing an emulator change, timing a boot or chasing a hang.
---

# Working with the 86slopper emulator

## Never block on a guest

- **Never open an emulator window**, not even for a smoke test: the user works on this Mac and windows pop up in front of them. `scripts/vm start` runs headless; leave anything that needs a real window or mouse to the user as manual steps.
- **Run every guest in the background** with `scripts/vm start`. Each tool call should come back within a minute or two. Don't use one long `sleep` or a `timeout` of 5+ minutes in the foreground.
- **Check in with short calls:** `scripts/vm status`, `scripts/vm shot` or `scripts/vm wait-text` (bounded, 60 s by default). Repeat them as often as needed. A long install is many short check-ins, not one long call.
- **Every wait loop needs a limit.** An `until [ -S sock ]` with no count hung for minutes once when the emulator failed to start.
- **Watch memory.** `scripts/vm start` stops a run whose memory footprint passes `VM_MAX_MEM_GB` (default 6) and logs why; a leaking run once took 21 GB and ran the Mac out of memory. Run one guest at a time unless you need two.
- **`scripts/vm start` sets a backstop**, `VM_TIMEOUT` (default 1800 s), so a forgotten guest dies on its own. Stop runs you're done with: `scripts/vm stop NAME`.

## Build

```sh
scripts/vm build        # nix develop + cargo into target/work, signed copy at target/vm/slopper
```

- **Build inside `nix develop`.** It provides libslirp; without it the guest has no network card.
- **Run only your own signed copy.** Never build into or run `target/release`, `target/vic` or another agent's `target/<name>`; the user runs those. `VM_BIN` picks a different binary.
- **`target/` may get cleaned.** If the binary is missing, rebuild.

## Run and drive

```sh
scripts/vm start xp --hda "$img"                    # headless, no sound, socket in the per-user temp dir
scripts/vm shot xp                                  # prints the PNG path; look at it with Read
scripts/vm ctl xp "key enter" "type hello" status   # any control-socket commands
scripts/vm wait-text xp "boot:" 90                  # text-mode screens only
scripts/vm log xp                                   # emulator stdout/stderr (SeaBIOS debug, panics)
scripts/vm stop xp
```

**Control socket commands:**
- `screen`, `status`, `regs`, `mem HEXADDR [LEN [CR3]]` (CR3 picks the address space; `regs` can be stale while the JIT runs)
- `profile on|off`, `pause`, `resume`
- `shot PATH`, `type TEXT`, `serial LINE`, `key NAME[+NAME]`, `down NAME`, `up NAME` (hold a key between them)
- `mouse DX DY [BUTTONS]`, `tablet X Y [BUTTONS [WHEEL]]`
- `power`, `battery [host|off|PERCENT[:ac]]`
- `cdrom [ISO]`, `eject`, `save DIR`, `load DIR`, `quit`
- `stall MS EVERY_MS` (`stall 0` stops): sleeps the emulation thread, to test devices under load

**Rules that bit agents before:**
- **Always use your own socket.** `scripts/ctl` without a socket argument talks to the newest emulator on the machine, which may be another agent's or the user's. Keys, `shot`, and once a `quit`, landed in other sessions that way. `scripts/vm` always passes the socket.
- **Keep socket paths under about 100 bytes**, or binding fails and nothing says so.
- **`shot` with a relative path** writes into the emulator's working directory, which may be the repo root. Use absolute paths; `scripts/vm shot` does.
- **Kill only your own runs.** `scripts/vm stop` kills by socket path. Never `pkill -f slopper` or anything broad.
- **Defaults.** The JIT, AC'97 sound, the RTL8139 network card and the battery mirror are on by default. `scripts/vm start` adds `--no-audio`, so tests don't play sound on the user's Mac; set `VM_AUDIO=1` to keep sound. To check what the guest plays without sound, pass `--audio-silent --audio-wav PATH`: the WAV is what the speaker would play, gaps included, and `status` shows how often the host ring ran dry. Opt-outs: `--no-jit`, `--no-net`, `--no-battery`. `slopper --help` lists everything.

## Save states: skip repeated boots

- **Save:** `scripts/vm ctl NAME "save $SCRATCH/snap"` takes about 0.2 s. It writes `state.bin` plus an APFS clone of the disk (`hda.img`).
- **Start from it:** `scripts/vm start NAME2 --load $SCRATCH/snap`. Each load runs on a fresh copy of `hda.img`, so one snapshot can be restored any number of times. `load DIR` on the control socket restores into a running emulator.
- **When to use it:** before a long reproduction, save just before the interesting moment (a driver loading, an install step), then re-run from there instead of booting each time.
- **Limits:**
  - The guest clock resumes as if no time passed.
  - Open network connections reset.
  - A snapshot only loads into a build whose devices have the same layout; rebuilding after device changes makes old snapshots refuse to load.

## Guest images

- **Never modify `~/xp-nomp.img`, `~/xp-acpi.img` or any ISO.** Run on an APFS clone and delete it afterwards:
  `img=$SCRATCH/x.img; cp -c ~/xp-acpi.img "$img"`
- **`~/xp-nomp.img`:** XP SP3 with the "Standard PC" HAL (8259 PIC, no ACPI). Runs at 1440x900.
- **`~/xp-acpi.img`:** XP SP3 with the "ACPI Uniprocessor PC" HAL (APIC, ACPI). Runs at 800x600. Needed to test ACPI, APIC or battery behaviour in XP.
- **First boot of a clone with new hardware.** Every fresh clone sees new hardware (network card, sound card, a different board) on its first boot and installs drivers during it. That makes it slower and can show a restart prompt. Pass `--no-net`/`--no-audio` when you need comparable boot times.
- **Ubuntu 8.04.4 live:** `--cdrom ~/Documents/ISOs/ubuntu-8.04.4-desktop-i386.iso --ram 512`. The default menu entry reaches the GNOME desktop in about 2 minutes under the JIT.
  - **For a kernel console and a shell on serial:** in the graphical menu press `key esc`, then `key enter` (leave the graphical menu), then type the *whole* line at `boot:`:
    `/casper/vmlinuz file=/cdrom/preseed/ubuntu.seed boot=casper initrd=/casper/initrd.gz console=tty0 console=ttyS0,115200`
    Options typed after `live` land after `--`, so the kernel never sees them.
  - **Serial in and out:** start with `--serial "$SCRATCH/serial.log"`. Type into COM1 with `ctl NAME "serial dmesg | grep -i acpi"` and read the log. Commands that aren't busybox applets may be missing in the initramfs shell.
- **Radeon 9700:** `--gpu radeon --vga-rom /Volumes/Projects/86slopper/references/docs/ati-r300/ATI.9700NP.128.Infineon.021216.rom` (implies the 440BX board). The ROM is the user's copyrighted firmware: never copy it into the repo, a test or the Nix store. `--gpu-trace FILE` logs every register access and collapses repeats into `... N more times`. The tail of the trace shows what a driver is spinning on.
  - The 3D engine draws with Metal; `--gpu-3d cpu` (or `SLOPPER_R3D=cpu`) uses the CPU reference path. `status` prints a `radeon:` line with 3D draws, `flips` (CRTC_OFFSET writes, so two samples give a page-flipping app's frame rate), Metal draws, CPU fallbacks and waits.
  - The command processor runs on its own thread (`cp thread` at the end of the `radeon:` line). `--gpu-thread off` (or `SLOPPER_GPU_THREAD=off`) runs it on the emulation thread again, to rule the thread out when chasing a GPU bug.
  - XP's first boot with the card installs the ATI driver and asks to restart: answer with a `tablet` click and let it reboot. Killing the run mid-install left agp440 broken (Code 39) and XP on VgaSave at 1280x1024; the ATI desktop is 1440x900. Save a state at that desktop and load it instead of booting.
- **Firmware:** `references/seabios` and `references/seavgabios` are Nix out-links. After changing the SeaBIOS part of `flake.nix`, run `nix build .#seabios --out-link references/seabios`. That needs the flake changes committed in jj (`jj new` first).

## Measure and debug

- **`status`:**
  - guest MIPS, and how host time splits between JIT, interpreter and halted;
  - exits per second by kind (interp, miss, fault, mmio);
  - in-VM TPR rate, translations/s, guest #PF/s;
  - APIC and NIC rates.
  Exits cost about 0.7 µs each, so a high exit rate is usually the first thing to look at.
- **Profiling:** start with `SLOPPER_JIT_PROFILE=1` (pass it as an env var to `scripts/vm start`), then `ctl NAME "profile on"`, wait, then `ctl NAME status profile`. That gives time per exit kind, interpreter exits by opcode, and PC samples.
  - **Opcode keys:**
    - `0x1xx` is a `0F xx` opcode;
    - `+0x2000` means REP;
    - `+0x4000` means 16-bit operand size;
    - `0x8000 | op<<4 | reg` is a group opcode, e.g. `8f75` = `F7 /5`, `imul r/m32`.
    - x87: `8d9x` = `D9 /x` with a memory operand; `d9c0`..`dfff` = the opcode byte and ModRM of a register form, e.g. `d9fe` = `fsin`.
  - **PC samples:** `rt+0x980` is the `hvc` at the end of the runtime's exit path, i.e. time spent in the host handling exits, not in guest code.
- **Hangs:** sample `ctl NAME status` a few times. `regs` gives EIP and registers, and `mem ADDR LEN` dumps guest memory at linear addresses, enough to disassemble a busy loop. Add `--gpu-trace` for the Radeon, or `--serial` for Linux.
- **JIT correctness:**
  - `SLOPPER_JIT_VERIFY=1` runs each block against the interpreter for its first executions and panics on the first difference; `=new` checks only recent translations.
  - `SLOPPER_JIT_AUDIT=N` audits the shadow page tables every Nth run.
  - Both are slow, so run them on a short reproduction.
  - A JIT panic names FAR, ELR and ESR. ELR in the code cache means translated code faulted.
- **Boot time:**
  - Time "launch to desktop" with screenshots, or "launch until the CPU goes idle". They differ, so say which one you used.
  - Compare like with like: same image, same flags, and fresh clones, with first-boot driver installs in mind.
  - XP typically takes about 8-10 s to the desktop.
- **Reproducing a race:** run the same thing 4-8 times in a row. Several bugs here showed up in only half the runs.

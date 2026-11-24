# pcslop

An x86-32 PC emulator for Apple Silicon Macs. The goal is to run Windows XP faster than QEMU's TCG, by running translated code inside a Hypervisor.framework VM where x86 paging maps onto the ARM MMU.

- [Architecture](docs/architecture.md)
- [References](docs/references.md)

## Download

Nightly builds of the Mac app (macOS 26, Apple Silicon) are at https://github.com/slopsmith/pcslop/releases/tag/nightly; https://github.com/slopsmith/pcslop/releases/download/nightly/pcslop-macos-arm64.zip is always the newest. See [apps/macos/README.md](apps/macos/README.md#nightly-builds).

## Setup

```sh
scripts/fetch-references.sh   # reference source trees -> references/src (gitignored)
scripts/fetch-docs.sh         # spec PDFs -> references/docs (gitignored)
experiments/hvf-probe/run.sh  # checks HVF, TSO, exit and exception costs on this Mac
nix build .#seabios --out-link references/seabios
```

## Running

```sh
cargo run --release -p pcslop -- --bios references/seabios/share/seabios/bios.bin
cargo run --release -p cputest -- references/src/SingleStepTests-80386/v1_ex_real_mode
```

SeaBIOS's debug output (port 0x402) goes to stdout. `pcslop --help` lists every option.

The board always has a CD-ROM drive, empty unless `--cdrom ISO` is given. Swap discs while the guest runs through the control socket (`--control SOCKET`, driven by `scripts/ctl SOCKET COMMAND`): `cdrom ISO` inserts one, `cdrom` alone shows the current one and `eject` empties the drive. The guest sees a medium change, as on a real drive.

`save DIR` on the control socket snapshots the running machine into a directory, with an APFS clone of the disk image; `load DIR` goes back to it, and `--load DIR` starts from it instead of booting. Start with the same `--ram`, sound, network and firmware options as when saving. See [Save states](docs/architecture.md#save-states).

By default the guest runs translated code in the VM and gets an AC'97 sound card, an RTL8139 network card and a battery that mirrors the Mac's. Turn them off with `--no-jit`, `--no-audio`, `--no-net` and `--no-battery`. Sound plays on the Mac's default output device; `--audio-wav out.wav` also records it and `--audio-silent` plays nothing.

`--gpu radeon --vga-rom ROM` swaps the stdvga for an ATI Radeon 9700 on a 440BX board's AGP bus, running the card's own video BIOS, which you supply. `--gpu-trace PATH` logs the card's register accesses, and `--gpu-3d cpu` draws its 3D engine on the CPU instead of with Metal. See [docs/gpu.md](docs/gpu.md).

The network card sits on a user-mode network (no host privileges needed), with QEMU's addresses: the guest is 10.0.2.15 and the gateway 10.0.2.2. `--net user,hostfwd=tcp::2222-:22` forwards a host port into the guest. It needs libslirp, so build inside `nix develop`, which provides it through pkg-config. A plain `cargo build` still works, but leaves the network card out.

The window resizes itself to each new guest resolution, scaled down to fit the screen, unless it is maximized or in full screen. Its green button puts it in macOS full screen, with the picture centred between black bars. Keys map by position on a US layout; Cmd is the Windows key, and holding a key repeats it in the guest at the Mac's key repeat rate. Cmd+Q and Cmd+H go to the guest too, so close the window to quit.

In the window, the mouse drives the guest's USB tablet, so the pointer moves in and out of the window freely. Games that read relative motion, such as first-person shooters, need the mouse captured instead: press Ctrl+Option+G to hide and lock the Mac's cursor and send the mouse's motion, buttons and wheel to the guest's PS/2 mouse. Ctrl+Option+G again, or switching to another app, releases it; the title bar says when the mouse is captured. `--capture-mouse click` also captures on a click in the window, and `--capture-mouse auto` does so only while the guest has no tablet driver. The control socket's `capture on|off|toggle` does the same and, without a window, makes the guest ignore `tablet` commands, so `mouse DX DY [BUTTONS [WHEEL]]` is its only pointer. Capturing needs no extra macOS permissions.

To watch and drive the VM from another machine, add `--vnc 5900` (with or without `--headless`) and connect a VNC client to port 5900. The server listens on 127.0.0.1 unless given an address, as in `--vnc 0.0.0.0:5900`. Set `PCSLOP_VNC_PASSWORD` to require a password; macOS Screen Sharing needs one. VNC authentication does not encrypt anything, so reach the port through SSH (`ssh -L 5900:localhost:5900 mac`) or a VPN. Over a slow link, pick ZRLE or Tight in the client if it does not already prefer one.

Licensed under GPL-2.0-or-later.

# 86slopper

An x86-32 PC emulator for Apple Silicon Macs. The goal is to run Windows XP faster than QEMU's TCG, by running translated code inside a Hypervisor.framework VM where x86 paging maps onto the ARM MMU.

- [Architecture](docs/architecture.md)
- [References](docs/references.md)

## Setup

```sh
scripts/fetch-references.sh   # reference source trees -> references/src (gitignored)
scripts/fetch-docs.sh         # spec PDFs -> references/docs (gitignored)
experiments/hvf-probe/run.sh  # checks HVF, TSO, exit and exception costs on this Mac
nix build .#seabios --out-link references/seabios
```

## Running

```sh
cargo run --release -p slopper -- --bios references/seabios/share/seabios/bios.bin
cargo run --release -p cputest -- references/src/SingleStepTests-80386/v1_ex_real_mode
```

SeaBIOS's debug output (port 0x402) goes to stdout. `slopper --help` lists every option.

The board always has a CD-ROM drive, empty unless `--cdrom ISO` is given. Swap discs while the guest runs through the control socket (`--control SOCKET`, driven by `scripts/ctl SOCKET COMMAND`): `cdrom ISO` inserts one, `cdrom` alone shows the current one and `eject` empties the drive. The guest sees a medium change, as on a real drive.

By default the guest runs translated code in the VM and gets an AC'97 sound card, an RTL8139 network card and a battery that mirrors the Mac's. Turn them off with `--no-jit`, `--no-audio`, `--no-net` and `--no-battery`. Sound plays on the Mac's default output device; `--audio-wav out.wav` also records it and `--audio-silent` plays nothing.

`--gpu radeon --vga-rom ROM` swaps the stdvga for an ATI Radeon 9700 on a 440BX board's AGP bus, running the card's own video BIOS, which you supply. `--gpu-trace PATH` logs the card's register accesses. See [docs/gpu.md](docs/gpu.md).

The network card sits on a user-mode network (no host privileges needed), with QEMU's addresses: the guest is 10.0.2.15 and the gateway 10.0.2.2. `--net user,hostfwd=tcp::2222-:22` forwards a host port into the guest. It needs libslirp, so build inside `nix develop`, which provides it through pkg-config. A plain `cargo build` still works, but leaves the network card out.

To watch and drive the VM from another machine, add `--vnc 5900` (with or without `--headless`) and connect a VNC client to port 5900. The server listens on 127.0.0.1 unless given an address, as in `--vnc 0.0.0.0:5900`. Set `SLOPPER_VNC_PASSWORD` to require a password; macOS Screen Sharing needs one. VNC authentication does not encrypt anything, so reach the port through SSH (`ssh -L 5900:localhost:5900 mac`) or a VPN. Over a slow link, pick ZRLE or Tight in the client if it does not already prefer one.

Licensed under GPL-2.0-or-later.

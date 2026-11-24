# pcslop machine manager

A SwiftUI app for macOS 26 that keeps a library of pcslop machines and starts
them. Each machine opens in pcslop's own window; the app talks to it over the
control socket to pause it, press its power button, save its state, change
its CD and show how fast it runs.

It is derived from the ps5slop launcher (`apps/macos` in ps5slop, by the same
author, also GPL-2.0-or-later): the package layout, binary lookup, control
socket, log buffer, process handling, inspector, settings, snapshot options
and icon tool started as copies of it.

## Build and run

```sh
cd apps/macos
swift build
swift test
swift run pcslop-manager
```

Inside `nix develop`, call Apple's toolchain directly:
`env -u SDKROOT -u DEVELOPER_DIR /usr/bin/xcrun swift build`.

`Tools/bundle-app` builds `target/pcslop.app` with pcslop, the libraries it
links and SeaBIOS inside. `Tools/screenshots` saves preview screenshots with a
few sample machines.

## Where things are found

- **pcslop**: the Settings value, the copy in the app bundle, `PCSLOP_BIN`,
  then `target/release/pcslop` or `target/vm/pcslop` above the current folder
  or the app. It needs the Hypervisor entitlement, which `cargo run` and
  `scripts/vm build` sign in.
- **BIOS and VGA BIOS**: the Settings values, the bundle's copies, then
  `references/seabios/share/seabios/bios.bin` and
  `references/seavgabios/share/seabios/vgabios.bin` above the current folder.
- **Machines**: `~/Library/Application Support/pcslop/machines/UUID/`, with
  `machine.json`, `thumbnail.png` (taken when the machine stops) and save
  states in `states/`. `--store DIR` uses another folder.

## Save states

A save state is a folder made by the control socket's `save DIR`. It holds a
copy of the primary disk, so a machine started from a state runs on that
copy, and the machine's own disk image stays as it was. pcslop refuses
`--hdb` with `--load`, so a state starts without the secondary disk.

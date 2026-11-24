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

- **pcslop, SeaBIOS and SeaVGABIOS** come inside the app bundle. Settings can
  point at other builds instead. Run from a checkout (`swift run`), the app
  also looks for `target/release/pcslop`, `target/vm/pcslop` and the
  `references/` SeaBIOS out-links above the current folder; a packaged app
  never looks outside itself.
- **Radeon 9700 video BIOS**: chosen once in Settings, for every machine. pcslop
  cannot ship it. Without it, the Radeon is unavailable and presets use standard VGA.
- **Machines**: `~/Library/Application Support/pcslop/machines/UUID/`, with
  `machine.json`, `thumbnail.png` (taken when the machine stops), save states
  in `states/` and any disk made for the machine. `--store DIR` uses another folder.

## Logos

The system logos in `Sources/Manager/Logos` were rendered from SVGs on
Wikimedia Commons. They are trademarks of their owners and are only used to
show which system a machine runs.

- `windows-9x.png`, `windows-xp.png`: Microsoft Windows logos, public domain on
  Commons ("Windows logo - 1992.svg", "Windows logo - 2002–2012 (Multicolored).svg").
- `ubuntu.png`: Canonical's Ubuntu Circle of Friends, public domain on Commons ("UbuntuCoF.svg").
- `debian.png`: the Debian open use logo, public domain on Commons ("Openlogo-debianV2.svg").
- `fedora.png`: the Fedora logo, public domain on Commons ("Fedora icon (2021).svg").
- `tux.png`: Tux by Larry Ewing (lewing@isc.tamu.edu), drawn with The GIMP ("Tux.svg").

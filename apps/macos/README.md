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
few sample machines. `--about` opens only the About window, for `--snapshot`.

The shared bundle steps come from `slop-app` in
[slopsmith/ci](https://github.com/slopsmith/ci), pinned in `flake.lock`
(`nix flake update slopsmith-ci` moves it). The script ends with the upload
checks the nightly workflow also runs: everything is arm64 and nothing links
into `/nix/store` (Apple's libiconv and libresolv are linked from `/usr/lib`
instead of bundled), the signatures verify and pcslop has the hypervisor
entitlement, the bundled `pcslop --version` names the same commit as the app,
and every bundled library is listed in `Licenses/README.txt`.

Every build is stamped with the commit it was built from: `SLOP_COMMIT` if
set (the nightly workflow sets it), else `git rev-parse HEAD`, plus `-dirty`
from `Tools/bundle-app` when tracked files differ and `-dev` from a plain
`cargo build`. `pcslop --version`, the first line pcslop prints, Info.plist's
`SlopCommit` and the About window show it.

## Nightly builds

`.github/workflows/nightly.yml` runs the shared workflow from slopsmith/ci at
03:00 UTC when `trunk` has moved (or by hand). It builds SeaBIOS on a Linux
runner, runs `cargo test` without the `jit` and `hvf` crates (hosted macOS
runners cannot make Hypervisor.framework VMs), bundles the app, runs the
checks and replaces the assets of the rolling `nightly` release:
`pcslop-macos-arm64.zip` and `nightly.json`. Older builds are not kept.

The app is signed ad hoc, not notarised: the first time, open it, then go to
System Settings > Privacy & Security and choose Open Anyway.

## About, licenses and updates

pcslop > About pcslop shows the version, the commit (linked on GitHub, with a
copy button) and every license in `Contents/Resources/Licenses`: the texts in
`Licenses/`, pcslop's own `LICENSE` and the Rust crates' notices, which
`cargo about` writes. A new bundled library needs its license text in
`Licenses/` and a line in `Licenses/README.txt`.

A nightly build reads `nightly.json` when it starts and offers to download a
newer nightly; pcslop > Check for Updates… checks at once. Local builds never
check. Settings turns the launch check off.

## Where things are found

- **pcslop, SeaBIOS and SeaVGABIOS** come inside the app bundle. Settings can
  point at other builds instead. Run from a checkout (`swift run`), the app
  also looks for `target/release/pcslop`, `target/vm/pcslop` and the
  `references/` SeaBIOS out-links above the current folder; a packaged app
  never looks outside itself.
- **Radeon 9700 Pro video BIOS**: chosen once in Settings, for every machine. pcslop
  cannot ship it. It must come from a 9700 Pro; a plain Radeon 9700's video BIOS
  does not work, and the machine never shows a picture. Without it, the Radeon is unavailable and presets use standard VGA.
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

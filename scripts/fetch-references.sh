#!/bin/sh
# Shallow-clones reference source trees into references/src (gitignored).
# Pass --tests to also fetch the large hardware-generated CPU test suites.
set -eu
dest="$(cd "$(dirname "$0")/.." && pwd)/references/src"
mkdir -p "$dest"
cd "$dest"

clone() {
	echo "=== $1"
	if [ -d "$1" ]; then
		git -C "$1" pull --ff-only --progress
	else
		git clone --depth 1 --progress "$2" "$1"
	fi
}

clone seabios https://github.com/coreboot/seabios.git
clone qemu https://gitlab.com/qemu-project/qemu.git
clone bochs https://github.com/bochs-emu/Bochs.git
clone 86Box https://github.com/86Box/86Box.git
clone box64 https://github.com/ptitSeb/box64.git
clone FEX https://github.com/FEX-Emu/FEX.git
clone berkeley-softfloat-3 https://github.com/ucb-bar/berkeley-softfloat-3.git
clone test386.asm https://github.com/barotto/test386.asm.git
clone applecpu https://github.com/dougallj/applecpu.git
clone asahi-docs https://github.com/AsahiLinux/docs.git

if [ "${1:-}" = "--tests" ]; then
	clone SingleStepTests-80386 https://github.com/SingleStepTests/80386.git
fi

#!/bin/sh
# Shallow-clones reference source trees into references/src (gitignored).
# Pass --tests to also fetch the large hardware-generated CPU test suites,
# and --gpu to fetch the ATI GPU references.
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

sparse() {
	echo "=== $1"
	[ -d "$1" ] && return
	name=$1 url=$2 rev=$3
	shift 3
	git clone --depth 1 --branch "$rev" --filter=blob:none --sparse --progress "$url" "$name"
	git -C "$name" sparse-checkout set "$@"
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
clone moo https://github.com/dbalsom/moo.git

for arg in "$@"; do
	case "$arg" in
	--tests)
		clone SingleStepTests-80386 https://github.com/SingleStepTests/80386.git
		;;
	--gpu)
		clone mesa https://gitlab.freedesktop.org/mesa/mesa.git
		sparse linux https://github.com/torvalds/linux.git master drivers/gpu/drm/radeon include/uapi/drm include/drm
		sparse linux-2.6.24 https://github.com/torvalds/linux.git v2.6.24 drivers/char/drm include/linux
		if [ ! -d xf86-video-ati ]; then
			git clone --progress https://gitlab.freedesktop.org/xorg/driver/xf86-video-ati.git xf86-video-ati
			git -C xf86-video-ati -c advice.detachedHead=false checkout xf86-video-ati-6.14.6
		fi
		;;
	esac
done

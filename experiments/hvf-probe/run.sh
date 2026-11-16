#!/bin/sh
# Builds and runs the probe in weak and TSO memory-model modes.
set -eu
cd "$(dirname "$0")"
out="${TMPDIR:-/tmp}/hvf-probe"
clang -O2 -Wall -o "$out" probe.c -framework Hypervisor
codesign -s - -f --entitlements ent.plist "$out"
"$out"
"$out" tso

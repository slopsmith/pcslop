# Guest CPU benchmarks

The numbers here steer JIT work. `scripts/bench-xp` runs a benchmark CD in
Windows XP and prints one line per test.

## Build and run

```sh
scripts/build-bench                 # target/bench/xpbench.iso
VM_BIN=target/vm/slopper scripts/bench-xp
```

`scripts/build-bench` enters the `xpbench` dev shell from `flake.nix`
(`nix/xpbench.nix`): the nixpkgs mingw32 cross compiler, xorriso and the
pinned third-party downloads. It compiles everything into `target/bench/cd`
and packs the ISO. Nothing binary is committed. `scripts/bench-xp` builds the
ISO itself when it is missing or older than its sources.

`scripts/bench-xp` boots a clone of `~/xp-nomp.img` with the CD, waits for an
idle desktop, types `D:\run.bat` into the Run dialog and waits for
`BENCH-DONE` on COM1 (the emulator's `--serial` file). Settings:

- `BENCH_TEST=NAME BENCH_REPEAT=N` runs one test N times through `D:\one.bat`.
  NAME is a `bench.exe` test (see below), `games` (all game tests), or one of
  `7zip coremark scimark whetstone linpack`.
- `BENCH_PROFILE=DIR` profiles the run: `profile.txt`, `status.txt` and the
  hottest blocks for `scripts/jit-blocks`.
- `BENCH_LOG=FILE` keeps the raw COM1 output.
- `BENCH_ISO=FILE` uses another CD.

In the guest, `bench.exe [OUT [TEST|all|games [REPEAT]]]` writes to OUT
(`COM1` on the CD, `C:\bench.txt` by default).

## Reading the results

`bench.exe` prints `name  seconds s  rate unit`. Lower seconds is better.
`total` covers the first nine tests only, so it stays comparable with numbers
from before the game tests; `games total` covers the game tests.

| Test | Name in output | What it does |
|------|----------------|--------------|
| `int` | crc32 | table-driven CRC-32 over 64 MB |
| `sort` | shellsort | shell sort of 2M random words, load-latency bound |
| `memcpy` | memcpy | 512 MB of `memcpy` between 16 MB buffers (`rep movs`) |
| `fp` | mandel x87, mandel sse2 | Mandelbrot in double precision, x87 and SSE2 |
| `syscall` | syscall | 200k `SwitchToThread` calls |
| `file` | file create | create, write 4 KB, close, delete; 2000 times |
| `pagefault` | page faults | touch 32 MB of fresh pages, 8 rounds |
| `ctxsw` | thread pingpong | two threads trading events, 50k round trips |
| `process` | process | `cmd.exe /c exit`, 20 times |
| `xform` | xform x87 | Direct3D-style transform and lighting: single precision x87 with the FPU at 24-bit precision, perspective divide, float-to-int colour packing |
| `blit` | blit mmx | MMX alpha blending of 64x64 sprites onto a 640x480 back buffer, then MMX conversion to RGB565 |
| `path` | pathfind | A* on a 128x128 8-connected tile map with a binary heap |
| `entity` | entities | C++ entity system: 4096 polymorphic objects updated through virtual calls, per-type state machines, deaths and respawns |
| `raster` | raster | software rasteriser: float setup, integer edge functions, 16-bit z-buffer, affine texturing, Gouraud shading through a lighting table |
| `physics` | physics | 600 rigid spheres with a grid broadphase, sqrt and divides in the contact solver |

Every test except syscall, file, page faults, pingpong and process checks its
result against a value computed by the interpreter (`--no-jit`). A mismatch
prints `BAD name: result X, want Y`; `scripts/bench-xp` then exits non-zero.
A wrong JIT shows up as an error, not as a fast time. After changing a test,
run it once with `--no-jit`, which prints `check name: result X` for a zero
expected value, and put that value in `bench.c`.

The third-party benchmarks follow, reduced to one line each:

- `Avr:` and `Tot:` from 7-Zip's `7za b 3 -mmt1 -md22` (compression and
  decompression ratings, the same flags as the earliest numbers). 7-Zip
  checks its own output; an error line counts as a wrong result.
- `coremark`: iterations per second, 30000 iterations. CoreMark wants at least
  10 s for a reportable score; that warning is ignored, its CRC checks are not.
- `scimark`: SciMark 2.0 composite Mflops with 0.2 s per kernel, then the five
  kernels (FFT, SOR, Monte Carlo, sparse matrix multiply, LU).
- `whetstone`: double precision MWIPS, 4000 loops. Heavy on `sin`, `cos`,
  `atan`, `exp`, `log` and `sqrt`, so on x87 transcendentals.
- `linpack`: KFLOPS of the 200x200 double precision solve, repeated until a
  pass takes 1 s.

Timings are noisy while other emulators run on the Mac. Compare large changes
or re-run on a quiet machine.

## Third-party code

All of it is downloaded at build time from its official source and pinned by
hash in `nix/xpbench.nix`. Each licence file goes onto the CD next to the
program.

| Program | Source | Licence | Local changes |
|---------|--------|---------|---------------|
| 7-Zip 9.20 command line (`7za.exe`), Igor Pavlov | https://www.7-zip.org/a/7za920.zip | GNU LGPL 2.1 | none, the official binary |
| CoreMark, EEMBC, commit 1f483d5 | https://github.com/eembc/coremark | Apache 2.0 plus the CoreMark trademark terms in LICENSE.md | none; built with the `simple` port, `PERFORMANCE_RUN=1 ITERATIONS=6000` |
| SciMark 2.0 C, Roldan Pozo and Bruce Miller, NIST | https://math.nist.gov/scimark2/ | public domain (US government work) | none |
| Whetstone, C version 1.2 by Rich Painter, Painter Engineering | https://www.netlib.org/benchmark/whetstone.c | use, copying and publishing allowed with its comment block kept | `bench/xp/patches/whetstone.patch`: times with `clock()` instead of `time()`, whose 1 s resolution is too coarse |
| Linpack, C translation by Bonnie Toy and Will Menninger | https://www.netlib.org/benchmark/linpackc.new | netlib, no restrictions stated | `bench/xp/patches/linpack.patch`: stops at 1 s instead of 10 s per pass, `int main`, quits on `q` |

The rest of `bench/xp` is ours. The restored tests in `bench.c` are the
original suite from before it was lost.

### Considered and rejected

- **Dhrystone** (netlib `dhry-c`): superseded by CoreMark, which its authors
  wrote to replace it; modern compilers optimise much of it away, and it
  times with `time()` or `times()`.
- **SuperPI** (and its "mod" builds): GUI only, with a result dialog; no
  command-line mode. Redistribution terms unclear.
- **y-cruncher**: current builds need a much newer Windows than XP, and I
  could not find an official download of an XP-capable old build. It is also
  driven from an interactive menu.
- **Cinebench R10 and R11.5**: Maxon's site offers only current versions, so
  there is no official URL to pin, and the EULA does not cover fetching it
  from a mirror. Both are GUI programs that show their score in the window.
- **nbench / BYTEmark**: no clear licence for the original BYTE code, and
  its index needs 10 s or more per test.
- **wPrime, PCMark, 3DMark CPU tests**: GUI only or not redistributable.

## Ubuntu (`scripts/bench-cpu`)

`scripts/bench-cpu` loads a save state of Ubuntu 8.04 live with a root shell
on serial and its benchmark script in `/tmp`. That state was lost with the
old XP CD and needs recreating; see the script.

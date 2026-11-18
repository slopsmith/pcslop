#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "bench.h"

static LARGE_INTEGER freq;
static FILE *out;

double now(void) {
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart / (double)freq.QuadPart;
}

static void emit(const char *line) {
    fputs(line, out);
    fflush(out);
    if (out != stdout) printf("%s", line);
}

void report(const char *name, double secs, double work, const char *unit) {
    char line[160];
    snprintf(line, sizeof line, "%-12s %8.3f s  %10.2f %s\r\n", name, secs, work / secs, unit);
    emit(line);
}

/* A wrong result prints a BAD line, which scripts/bench-xp reports as a
   failure. A zero want means the expected value is not known yet. */
void check(const char *name, unsigned got, unsigned want) {
    char line[160];
    if (got == want) return;
    if (want)
        snprintf(line, sizeof line, "BAD %s: result %08x, want %08x\r\n", name, got, want);
    else
        snprintf(line, sizeof line, "check %s: result %08x\r\n", name, got);
    emit(line);
}

static volatile unsigned sink;

static unsigned crc_table[256];

static void bench_int(void) {
    static unsigned char buf[1 << 20];
    for (unsigned i = 0; i < 256; i++) {
        unsigned c = i;
        for (int k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >> 1) : c >> 1;
        crc_table[i] = c;
    }
    unsigned x = 12345;
    for (unsigned i = 0; i < sizeof buf; i++) { x = x * 1103515245 + 12345; buf[i] = x >> 16; }
    double t = now();
    unsigned crc = ~0u;
    for (int r = 0; r < 64; r++)
        for (unsigned i = 0; i < sizeof buf; i++) crc = crc_table[(crc ^ buf[i]) & 0xff] ^ (crc >> 8);
    sink = crc;
    report("crc32", now() - t, 64.0, "MB/s");
    check("crc32", crc, 0xb496ee72);
}

static int cmp_sort(unsigned *a, int n) {
    for (int gap = n / 2; gap > 0; gap /= 2)
        for (int i = gap; i < n; i++) {
            unsigned v = a[i];
            int j = i;
            while (j >= gap && a[j - gap] > v) { a[j] = a[j - gap]; j -= gap; }
            a[j] = v;
        }
    return a[n / 2];
}

static void bench_sort(void) {
    enum { N = 1 << 18 };
    static unsigned a[N];
    double t = now();
    unsigned x = 1, sum = 0;
    for (int r = 0; r < 8; r++) {
        for (int i = 0; i < N; i++) { x ^= x << 13; x ^= x >> 17; x ^= x << 5; a[i] = x; }
        sink = cmp_sort(a, N);
        sum += sink;
    }
    report("shellsort", now() - t, 8.0 * N / 1e6, "Melem/s");
    for (int i = 1; i < N; i++)
        if (a[i - 1] > a[i]) sum = 0;
    check("shellsort", sum, 0xff3b69ae);
}

static void bench_memcpy(void) {
    enum { SIZE = 16 << 20 };
    char *a = VirtualAlloc(0, SIZE, MEM_COMMIT, PAGE_READWRITE);
    char *b = VirtualAlloc(0, SIZE, MEM_COMMIT, PAGE_READWRITE);
    memset(a, 1, SIZE);
    memset(b, 2, SIZE);
    double t = now();
    for (int r = 0; r < 32; r++) memcpy(r & 1 ? a : b, r & 1 ? b : a, SIZE);
    report("memcpy", now() - t, 32.0 * 16, "MB/s");
    unsigned sum = 0;
    for (int i = 0; i < SIZE; i += 4093) sum += a[i] + b[i] * 3;
    check("memcpy", sum, 0x00004010);
    VirtualFree(a, 0, MEM_RELEASE);
    VirtualFree(b, 0, MEM_RELEASE);
}

static int mandel_x87(void) {
    int total = 0;
    for (int py = 0; py < 300; py++)
        for (int px = 0; px < 400; px++) {
            double cr = -2.0 + px * (3.0 / 400), ci = -1.2 + py * (2.4 / 300), zr = 0, zi = 0;
            int i = 0;
            while (i < 256 && zr * zr + zi * zi < 4.0) {
                double t = zr * zr - zi * zi + cr;
                zi = 2 * zr * zi + ci;
                zr = t;
                i++;
            }
            total += i;
        }
    return total;
}

__attribute__((target("sse2,fpmath=sse"))) static int mandel_sse2(void) {
    int total = 0;
    for (int py = 0; py < 300; py++)
        for (int px = 0; px < 400; px++) {
            double cr = -2.0 + px * (3.0 / 400), ci = -1.2 + py * (2.4 / 300), zr = 0, zi = 0;
            int i = 0;
            while (i < 256 && zr * zr + zi * zi < 4.0) {
                double t = zr * zr - zi * zi + cr;
                zi = 2 * zr * zi + ci;
                zr = t;
                i++;
            }
            total += i;
        }
    return total;
}

static void bench_fp(void) {
    double t = now();
    int n = 0;
    for (int r = 0; r < 4; r++) n += mandel_x87();
    report("mandel x87", now() - t, n / 1e6, "Miter/s");
    check("mandel x87", n, 0x01affc84);
    t = now();
    n = 0;
    for (int r = 0; r < 4; r++) n += mandel_sse2();
    report("mandel sse2", now() - t, n / 1e6, "Miter/s");
    check("mandel sse2", n, 0x01affc84);
}

static void bench_syscall(void) {
    enum { N = 200000 };
    double t = now();
    for (int i = 0; i < N; i++) SwitchToThread();
    report("syscall", now() - t, N / 1e3, "Kcalls/s");
}

static void bench_file(void) {
    enum { N = 2000 };
    char path[MAX_PATH];
    GetTempPathA(sizeof path, path);
    strcat(path, "slopbench.tmp");
    char data[4096];
    memset(data, 'x', sizeof data);
    double t = now();
    for (int i = 0; i < N; i++) {
        HANDLE h = CreateFileA(path, GENERIC_WRITE, 0, 0, CREATE_ALWAYS, FILE_ATTRIBUTE_TEMPORARY, 0);
        DWORD w;
        WriteFile(h, data, sizeof data, &w, 0);
        CloseHandle(h);
        DeleteFileA(path);
    }
    report("file create", now() - t, N / 1e3, "Kfiles/s");
}

static void bench_pagefault(void) {
    enum { SIZE = 32 << 20, ROUNDS = 8 };
    double t = now();
    for (int r = 0; r < ROUNDS; r++) {
        char *p = VirtualAlloc(0, SIZE, MEM_COMMIT, PAGE_READWRITE);
        for (int i = 0; i < SIZE; i += 4096) p[i] = 1;
        VirtualFree(p, 0, MEM_RELEASE);
    }
    report("page faults", now() - t, ROUNDS * (SIZE / 4096) / 1e3, "Kfaults/s");
}

static HANDLE ping, pong;
static DWORD WINAPI ponger(LPVOID n) {
    for (int i = 0; i < (int)(INT_PTR)n; i++) {
        WaitForSingleObject(ping, INFINITE);
        SetEvent(pong);
    }
    return 0;
}

static void bench_ctxsw(void) {
    enum { N = 50000 };
    ping = CreateEvent(0, FALSE, FALSE, 0);
    pong = CreateEvent(0, FALSE, FALSE, 0);
    HANDLE th = CreateThread(0, 0, ponger, (LPVOID)(INT_PTR)N, 0, 0);
    double t = now();
    for (int i = 0; i < N; i++) {
        SetEvent(ping);
        WaitForSingleObject(pong, INFINITE);
    }
    report("thread pingpong", now() - t, N / 1e3, "Kround/s");
    WaitForSingleObject(th, INFINITE);
    CloseHandle(th);
    CloseHandle(ping);
    CloseHandle(pong);
}

static void bench_process(void) {
    enum { N = 20 };
    char cmd[] = "cmd.exe /c exit";
    double t = now();
    for (int i = 0; i < N; i++) {
        STARTUPINFOA si = { sizeof si };
        PROCESS_INFORMATION pi;
        if (!CreateProcessA(0, cmd, 0, 0, FALSE, CREATE_NO_WINDOW, 0, 0, &si, &pi)) return;
        WaitForSingleObject(pi.hProcess, INFINITE);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    report("process", now() - t, N, "procs/s");
}

#define GAME_TEST(fn, label, run, n, work, unit, want) \
    static void fn(void) {                             \
        double t = now();                              \
        unsigned r = run(n);                           \
        report(label, now() - t, (work) * (n), unit);  \
        check(label, r, want);                         \
    }

GAME_TEST(bench_xform, "xform x87", xform_run, 30, 4096 / 1e6, "Mvert/s", 0xd8ba6b1a)
GAME_TEST(bench_blit, "blit mmx", blit_run, 60, 640 * 480 / 1e6, "Mpix/s", 0xa3e21077)
GAME_TEST(bench_path, "pathfind", path_run, 1500, 1 / 1e3, "Kpath/s", 0x340902e5)
GAME_TEST(bench_entity, "entities", entity_run, 4000, 4096 / 1e6, "Mupd/s", 0x2e45824c)
GAME_TEST(bench_raster, "raster", raster_run, 150, 1, "frames/s", 0x142e60f7)
GAME_TEST(bench_physics, "physics", physics_run, 1000, 1, "steps/s", 0xcb052d9c)

static const struct {
    const char *key;
    void (*run)(void);
    int game;
} tests[] = {
    { "int", bench_int, 0 },
    { "sort", bench_sort, 0 },
    { "memcpy", bench_memcpy, 0 },
    { "fp", bench_fp, 0 },
    { "syscall", bench_syscall, 0 },
    { "file", bench_file, 0 },
    { "pagefault", bench_pagefault, 0 },
    { "ctxsw", bench_ctxsw, 0 },
    { "process", bench_process, 0 },
    { "xform", bench_xform, 1 },
    { "blit", bench_blit, 1 },
    { "path", bench_path, 1 },
    { "entity", bench_entity, 1 },
    { "raster", bench_raster, 1 },
    { "physics", bench_physics, 1 },
};
enum { NTESTS = sizeof tests / sizeof tests[0] };

static void total(const char *name, double t, DWORD tick) {
    char line[120];
    snprintf(line, sizeof line, "%-12s %8.3f s  (GetTickCount %.3f s)\r\n", name, now() - t,
             (GetTickCount() - tick) / 1000.0);
    emit(line);
}

static void usage(void) {
    printf("usage: bench [OUT [TEST|all|games [REPEAT]]]\ntests:");
    for (int i = 0; i < NTESTS; i++) printf(" %s", tests[i].key);
    printf("\n");
    exit(2);
}

int main(int argc, char **argv) {
    const char *which = argc > 2 ? argv[2] : "all";
    int repeat = argc > 3 ? atoi(argv[3]) : 1;
    if (argc > 4 || repeat < 1) usage();
    QueryPerformanceFrequency(&freq);
    out = fopen(argc > 1 ? argv[1] : "C:\\bench.txt", "ab");
    if (!out) out = stdout;
    emit("---\r\n");

    int all = !strcmp(which, "all"), games = !strcmp(which, "games"), ran = 0;
    DWORD tick = GetTickCount();
    double t = now();
    for (int r = 0; r < repeat; r++)
        for (int i = 0; i < NTESTS; i++) {
            if (all && tests[i].game && !ran) {
                /* "total" covers the original nine tests only, so it stays
                   comparable with results from before the game tests. */
                total("total", t, tick);
                tick = GetTickCount();
                t = now();
                ran = 1;
            }
            if (all || (games && tests[i].game) || !strcmp(which, tests[i].key)) tests[i].run();
        }
    if (all || games) total("games total", t, tick);
    else if (!ran && !games) {
        int known = 0;
        for (int i = 0; i < NTESTS; i++) known |= !strcmp(which, tests[i].key);
        if (!known) usage();
    }
    if (out != stdout) fclose(out);
    return 0;
}

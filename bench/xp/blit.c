/* 2D sprite work in MMX, as a DirectDraw-era game or video player does it:
   alpha-blend 32-bit sprites onto a back buffer, then convert the frame to
   RGB565 for a 16-bit primary surface. */
#include <mmintrin.h>
#include "bench.h"

enum { W = 640, H = 480, SW = 64, SH = 64, SPRITES = 64 };

static unsigned back[W * H] __attribute__((aligned(8)));
static unsigned short front[W * H] __attribute__((aligned(8)));
static unsigned sprite[SW * SH] __attribute__((aligned(8)));

static __m64 alpha_of(__m64 px) {
    __m64 a = _mm_srli_si64(px, 48);
    a = _mm_unpacklo_pi16(a, a);
    return _mm_unpacklo_pi32(a, a);
}

static void blend_row(unsigned *dst, const unsigned *src, int n) {
    const __m64 zero = _mm_setzero_si64(), c256 = _mm_set1_pi16(256);
    for (int i = 0; i < n; i += 2) {
        __m64 s = *(const __m64 *)&src[i], d = *(__m64 *)&dst[i];
        __m64 slo = _mm_unpacklo_pi8(s, zero), shi = _mm_unpackhi_pi8(s, zero);
        __m64 dlo = _mm_unpacklo_pi8(d, zero), dhi = _mm_unpackhi_pi8(d, zero);
        __m64 alo = alpha_of(slo), ahi = alpha_of(shi);
        __m64 rlo = _mm_add_pi16(_mm_mullo_pi16(slo, alo), _mm_mullo_pi16(dlo, _mm_sub_pi16(c256, alo)));
        __m64 rhi = _mm_add_pi16(_mm_mullo_pi16(shi, ahi), _mm_mullo_pi16(dhi, _mm_sub_pi16(c256, ahi)));
        *(__m64 *)&dst[i] = _mm_packs_pu16(_mm_srli_pi16(rlo, 8), _mm_srli_pi16(rhi, 8));
    }
}

static __m64 to565(__m64 p) {
    __m64 r = _mm_and_si64(_mm_srli_pi32(p, 8), _mm_set1_pi32(0xf800));
    __m64 g = _mm_and_si64(_mm_srli_pi32(p, 5), _mm_set1_pi32(0x07e0));
    __m64 b = _mm_and_si64(_mm_srli_pi32(p, 3), _mm_set1_pi32(0x001f));
    return _mm_sub_pi32(_mm_or_si64(_mm_or_si64(r, g), b), _mm_set1_pi32(0x8000));
}

static void convert(void) {
    const __m64 bias = _mm_set1_pi16((short)0x8000);
    for (int i = 0; i < W * H; i += 4) {
        __m64 lo = to565(*(__m64 *)&back[i]), hi = to565(*(__m64 *)&back[i + 2]);
        *(__m64 *)&front[i] = _mm_add_pi16(_mm_packs_pi32(lo, hi), bias);
    }
}

unsigned blit_run(int frames) {
    unsigned s = 99, sum = 0;
    for (int i = 0; i < SW * SH; i++) {
        int x = i % SW - SW / 2, y = i / SW - SH / 2, r2 = x * x + y * y;
        unsigned a = r2 >= 1024 ? 0 : 255 - r2 / 4;
        sprite[i] = a << 24 | (lcg(&s) & 0xffffff);
    }
    for (int f = 0; f < frames; f++) {
        for (int y = 0; y < H; y++)
            for (int x = 0; x < W; x++) back[y * W + x] = (y + f) * 0x010101u ^ x;
        for (int n = 0; n < SPRITES; n++) {
            int x0 = (lcg(&s) % ((W - SW) / 2)) * 2, y0 = lcg(&s) % (H - SH);
            for (int y = 0; y < SH; y++) blend_row(&back[(y0 + y) * W + x0], &sprite[y * SW], SW);
        }
        convert();
        const unsigned *p = (const unsigned *)front;
        for (int i = 0; i < W * H / 2; i += 3) sum = (sum << 5 | sum >> 27) ^ p[i];
    }
    _mm_empty();
    return sum;
}

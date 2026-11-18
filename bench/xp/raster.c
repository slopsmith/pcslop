/* A software renderer of the Quake era: x87 float triangle setup, then
   integer inner loops with edge functions, a 16-bit z-buffer, affine texture
   mapping and Gouraud shading through a lighting table. */
#include "bench.h"

enum { RW = 320, RH = 240, TRIS = 1500 };

static unsigned short fb[RW * RH], zb[RW * RH], shade[32][256];
static unsigned char tex[64 * 64];

typedef struct {
    float x, y, z, u, v, l;
} vert;

static int fix12(float f) {
    return (int)(f * 4096.0f);
}

static void draw(const vert *a, const vert *b, const vert *c) {
    float area = (b->x - a->x) * (c->y - a->y) - (c->x - a->x) * (b->y - a->y);
    if (area < 16.0f) return;
    int x0 = (int)a->x, y0 = (int)a->y, x1 = (int)b->x, y1 = (int)b->y, x2 = (int)c->x, y2 = (int)c->y;
    int minx = x0 < x1 ? x0 : x1, maxx = x0 > x1 ? x0 : x1, miny = y0 < y1 ? y0 : y1, maxy = y0 > y1 ? y0 : y1;
    if (x2 < minx) minx = x2;
    if (x2 > maxx) maxx = x2;
    if (y2 < miny) miny = y2;
    if (y2 > maxy) maxy = y2;
    if (minx < 0) minx = 0;
    if (miny < 0) miny = 0;
    if (maxx > RW - 1) maxx = RW - 1;
    if (maxy > RH - 1) maxy = RH - 1;

    float inv = 1.0f / area;
    float ax = (float)minx - a->x, ay = (float)miny - a->y;
#define GRAD(f, dx, dy, base)                                                      \
    float d##f##1 = b->f - a->f, d##f##2 = c->f - a->f;                            \
    int dx = fix12((d##f##1 * (c->y - a->y) - d##f##2 * (b->y - a->y)) * inv);     \
    int dy = fix12((d##f##2 * (b->x - a->x) - d##f##1 * (c->x - a->x)) * inv);     \
    int base = fix12(a->f) + (int)(ax * dx) + (int)(ay * dy);
    GRAD(z, zdx, zdy, zrow)
    GRAD(u, udx, udy, urow)
    GRAD(v, vdx, vdy, vrow)
    GRAD(l, ldx, ldy, lrow)
#undef GRAD

    int e0dx = y1 - y2, e0dy = x2 - x1, e1dx = y2 - y0, e1dy = x0 - x2, e2dx = y0 - y1, e2dy = x1 - x0;
    int e0 = (minx - x1) * e0dx + (miny - y1) * e0dy;
    int e1 = (minx - x2) * e1dx + (miny - y2) * e1dy;
    int e2 = (minx - x0) * e2dx + (miny - y0) * e2dy;
    for (int y = miny; y <= maxy; y++) {
        int w0 = e0, w1 = e1, w2 = e2, z = zrow, u = urow, v = vrow, l = lrow;
        unsigned short *frow = &fb[y * RW], *zr = &zb[y * RW];
        for (int x = minx; x <= maxx; x++) {
            if ((w0 | w1 | w2) >= 0) {
                unsigned zz = (unsigned)z >> 4;
                if (zz < zr[x]) {
                    zr[x] = (unsigned short)zz;
                    int li = (l >> 12) & 31;
                    frow[x] = shade[li][tex[((v >> 12) & 63) * 64 + ((u >> 12) & 63)]];
                }
            }
            w0 += e0dx, w1 += e1dx, w2 += e2dx;
            z += zdx, u += udx, v += vdx, l += ldx;
        }
        e0 += e0dy, e1 += e1dy, e2 += e2dy;
        zrow += zdy, urow += udy, vrow += vdy, lrow += ldy;
    }
}

unsigned raster_run(int frames) {
    unsigned s = 2024, sum = 0;
    for (int i = 0; i < 64 * 64; i++) tex[i] = (unsigned char)(((i & 7) ^ (i >> 9)) * 29 + (lcg(&s) & 15));
    for (int l = 0; l < 32; l++)
        for (int t = 0; t < 256; t++) {
            int r = (t * l) >> 5, g = ((t * 3 + 40) & 255) * l >> 5, b = ((255 - t) * l) >> 5;
            shade[l][t] = (unsigned short)((r >> 3) << 11 | (g >> 2) << 5 | (b >> 3));
        }
    for (int f = 0; f < frames; f++) {
        for (int i = 0; i < RW * RH; i++) fb[i] = 0, zb[i] = 0xffff;
        for (int t = 0; t < TRIS; t++) {
            vert v[3];
            float cx = (float)(lcg(&s) % (RW + 40)) - 20.0f, cy = (float)(lcg(&s) % (RH + 40)) - 20.0f;
            float size = 4.0f + (float)(lcg(&s) % 48);
            for (int k = 0; k < 3; k++) {
                v[k].x = cx + ((float)(lcg(&s) % 256) / 128.0f - 1.0f) * size;
                v[k].y = cy + ((float)(lcg(&s) % 256) / 128.0f - 1.0f) * size;
                v[k].z = 1.0f + (float)(lcg(&s) % 4000) / 256.0f * 15.0f;
                v[k].u = (float)(lcg(&s) % 128);
                v[k].v = (float)(lcg(&s) % 128);
                v[k].l = 0.5f + (float)(lcg(&s) % 30);
            }
            /* Both windings, as a mesh sends; the back-facing half is culled. */
            draw(&v[0], &v[1], &v[2]);
        }
        const unsigned *p = (const unsigned *)fb;
        for (int i = 0; i < RW * RH / 2; i += 5) sum = (sum << 3 | sum >> 29) + p[i];
        p = (const unsigned *)zb;
        for (int i = 0; i < RW * RH / 2; i += 7) sum ^= p[i] + (unsigned)i;
    }
    return sum;
}

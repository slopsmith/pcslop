/* Vertex transform and lighting the way a Direct3D 7-9 era game or its
   software T&L path does it: single-precision x87 with the FPU set to 24-bit
   precision (Direct3D does that unless the app passes FPU_PRESERVE),
   a perspective divide and float-to-int conversions for the colour. */
#include <float.h>
#include <math.h>
#include "bench.h"

enum { NV = 4096 };

typedef struct {
    float x, y, z, nx, ny, nz;
} vertex;

typedef struct {
    float sx, sy, sz, rhw;
    unsigned color;
} tlvertex;

static vertex in[NV];
static tlvertex out[NV];

static int clamp255(float f) {
    int i = (int)f;
    return i > 255 ? 255 : i;
}

unsigned xform_run(int frames) {
    unsigned s = 7;
    for (int i = 0; i < NV; i++) {
        vertex *v = &in[i];
        v->x = (lcg(&s) & 0xffff) / 32768.0f - 1.0f;
        v->y = (lcg(&s) & 0xffff) / 32768.0f - 1.0f;
        v->z = (lcg(&s) & 0xffff) / 32768.0f - 1.0f;
        float nx = v->x + 0.01f, ny = v->y, nz = v->z;
        float inv = 1.0f / sqrtf(nx * nx + ny * ny + nz * nz);
        v->nx = nx * inv;
        v->ny = ny * inv;
        v->nz = nz * inv;
    }

    unsigned cw = _controlfp(0, 0);
    _controlfp(_PC_24, _MCW_PC);
    unsigned sum = 0;
    for (int f = 0; f < frames; f++) {
        float a = f * 0.0123f, b = f * 0.0071f;
        float sa = sinf(a), ca = cosf(a), sb = sinf(b), cb = cosf(b);
        float m[3][3] = {
            { ca, sa * sb, -sa * cb },
            { 0.0f, cb, sb },
            { sa, -ca * sb, ca * cb },
        };
        float lx = cosf(a * 3.0f), ly = 0.8f, lz = sinf(a * 3.0f);
        float il = 1.0f / sqrtf(lx * lx + ly * ly + lz * lz);
        lx *= il, ly *= il, lz *= il;
        float hx = lx, hy = ly, hz = lz - 1.0f;
        float ih = 1.0f / sqrtf(hx * hx + hy * hy + hz * hz);
        hx *= ih, hy *= ih, hz *= ih;

        for (int i = 0; i < NV; i++) {
            const vertex *v = &in[i];
            tlvertex *o = &out[i];
            float px = v->x * m[0][0] + v->y * m[1][0] + v->z * m[2][0];
            float py = v->x * m[0][1] + v->y * m[1][1] + v->z * m[2][1];
            float pz = v->x * m[0][2] + v->y * m[1][2] + v->z * m[2][2] + 3.0f;
            float rhw = 1.0f / pz;
            o->sx = 320.0f + px * 480.0f * rhw;
            o->sy = 240.0f - py * 480.0f * rhw;
            o->sz = (pz - 1.0f) * 0.125f;
            o->rhw = rhw;

            float nx = v->nx * m[0][0] + v->ny * m[1][0] + v->nz * m[2][0];
            float ny = v->nx * m[0][1] + v->ny * m[1][1] + v->nz * m[2][1];
            float nz = v->nx * m[0][2] + v->ny * m[1][2] + v->nz * m[2][2];
            float d = nx * lx + ny * ly + nz * lz;
            if (d < 0.0f) d = 0.0f;
            float sp = nx * hx + ny * hy + nz * hz;
            if (sp < 0.0f) sp = 0.0f;
            sp *= sp, sp *= sp, sp *= sp, sp *= sp;
            o->color = 0xff000000u | clamp255(32.0f + d * 180.0f + sp * 255.0f) << 16 |
                       clamp255(24.0f + d * 150.0f + sp * 255.0f) << 8 | clamp255(40.0f + d * 120.0f + sp * 255.0f);
        }
        for (int i = 0; i < NV; i++)
            sum = sum * 33 + (out[i].color ^ ((int)(out[i].sx * 16.0f) << 12 | (int)(out[i].sy * 16.0f)));
    }
    _controlfp(cw, _MCW_PC);
    return sum;
}

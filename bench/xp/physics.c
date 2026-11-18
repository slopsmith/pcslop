/* A rigid-sphere physics step as a game engine runs it each frame: x87
   single-precision floats, a uniform grid broadphase, sqrt and divides in the
   contact solver, and wall bounces. */
#include <math.h>
#include "bench.h"

enum { NB = 600, CELLS = 16, NC = CELLS * CELLS * CELLS };

#define BOX 48.0f
#define CELL (BOX / CELLS)

typedef struct {
    float x, y, z, vx, vy, vz, r, im;
} body;

static body b[NB];
static int head[NC], next[NB];

static int cell_of(float v) {
    int c = (int)(v * (1.0f / CELL));
    return c < 0 ? 0 : c >= CELLS ? CELLS - 1 : c;
}

static void collide(body *p, body *q) {
    float dx = q->x - p->x, dy = q->y - p->y, dz = q->z - p->z;
    float rr = p->r + q->r, d2 = dx * dx + dy * dy + dz * dz;
    if (d2 >= rr * rr || d2 < 1e-6f) return;
    float d = sqrtf(d2), nx = dx / d, ny = dy / d, nz = dz / d;
    float push = (rr - d) / (p->im + q->im);
    p->x -= nx * push * p->im, p->y -= ny * push * p->im, p->z -= nz * push * p->im;
    q->x += nx * push * q->im, q->y += ny * push * q->im, q->z += nz * push * q->im;
    float vn = (q->vx - p->vx) * nx + (q->vy - p->vy) * ny + (q->vz - p->vz) * nz;
    if (vn >= 0.0f) return;
    float j = -1.6f * vn / (p->im + q->im);
    p->vx -= j * nx * p->im, p->vy -= j * ny * p->im, p->vz -= j * nz * p->im;
    q->vx += j * nx * q->im, q->vy += j * ny * q->im, q->vz += j * nz * q->im;
}

static void wall(float *pos, float *vel, float r) {
    if (*pos < r) *pos = r, *vel = -*vel * 0.7f;
    else if (*pos > BOX - r) *pos = BOX - r, *vel = -*vel * 0.7f;
}

unsigned physics_run(int steps) {
    unsigned s = 777, sum = 0;
    for (int i = 0; i < NB; i++) {
        body *p = &b[i];
        p->r = 0.5f + (lcg(&s) % 64) / 128.0f;
        p->im = 1.0f / (p->r * p->r * p->r);
        p->x = p->r + (lcg(&s) % 1000) * ((BOX - 2 * p->r) / 1000.0f);
        p->y = p->r + (lcg(&s) % 1000) * ((BOX - 2 * p->r) / 1000.0f);
        p->z = p->r + (lcg(&s) % 1000) * ((BOX - 2 * p->r) / 1000.0f);
        p->vx = (lcg(&s) % 200) / 20.0f - 5.0f;
        p->vy = (lcg(&s) % 200) / 20.0f - 5.0f;
        p->vz = (lcg(&s) % 200) / 20.0f - 5.0f;
    }
    const float dt = 1.0f / 60.0f;
    for (int t = 0; t < steps; t++) {
        for (int i = 0; i < NC; i++) head[i] = -1;
        for (int i = 0; i < NB; i++) {
            body *p = &b[i];
            p->vy -= 9.8f * dt;
            p->x += p->vx * dt, p->y += p->vy * dt, p->z += p->vz * dt;
            wall(&p->x, &p->vx, p->r);
            wall(&p->y, &p->vy, p->r);
            wall(&p->z, &p->vz, p->r);
            int c = (cell_of(p->z) * CELLS + cell_of(p->y)) * CELLS + cell_of(p->x);
            next[i] = head[c];
            head[c] = i;
        }
        for (int i = 0; i < NB; i++) {
            body *p = &b[i];
            int cx = cell_of(p->x), cy = cell_of(p->y), cz = cell_of(p->z);
            for (int z = cz - 1; z <= cz + 1; z++)
                for (int y = cy - 1; y <= cy + 1; y++)
                    for (int x = cx - 1; x <= cx + 1; x++) {
                        if (x < 0 || y < 0 || z < 0 || x >= CELLS || y >= CELLS || z >= CELLS) continue;
                        for (int j = head[(z * CELLS + y) * CELLS + x]; j >= 0; j = next[j])
                            if (j > i) collide(p, &b[j]);
                    }
        }
        for (int i = 0; i < NB; i += 3)
            sum = sum * 31 + (unsigned)(int)(b[i].x * 64.0f) + ((unsigned)(int)(b[i].y * 64.0f) << 10);
    }
    return sum;
}

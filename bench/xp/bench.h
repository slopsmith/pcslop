#ifndef BENCH_H
#define BENCH_H

#ifdef __cplusplus
extern "C" {
#endif

double now(void);
void report(const char *name, double secs, double work, const char *unit);
void check(const char *name, unsigned got, unsigned want);

unsigned xform_run(int frames);
unsigned blit_run(int frames);
unsigned path_run(int queries);
unsigned entity_run(int frames);
unsigned raster_run(int frames);
unsigned physics_run(int steps);

static inline unsigned lcg(unsigned *s) {
    *s = *s * 1103515245u + 12345u;
    return *s >> 8;
}

#ifdef __cplusplus
}
#endif

#endif

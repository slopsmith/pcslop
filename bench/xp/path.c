/* Game AI pathfinding: A* with a binary heap on an 8-connected tile map with
   rooms and scattered walls. Branchy integer code with unpredictable
   branches and scattered loads. */
#include "bench.h"

enum { GW = 128, GH = 128, N = GW * GH };

static unsigned char wall[N];
static int cost[N];
static unsigned seen[N], closed[N];
static int heap_f[N * 8], heap_i[N * 8], heap_n;

static void push(int f, int i) {
    int k = heap_n++;
    while (k > 0) {
        int p = (k - 1) / 2;
        if (heap_f[p] <= f) break;
        heap_f[k] = heap_f[p];
        heap_i[k] = heap_i[p];
        k = p;
    }
    heap_f[k] = f;
    heap_i[k] = i;
}

static int pop(void) {
    int top = heap_i[0], f = heap_f[--heap_n], i = heap_i[heap_n], k = 0;
    for (;;) {
        int c = 2 * k + 1;
        if (c >= heap_n) break;
        if (c + 1 < heap_n && heap_f[c + 1] < heap_f[c]) c++;
        if (f <= heap_f[c]) break;
        heap_f[k] = heap_f[c];
        heap_i[k] = heap_i[c];
        k = c;
    }
    heap_f[k] = f;
    heap_i[k] = i;
    return top;
}

static int heuristic(int a, int b) {
    int dx = a % GW - b % GW, dy = a / GW - b / GW;
    if (dx < 0) dx = -dx;
    if (dy < 0) dy = -dy;
    return dx > dy ? 14 * dy + 10 * (dx - dy) : 14 * dx + 10 * (dy - dx);
}

static int astar(int from, int to, unsigned stamp, int *expanded) {
    static const int dx[8] = { 1, -1, 0, 0, 1, 1, -1, -1 }, dy[8] = { 0, 0, 1, -1, 1, -1, 1, -1 };
    heap_n = 0;
    cost[from] = 0;
    seen[from] = stamp;
    push(heuristic(from, to), from);
    while (heap_n) {
        int cur = pop();
        if (cur == to) return cost[cur];
        if (closed[cur] == stamp) continue;
        closed[cur] = stamp;
        ++*expanded;
        int cx = cur % GW, cy = cur / GW;
        for (int d = 0; d < 8; d++) {
            int nx = cx + dx[d], ny = cy + dy[d];
            if (nx < 0 || ny < 0 || nx >= GW || ny >= GH) continue;
            int n = ny * GW + nx;
            if (wall[n] || closed[n] == stamp) continue;
            if (d >= 4 && (wall[cy * GW + nx] || wall[ny * GW + cx])) continue;
            int c = cost[cur] + (d < 4 ? 10 : 14);
            if (seen[n] == stamp && c >= cost[n]) continue;
            seen[n] = stamp;
            cost[n] = c;
            push(c + heuristic(n, to), n);
        }
    }
    return -1;
}

unsigned path_run(int queries) {
    unsigned s = 4242, sum = 0;
    for (int i = 0; i < N; i++) {
        wall[i] = lcg(&s) % 100 < 18;
        seen[i] = closed[i] = 0;
    }
    for (int r = 0; r < 40; r++) {
        int x0 = lcg(&s) % (GW - 16), y0 = lcg(&s) % (GH - 16), w = 4 + lcg(&s) % 12, h = 4 + lcg(&s) % 12;
        for (int x = x0; x <= x0 + w; x++) wall[y0 * GW + x] = wall[(y0 + h) * GW + x] = 1;
        for (int y = y0; y <= y0 + h; y++) wall[y * GW + x0] = wall[y * GW + x0 + w] = 1;
        wall[(y0 + h / 2) * GW + x0] = wall[(y0 + h) * GW + x0 + w / 2] = 0;
    }
    for (int q = 0; q < queries; q++) {
        int a, b;
        do a = lcg(&s) % N; while (wall[a]);
        do b = lcg(&s) % N; while (wall[b]);
        int expanded = 0, c = astar(a, b, q + 1, &expanded);
        sum = sum * 31 + (unsigned)c + (unsigned)expanded * 7;
    }
    return sum;
}

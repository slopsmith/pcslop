// Entity updates as a C++ game engine of the era runs them: a list of
// polymorphic objects updated through virtual calls each frame, with
// per-type state machines, deaths and respawns that change which class sits
// in a slot, so the call sites see many targets.
#include <new>
#include "bench.h"

namespace {

enum { SLOTS = 4096, ARENA = 1 << 16 };

struct World {
    int px, py;
    unsigned rng;
    unsigned hash;
};

int iabs(int v) { return v < 0 ? -v : v; }

struct Entity {
    int x, y, vx, vy, hp, state, timer;
    bool dead;

    Entity(World &w) : x(lcg(&w.rng) % ARENA), y(lcg(&w.rng) % ARENA), vx(0), vy(0), hp(100), state(0), timer(0),
                       dead(false) {}
    virtual void update(World &w) { timer++; (void)w; }
    virtual unsigned kind() const { return 0; }
    virtual void hit(int damage) {
        hp -= damage;
        if (hp <= 0) dead = true;
    }
};

struct Monster : Entity {
    enum { IDLE, PATROL, CHASE, ATTACK, FLEE };
    int home_x, home_y;

    Monster(World &w) : Entity(w), home_x(x), home_y(y) {}
    void update(World &w) override {
        int dx = w.px - x, dy = w.py - y, dist = iabs(dx) + iabs(dy);
        switch (state) {
        case IDLE:
            if (dist < 12000) state = CHASE;
            else if (++timer > 30) state = PATROL, timer = 0, vx = (int)(lcg(&w.rng) % 512) - 256, vy = (int)(lcg(&w.rng) % 512) - 256;
            break;
        case PATROL:
            x += vx, y += vy;
            if (dist < 12000) state = CHASE;
            else if (iabs(x - home_x) + iabs(y - home_y) > 8000) vx = -vx, vy = -vy;
            else if (++timer > 90) state = IDLE, timer = 0;
            break;
        case CHASE:
            x += dx > 0 ? 300 : -300;
            y += dy > 0 ? 300 : -300;
            if (dist < 1500) state = ATTACK, timer = 0;
            else if (dist > 20000) state = IDLE;
            if (hp < 30) state = FLEE;
            break;
        case ATTACK:
            if (++timer % 8 == 0) w.hash = w.hash * 31 + (unsigned)dist;
            if (dist > 2500) state = CHASE;
            hit(1);
            break;
        case FLEE:
            x -= dx > 0 ? 400 : -400;
            y -= dy > 0 ? 400 : -400;
            if (dist > 25000) state = IDLE, hp += 40;
            break;
        }
    }
    unsigned kind() const override { return 1; }
};

struct Projectile : Entity {
    Projectile(World &w) : Entity(w) {
        vx = (int)(lcg(&w.rng) % 2048) - 1024;
        vy = (int)(lcg(&w.rng) % 2048) - 1024;
    }
    void update(World &w) override {
        x += vx, y += vy;
        vy += 8;
        if (x < 0 || y < 0 || x >= ARENA || y >= ARENA) dead = true;
        else if (iabs(x - w.px) < 600 && iabs(y - w.py) < 600) dead = true, w.hash ^= (unsigned)(x * 7 + y);
    }
    unsigned kind() const override { return 2; }
};

struct Pickup : Entity {
    Pickup(World &w) : Entity(w) {}
    void update(World &w) override {
        static const signed char bob[16] = { 0, 2, 4, 5, 6, 5, 4, 2, 0, -2, -4, -5, -6, -5, -4, -2 };
        y += bob[++timer & 15];
        if (iabs(x - w.px) < 900 && iabs(y - w.py) < 900) dead = true, w.hash += (unsigned)timer;
        if (timer > 2000) dead = true;
    }
    unsigned kind() const override { return 3; }
};

struct Door : Entity {
    enum { CLOSED, OPENING, OPEN, CLOSING };
    Door(World &w) : Entity(w) {}
    void update(World &w) override {
        bool near = iabs(x - w.px) + iabs(y - w.py) < 6000;
        switch (state) {
        case CLOSED: if (near) state = OPENING; break;
        case OPENING: if (++timer >= 16) state = OPEN; break;
        case OPEN: if (!near) state = CLOSING; break;
        case CLOSING: if (--timer <= 0) state = CLOSED; else if (near) state = OPENING; break;
        }
    }
    unsigned kind() const override { return 4; }
    void hit(int) override {}
};

struct Particle : Entity {
    Particle(World &w) : Entity(w) {
        vx = (int)(lcg(&w.rng) % 1024) - 512;
        vy = (int)(lcg(&w.rng) % 1024) - 512;
        hp = 20 + lcg(&w.rng) % 40;
    }
    void update(World &) override {
        x += vx, y += vy;
        vx -= vx >> 4, vy -= vy >> 4;
        if (--hp <= 0) dead = true;
    }
    unsigned kind() const override { return 5; }
};

union Slot {
    Monster m;
    Projectile p;
    Pickup u;
    Door d;
    Particle a;
    Slot() {}
};

Slot slots[SLOTS];
Entity *list[SLOTS];

Entity *spawn(World &w, int i) {
    void *at = &slots[i];
    switch (lcg(&w.rng) % 8) {
    case 0: case 1: return new (at) Monster(w);
    case 2: case 3: return new (at) Projectile(w);
    case 4: return new (at) Pickup(w);
    case 5: return new (at) Door(w);
    default: return new (at) Particle(w);
    }
}

}

unsigned entity_run(int frames) {
    World w = { ARENA / 2, ARENA / 2, 31337, 0 };
    for (int i = 0; i < SLOTS; i++) list[i] = spawn(w, i);
    static const int path[8][2] = { { 900, 0 }, { 600, 600 }, { 0, 900 }, { -600, 600 },
                                    { -900, 0 }, { -600, -600 }, { 0, -900 }, { 600, -600 } };
    for (int f = 0; f < frames; f++) {
        w.px += path[(f / 25) & 7][0];
        w.py += path[(f / 25) & 7][1];
        for (int i = 0; i < SLOTS; i++) list[i]->update(w);
        for (int i = 0; i < SLOTS; i++) {
            Entity *e = list[i];
            if (i % 61 == f % 61) e->hit(5);
            if (e->dead) list[i] = spawn(w, i);
        }
        for (int i = 0; i < SLOTS; i += 5) w.hash = w.hash * 33 + list[i]->kind() + (unsigned)list[i]->x;
    }
    return w.hash;
}

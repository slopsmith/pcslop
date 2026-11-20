// Texture sampling as tex.rs does it, compiled once into its own library
// and linked into each fragment shader: the per-pipeline source then never
// names a texture type, which saves its compile about 0.25 s.

struct TexSlot { texture2d<float> t; };
struct SamplerSlot { sampler s; };

struct TexTables {
    const device TexSlot *images;
    const device SamplerSlot *samplers;
};

static bool wrap(uint mode, int i, uint size, thread uint &out) {
    int n = int(size);
    int m;
    switch (mode) {
    case 0: m = ((i % n) + n) % n; break;
    case 1: { int r = ((i % (2 * n)) + 2 * n) % (2 * n); m = r < n ? r : 2 * n - 1 - r; break; }
    case 2: case 4: m = clamp(i, 0, n - 1); break;
    case 3: case 5: m = clamp(i < 0 ? -1 - i : i, 0, n - 1); break;
    default:
        m = (mode == 7 && i < 0) ? -1 - i : i;
        if (m < 0 || m >= n) return false;
    }
    out = uint(m);
    return true;
}

// As Texture::coords in tex.rs: (s, t, r), or for a cube map the face's
// (s, t) and its number.
static float3 tex_coords(constant TexDesc &t, float4 c, bool projected) {
    if (t.kind == 2) {
        float3 a = abs(c.xyz);
        float face, sc, tc, ma;
        if (a.x >= a.y && a.x >= a.z) {
            ma = a.x; tc = -c.y;
            if (c.x >= 0.0) { face = 0; sc = -c.z; } else { face = 1; sc = c.z; }
        } else if (a.y >= a.z) {
            ma = a.y; sc = c.x;
            if (c.y >= 0.0) { face = 2; tc = c.z; } else { face = 3; tc = -c.z; }
        } else {
            ma = a.z; tc = -c.y;
            if (c.z >= 0.0) { face = 4; sc = c.x; } else { face = 5; sc = -c.x; }
        }
        if (!(ma > 0.0)) ma = 1.0;
        return float3((sc / ma + 1.0) / 2.0, (tc / ma + 1.0) / 2.0, face);
    }
    return (projected && c.w != 0.0) ? c.xyz / c.w : c.xyz;
}

static float channel(float4 c, uint m) {
    return m <= 3 ? c[m] : m == 5 ? 1.0 : 0.0;
}

static float4 mapped(constant TexDesc &t, float4 c) {
    return float4(channel(c, t.map & 7), channel(c, (t.map >> 3) & 7), channel(c, (t.map >> 6) & 7), channel(c, (t.map >> 9) & 7));
}

// One texel after the wrap modes, as Texture::fetch in tex.rs. Inlining it
// at every tap slows pipeline compiles down.
__attribute__((noinline)) static float4 fetch(constant TexDesc &t, TexTables tb, uint level, int x, int y, int z) {
    uint w = max(t.width >> level, 1u), h = max(t.height >> level, 1u);
    uint ux, uy, uz = uint(z);
    bool zin = t.kind != 1 || wrap((t.filter >> 6) & 7, z, max(t.depth >> level, 1u), uz);
    if (wrap(t.filter & 7, x, w, ux) && wrap((t.filter >> 3) & 7, y, h, uy) && zin) {
        texture2d<float> img = tb.images[t.first + level * t.layers + (t.kind == 0 ? 0 : uz)].t;
        return mapped(t, img.read(uint2(ux, uy)));
    }
    uint b = t.border;
    return float4(float((b >> 16) & 0xff), float((b >> 8) & 0xff), float(b & 0xff), float(b >> 24)) / 255.0;
}

static float4 bilinear(constant TexDesc &t, TexTables tb, uint level, float2 uv, int z) {
    uv -= 0.5;
    float2 f0 = floor(uv);
    float2 f = uv - f0;
    int x0 = int(f0.x), y0 = int(f0.y);
    float4 t00 = fetch(t, tb, level, x0, y0, z), t10 = fetch(t, tb, level, x0 + 1, y0, z);
    float4 t01 = fetch(t, tb, level, x0, y0 + 1, z), t11 = fetch(t, tb, level, x0 + 1, y0 + 1, z);
    float4 top = t00 + (t10 - t00) * f.x, bottom = t01 + (t11 - t01) * f.x;
    return top + (bottom - top) * f.y;
}

// Samples one image of a texture with a Metal sampler, as `fetch` and
// `bilinear` would.
static float4 native_tap(constant TexDesc &t, TexTables tb, uint level, uint face, float2 st, bool linear) {
    texture2d<float> img = tb.images[t.first + level * t.layers + face].t;
    return mapped(t, img.sample(tb.samplers[t.sampler + (linear ? 1 : 0)].s, st, metal::level(0)));
}

static float4 sample(constant TexDesc &t, TexTables tb, float4 coords, uint op) {
    float3 str = tex_coords(t, coords, op == 3);
    float2 st = str.xy;
    float2 size = float2(t.width, t.height);
    float2 dx = dfdx(st) * size, dy = dfdy(st) * size;
    float rho = max(dot(dx, dx), dot(dy, dy));
    float lod = rho > 0.0 ? 0.5 * log2(rho) : 0.0;
    if (op == 4) lod += coords.w;
    if (!t.enabled) return float4(0, 0, 0, 1);
    bool minify = lod > 0.0;
    uint mip = (t.filter >> 13) & 3;
    uint level = 0;
    if (minify && mip != 0) level = min(uint(round(lod)) + ((t.filter >> 17) & 0xf), t.nlevels - 1);
    bool linear = minify ? ((t.filter >> 11) & 3) == 2 : ((t.filter >> 9) & 3) == 2;
    if (!t.manual) return native_tap(t, tb, level, t.kind == 2 ? uint(str.z) : 0, st, linear);
    float2 wh = float2(max(t.width >> level, 1u), max(t.height >> level, 1u));
    float2 uv = st * wh;
    int z0 = 0;
    float fz = 0.0;
    if (t.kind == 2) {
        z0 = int(str.z);
    } else if (t.kind == 1) {
        float dd = float(max(t.depth >> level, 1u));
        float wz = linear ? str.z * dd - 0.5 : floor(str.z * dd);
        z0 = int(floor(wz));
        fz = linear ? wz - floor(wz) : 0.0;
    }
    if (!linear) return fetch(t, tb, level, int(floor(uv.x)), int(floor(uv.y)), z0);
    float4 a = bilinear(t, tb, level, uv, z0);
    if (fz == 0.0) return a;
    return a + (bilinear(t, tb, level, uv, z0 + 1) - a) * fz;
}

[[visible]] float4 sample_tex(constant TexDesc &t, const device uchar *images, const device uchar *samplers, float4 coords, uint op) {
    TexTables tb = {(const device TexSlot *)images, (const device SamplerSlot *)samplers};
    float4 c = sample(t, tb, coords, op);
    if (t.gamma) {
        float3 lo = c.xyz / 12.92, hi = pow((c.xyz + 0.055) / 1.055, 2.4);
        c.xyz = select(hi, lo, c.xyz <= 0.04045);
    }
    return c;
}

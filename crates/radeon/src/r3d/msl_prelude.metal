#include <metal_stdlib>
using namespace metal;

// Texture fetch and filtering, the alpha test, depth and stencil, blending,
// the ROP and colour packing, ported from tex.rs and raster.rs so both
// backends draw the same pixels. Everything reads and writes the VRAM buffer.

struct TexDesc {
    uint enabled, stage, base, limit, width, height, format, nlevels, filter, border;
    uint sel[4];
    uint signed_comps;
    uint kind, depth;
    uint off[12];
    uint pitch[12];
    uint slice[12];
    uint native, first, layers, map, sampler;
    uint pad[3];
};

struct TexSlot { texture2d<float> t; };
struct SamplerSlot { sampler s; };

struct TexTables {
    const device TexSlot *images;
    const device SamplerSlot *samplers;
};

struct ColourDesc {
    uint offset, pitch, format, pad;
};

struct Uniforms {
    float4 consts[32];
    TexDesc tex[16];
    ColourDesc cb[4];
    uint ncb, has_depth, depth_base, depth_pitch, z16;
    uint zb_cntl, cb_clear, cb_clear_value, zs_cntl, stencil_ref;
    float depth_scale, depth_bias;
    int clips[16];
    uint clip_rule, blend, ablend, channel_mask, constant_colour, rop, alpha_func, out_fmt;
    uint vram_size;
    uint fog;
    float fog_factor;
    float fog_colour[3];
    uint poly_enable;
    float poly[4];
};

static uint rd8(const device uchar *d, uint a, uint limit) {
    return d[a < limit ? a : a % limit];
}

static uint rd16(const device uchar *d, uint a, uint limit) {
    if ((a & 1) == 0 && a + 2 <= limit) return *(const device ushort *)(d + a);
    return rd8(d, a, limit) | rd8(d, a + 1, limit) << 8;
}

static uint rd32(const device uchar *d, uint a, uint limit) {
    if ((a & 3) == 0 && a + 4 <= limit) return *(const device uint *)(d + a);
    return rd16(d, a, limit) | rd16(d, a + 2, limit) << 16;
}

static float un(uint v, uint bits, bool sgn) {
    v &= (1u << bits) - 1u;
    if (sgn && bits > 1) return max(float(int(v << (32 - bits)) >> (32 - bits)) / float((1u << (bits - 1)) - 1u), -1.0);
    return float(v) / float((1u << bits) - 1u);
}

static float4 fields(uint v, uint w0, uint w1, uint w2, uint w3, uint sg) {
    float4 out = float4(0, 0, 0, 1);
    uint shift = 0;
    out.x = un(v, w0, sg & 1);
    shift += w0;
    if (w1) { out.y = un(v >> shift, w1, sg & 2); shift += w1; }
    if (w2) { out.z = un(v >> shift, w2, sg & 4); shift += w2; }
    if (w3) { out.w = un(v >> shift, w3, sg & 8); }
    return out;
}

static float hf(uint h) {
    return float(as_type<half>(ushort(h)));
}

static uint2 block(uint format) {
    switch (format) {
    case 0: case 2: case 5: case 0x12: return uint2(1, 1);
    case 1: case 3: case 6: case 7: case 10: case 11: case 0x14: case 0x15: case 0x18: return uint2(2, 1);
    case 14: case 0x1a: case 0x1c: return uint2(8, 1);
    case 0x1d: return uint2(16, 1);
    case 0xf: return uint2(8, 4);
    case 0x10: case 0x11: return uint2(16, 4);
    default: return uint2(4, 1);
    }
}

static float3 rgb565(uint c) {
    return float3(float((c >> 11) & 31) / 31.0, float((c >> 5) & 63) / 63.0, float(c & 31) / 31.0);
}

static float4 dxt(uint format, const device uchar *d, uint a, uint limit, uint x, uint y) {
    uint colour_at = format == 0xf ? 0 : 8;
    uint c0 = rd16(d, a + colour_at, limit), c1 = rd16(d, a + colour_at + 2, limit);
    float3 ca = rgb565(c0), cb = rgb565(c1);
    uint bits = (rd8(d, a + colour_at + 4 + y, limit) >> (2 * x)) & 3;
    float3 rgb;
    float alpha = 1.0;
    if (c0 > c1 || format != 0xf) {
        rgb = bits == 0 ? ca : bits == 1 ? cb : bits == 2 ? ca * (2.0 / 3.0) + cb * (1.0 / 3.0) : ca * (1.0 / 3.0) + cb * (2.0 / 3.0);
    } else {
        rgb = bits == 0 ? ca : bits == 1 ? cb : bits == 2 ? ca * 0.5 + cb * 0.5 : float3(0);
        if (bits == 3) alpha = 0.0;
    }
    uint i = y * 4 + x;
    if (format == 0x10) {
        alpha = float((rd8(d, a + i / 2, limit) >> (4 * (i % 2))) & 15) / 15.0;
    } else if (format == 0x11) {
        float a0 = float(rd8(d, a, limit)), a1 = float(rd8(d, a + 1, limit));
        uint bit = 3 * i;
        uint idx = 0;
        for (uint k = 0; k < 3; k++) {
            uint b = bit + k;
            idx |= ((rd8(d, a + 2 + b / 8, limit) >> (b % 8)) & 1) << k;
        }
        float v;
        if (idx == 0) v = a0;
        else if (idx == 1) v = a1;
        else if (a0 > a1) v = (float(8 - idx) * a0 + float(idx - 1) * a1) / 7.0;
        else if (idx == 6) v = 0.0;
        else if (idx == 7) v = 255.0;
        else v = (float(6 - idx) * a0 + float(idx - 1) * a1) / 5.0;
        alpha = v / 255.0;
    }
    return float4(rgb.z, rgb.y, rgb.x, alpha);
}

static float4 texel(constant TexDesc &t, const device uchar *d, uint level, uint x, uint y, uint z) {
    uint2 b = block(t.format);
    uint a = t.base + t.off[level] + z * t.slice[level] + (y / b.y) * t.pitch[level] + (x / b.y) * b.x;
    uint l = t.limit;
    switch (t.format) {
    case 0: return fields(rd8(d, a, l), 8, 0, 0, 0, t.signed_comps);
    case 1: return fields(rd16(d, a, l), 16, 0, 0, 0, t.signed_comps);
    case 2: return fields(rd8(d, a, l), 4, 4, 0, 0, t.signed_comps);
    case 3: return fields(rd16(d, a, l), 8, 8, 0, 0, t.signed_comps);
    case 4: return fields(rd32(d, a, l), 16, 16, 0, 0, t.signed_comps);
    case 5: return fields(rd8(d, a, l), 2, 3, 3, 0, t.signed_comps);
    case 6: return fields(rd16(d, a, l), 5, 6, 5, 0, t.signed_comps);
    case 7: return fields(rd16(d, a, l), 5, 5, 6, 0, t.signed_comps);
    case 8: return fields(rd32(d, a, l), 10, 11, 11, 0, t.signed_comps);
    case 9: return fields(rd32(d, a, l), 11, 11, 10, 0, t.signed_comps);
    case 10: return fields(rd16(d, a, l), 4, 4, 4, 4, t.signed_comps);
    case 11: return fields(rd16(d, a, l), 5, 5, 5, 1, t.signed_comps);
    case 12: return fields(rd32(d, a, l), 8, 8, 8, 8, t.signed_comps);
    case 13: return fields(rd32(d, a, l), 10, 10, 10, 2, t.signed_comps);
    case 14: return float4(un(rd16(d, a, l), 16, t.signed_comps & 1), un(rd16(d, a + 2, l), 16, t.signed_comps & 2), un(rd16(d, a + 4, l), 16, t.signed_comps & 4), un(rd16(d, a + 6, l), 16, t.signed_comps & 8));
    case 0xf: case 0x10: case 0x11: return dxt(t.format, d, a, l, x % 4, y % 4);
    case 0x18: return float4(hf(rd16(d, a, l)), 0, 0, 1);
    case 0x19: return float4(hf(rd16(d, a, l)), hf(rd16(d, a + 2, l)), 0, 1);
    case 0x1a: return float4(hf(rd16(d, a, l)), hf(rd16(d, a + 2, l)), hf(rd16(d, a + 4, l)), hf(rd16(d, a + 6, l)));
    case 0x1b: return float4(as_type<float>(rd32(d, a, l)), 0, 0, 1);
    case 0x1c: return float4(as_type<float>(rd32(d, a, l)), as_type<float>(rd32(d, a + 4, l)), 0, 1);
    case 0x1d: return float4(as_type<float>(rd32(d, a, l)), as_type<float>(rd32(d, a + 4, l)), as_type<float>(rd32(d, a + 8, l)), as_type<float>(rd32(d, a + 12, l)));
    default: return fields(rd32(d, a, l), 8, 8, 8, 8, t.signed_comps);
    }
}

static float pick(float4 c, uint s) {
    return s <= 3 ? c[s] : s == 5 ? 1.0 : 0.0;
}

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

static float4 fetch(constant TexDesc &t, const device uchar *d, uint level, int x, int y, int z) {
    uint w = max(t.width >> level, 1u), h = max(t.height >> level, 1u);
    uint ux, uy, uz = uint(z);
    bool zin = t.kind != 1 || wrap((t.filter >> 6) & 7, z, max(t.depth >> level, 1u), uz);
    if (wrap(t.filter & 7, x, w, ux) && wrap((t.filter >> 3) & 7, y, h, uy) && zin) {
        float4 c = texel(t, d, level, ux, uy, t.kind == 0 ? 0 : uz);
        return float4(pick(c, t.sel[1]), pick(c, t.sel[2]), pick(c, t.sel[3]), pick(c, t.sel[0]));
    }
    uint b = t.border;
    return float4(float((b >> 16) & 0xff), float((b >> 8) & 0xff), float(b & 0xff), float(b >> 24)) / 255.0;
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

static float4 bilinear(constant TexDesc &t, const device uchar *d, uint level, float2 uv, int z) {
    uv -= 0.5;
    float2 f0 = floor(uv);
    float2 f = uv - f0;
    int x0 = int(f0.x), y0 = int(f0.y);
    float4 t00 = fetch(t, d, level, x0, y0, z), t10 = fetch(t, d, level, x0 + 1, y0, z);
    float4 t01 = fetch(t, d, level, x0, y0 + 1, z), t11 = fetch(t, d, level, x0 + 1, y0 + 1, z);
    float4 top = t00 + (t10 - t00) * f.x, bottom = t01 + (t11 - t01) * f.x;
    return top + (bottom - top) * f.y;
}

static float channel(float4 c, uint m) {
    return m <= 3 ? c[m] : m == 5 ? 1.0 : 0.0;
}

// Samples one image of a texture with Metal, as `fetch` and `bilinear` would.
static float4 native_tap(constant TexDesc &t, TexTables tb, uint level, uint face, float2 st, bool linear) {
    texture2d<float> img = tb.images[t.first + level * t.layers + face].t;
    float4 c = img.sample(tb.samplers[t.sampler + (linear ? 1 : 0)].s, st, metal::level(0));
    return float4(channel(c, t.map & 7), channel(c, (t.map >> 3) & 7), channel(c, (t.map >> 6) & 7), channel(c, (t.map >> 9) & 7));
}

static float4 sample_tex(constant TexDesc &t, const device uchar *vram, const device uchar *stage, TexTables tb, float4 coords, uint op) {
    float3 str = tex_coords(t, coords, op == 3);
    float2 st = str.xy;
    float2 size = float2(t.width, t.height);
    float2 dx = dfdx(st) * size, dy = dfdy(st) * size;
    float rho = max(dot(dx, dx), dot(dy, dy));
    float lod = rho > 0.0 ? 0.5 * log2(rho) : 0.0;
    if (op == 4) lod += coords.w;
    if (!t.enabled) return float4(0, 0, 0, 1);
    const device uchar *d = t.stage ? stage : vram;
    bool minify = lod > 0.0;
    uint mip = (t.filter >> 13) & 3;
    uint level = 0;
    if (minify && mip != 0) level = min(uint(round(lod)) + ((t.filter >> 17) & 0xf), t.nlevels - 1);
    bool linear = minify ? ((t.filter >> 11) & 3) == 2 : ((t.filter >> 9) & 3) == 2;
    if (t.native) return native_tap(t, tb, level, t.kind == 2 ? uint(str.z) : 0, st, linear);
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
    if (!linear) return fetch(t, d, level, int(floor(uv.x)), int(floor(uv.y)), z0);
    float4 a = bilinear(t, d, level, uv, z0);
    if (fz == 0.0) return a;
    return a + (bilinear(t, d, level, uv, z0 + 1) - a) * fz;
}

static float presub(uint op, float a, float b) {
    return op == 0 ? 1.0 - 2.0 * a : op == 1 ? b - a : op == 2 ? b + a : 1.0 - a;
}

static float3 presub3(uint op, float3 a, float3 b) {
    return float3(presub(op, a.x, b.x), presub(op, a.y, b.y), presub(op, a.z, b.z));
}

static float omod(float v, uint m) {
    switch (m) {
    case 1: return v * 2.0;
    case 2: return v * 4.0;
    case 3: return v * 8.0;
    case 4: return v / 2.0;
    case 5: return v / 4.0;
    case 6: return v / 8.0;
    default: return v;
    }
}

static bool inside_clip(constant Uniforms &u, int x, int y) {
    if (u.clip_rule == 0xffff) return true;
    uint n = 0;
    for (uint k = 0; k < 4; k++) {
        if (x >= u.clips[4 * k] && x <= u.clips[4 * k + 2] && y >= u.clips[4 * k + 1] && y <= u.clips[4 * k + 3]) n |= 1u << k;
    }
    return (u.clip_rule >> n) & 1;
}

static bool test(uint func, uint v, uint r) {
    switch (func) {
    case 0: return false;
    case 1: return v < r;
    case 2: return v <= r;
    case 3: return v == r;
    case 4: return v >= r;
    case 5: return v > r;
    case 6: return v != r;
    default: return true;
    }
}

static uint stencil_op(uint op, uint s, uint r) {
    switch (op) {
    case 1: return 0;
    case 2: return r;
    case 3: return min(s + 1, 255u);
    case 4: return s == 0 ? 0 : s - 1;
    case 5: return ~s & 0xff;
    case 6: return (s + 1) & 0xff;
    case 7: return (s - 1) & 0xff;
    default: return s;
    }
}

static uint read_px(device uchar *v, uint a, uint bpp) {
    if (bpp == 4) return *(device uint *)(v + a);
    if (bpp == 2) return *(device ushort *)(v + a);
    return v[a];
}

static void write_px(device uchar *v, uint a, uint bpp, uint x) {
    if (bpp == 4) *(device uint *)(v + a) = x;
    else if (bpp == 2) *(device ushort *)(v + a) = ushort(x);
    else v[a] = uchar(x);
}

static bool depth_stencil(constant Uniforms &u, device uchar *vram, uint x, uint y, float z, bool front) {
    if (!u.has_depth || (u.zb_cntl & 3) == 0) return true;
    uint bpp = u.z16 ? 2 : 4;
    uint a = u.depth_base + (y * u.depth_pitch + x) * bpp;
    if (a + bpp > u.vram_size) return true;
    if (u.cb_clear) {
        write_px(vram, a, bpp, u.cb_clear_value);
        return true;
    }
    uint old = read_px(vram, a, bpp);
    float ds = u.depth_scale != 0.0 ? u.depth_scale : 16777215.0;
    uint z24 = uint(clamp(round(z * ds + u.depth_bias), 0.0, 16777215.0));
    uint zval = u.z16 ? z24 >> 8 : z24;
    uint zold = u.z16 ? old & 0xffff : old >> 8;
    bool z_on = (u.zb_cntl & 2) != 0;
    bool z_pass = !z_on || test(u.zs_cntl & 7, zval, zold);
    bool stencil_on = (u.zb_cntl & 1) != 0 && !u.z16;
    uint nv = old;
    bool pass = z_pass;
    if (stencil_on) {
        bool back = (u.zb_cntl & (1 << 4)) != 0 && !front;
        uint func = (u.zs_cntl >> (back ? 15 : 3)) & 7;
        uint fail_op = (u.zs_cntl >> (back ? 18 : 6)) & 7;
        uint zpass_op = (u.zs_cntl >> (back ? 21 : 9)) & 7;
        uint zfail_op = (u.zs_cntl >> (back ? 24 : 12)) & 7;
        uint ref = u.stencil_ref & 0xff, mask = (u.stencil_ref >> 8) & 0xff, wmask = (u.stencil_ref >> 16) & 0xff;
        uint s = old & 0xff;
        bool s_pass = test(func, s & mask, ref & mask);
        uint op = !s_pass ? fail_op : !z_pass ? zfail_op : zpass_op;
        uint ns = stencil_op(op, s, ref);
        nv = (nv & ~wmask) | (ns & wmask);
        pass = s_pass && z_pass;
    }
    if (pass && z_on && (u.zb_cntl & 4) != 0) nv = u.z16 ? zval : (zval << 8) | (nv & 0xff);
    if (nv != old) write_px(vram, a, bpp, nv);
    return pass;
}

static float factor(uint f, uint k, float4 s, float4 d, float4 cc, bool alpha) {
    float sa = s.w, da = d.w;
    switch (f) {
    case 1: case 32: return 0.0;
    case 2: case 33: return 1.0;
    case 3: case 34: return s[k];
    case 4: case 35: return 1.0 - s[k];
    case 5: case 38: case 12: return sa;
    case 6: case 39: case 13: return 1.0 - sa;
    case 7: case 40: return da;
    case 8: case 41: return 1.0 - da;
    case 9: case 36: return d[k];
    case 10: case 37: return 1.0 - d[k];
    case 11: case 42: return alpha ? 1.0 : min(sa, 1.0 - da);
    case 43: return cc[k];
    case 44: return 1.0 - cc[k];
    case 45: return cc.w;
    case 46: return 1.0 - cc.w;
    default: return 1.0;
    }
}

static float combine(uint cntl, uint k, float4 s, float4 d, float4 cc) {
    uint comb = (cntl >> 12) & 7, fs = (cntl >> 16) & 63, fd = (cntl >> 24) & 63;
    if (comb == 4) return min(s[k], d[k]);
    if (comb == 5) return max(s[k], d[k]);
    bool alpha = k == 3;
    float a = s[k] * factor(fs, k, s, d, cc, alpha);
    float dfac = fs == 12 ? 1.0 - s.w : fs == 13 ? s.w : factor(fd, k, s, d, cc, alpha);
    float b = d[k] * dfac;
    float v = (comb == 0 || comb == 1) ? a + b : (comb == 2 || comb == 3) ? a - b : b - a;
    return comb % 2 == 0 ? clamp(v, 0.0, 1.0) : v;
}

static uint widths(uint format, thread uint *w) {
    switch (format) {
    case 3: w[0] = 5; w[1] = 5; w[2] = 5; w[3] = 1; return 4;
    case 4: w[0] = 5; w[1] = 6; w[2] = 5; return 3;
    case 9: w[0] = 8; return 1;
    case 13: w[0] = 8; w[1] = 8; return 2;
    case 15: w[0] = 4; w[1] = 4; w[2] = 4; w[3] = 4; return 4;
    default: w[0] = 8; w[1] = 8; w[2] = 8; w[3] = 8; return 4;
    }
}

static uint bytes_per_pixel(uint format) {
    return format == 9 ? 1 : (format == 3 || format == 4 || format == 13 || format == 15) ? 2 : 4;
}

static uint rop2(uint r, uint s, uint d) {
    uint out = 0;
    if (r & 1) out |= ~s & ~d;
    if (r & 2) out |= ~s & d;
    if (r & 4) out |= s & ~d;
    if (r & 8) out |= s & d;
    return out;
}

static float4 blend_fog(constant Uniforms &u, float4 c, float value) {
    if (!(u.fog & 1)) return c;
    float amount;
    switch ((u.fog >> 1) & 3) {
    case 0: amount = value; break;
    case 1: amount = 1.0 - exp2(-16.0 * value); break;
    case 2: amount = 1.0 - exp2(-16.0 * value * value); break;
    default: amount = u.fog_factor; break;
    }
    amount = clamp(amount, 0.0, 1.0);
    return float4(mix(c.xyz, float3(u.fog_colour[0], u.fog_colour[1], u.fog_colour[2]), amount), c.w);
}

static void backend(constant Uniforms &u, device uchar *vram, device atomic_uint *counter, uint x, uint y, float4 colour, float z, bool front) {
    if (u.alpha_func & (1 << 11)) {
        uint a = uint(round(clamp(colour.w, 0.0, 1.0) * 255.0));
        const uint alpha_to_z[8] = {0, 1, 3, 2, 5, 6, 4, 7};
        if (!test(alpha_to_z[(u.alpha_func >> 8) & 7], a, u.alpha_func & 0xff)) return;
    }
    if (!depth_stencil(u, vram, x, y, z, front)) return;
    atomic_fetch_add_explicit(counter, 1, memory_order_relaxed);
    uint sel[4];
    for (uint k = 0; k < 4; k++) {
        uint s = (u.out_fmt >> (8 + 2 * k)) & 3;
        sel[k] = s == 0 ? 3 : s - 1;
    }
    float4 src = clamp(float4(colour[sel[0]], colour[sel[1]], colour[sel[2]], colour[sel[3]]), 0.0, 1.0);
    float4 cc = float4(float(u.constant_colour & 0xff), float((u.constant_colour >> 8) & 0xff), float((u.constant_colour >> 16) & 0xff), float(u.constant_colour >> 24)) / 255.0;
    for (uint i = 0; i < u.ncb; i++) {
        uint fmt = u.cb[i].format;
        uint bpp = bytes_per_pixel(fmt);
        uint a = u.cb[i].offset + (y * u.cb[i].pitch + x) * bpp;
        if (a + bpp > u.vram_size) continue;
        uint w[4] = {0, 0, 0, 0};
        uint n = widths(fmt, w);
        uint old = read_px(vram, a, bpp);
        float4 dst = float4(0, 0, 0, 1);
        uint shift = 0;
        for (uint k = 0; k < n; k++) {
            dst[k] = float((old >> shift) & ((1u << w[k]) - 1u)) / float((1u << w[k]) - 1u);
            shift += w[k];
        }
        float4 out = src;
        if (u.blend & 1) {
            for (uint k = 0; k < 4; k++) {
                uint cntl = (k == 3 && (u.blend & 2)) ? u.ablend : u.blend;
                out[k] = combine(cntl, k, src, dst, cc);
            }
        }
        uint packed = 0, mask = 0;
        shift = 0;
        for (uint k = 0; k < n; k++) {
            uint mx = (1u << w[k]) - 1u;
            packed |= uint(round(clamp(out[k], 0.0, 1.0) * float(mx))) << shift;
            if (u.channel_mask & (1u << k)) mask |= mx << shift;
            shift += w[k];
        }
        if (u.rop & 4) packed = rop2((u.rop >> 8) & 15, packed, old);
        uint nv = (old & ~mask) | (packed & mask);
        if (nv != old) write_px(vram, a, bpp, nv);
    }
}

// As Raster::offset_depth in raster.rs, whose slopes are per 1/16 pixel.
static float poly_offset(constant Uniforms &u, float z, bool front) {
    if ((u.poly_enable & (front ? 1u : 2u)) == 0) return z;
    float scale = front ? u.poly[0] : u.poly[2], offset = front ? u.poly[1] : u.poly[3];
    float ds = u.depth_scale != 0.0 ? u.depth_scale : 16777215.0;
    float slope = max(abs(dfdx(z)), abs(dfdy(z))) / 16.0;
    return z + (scale * slope * ds + offset) / ds;
}

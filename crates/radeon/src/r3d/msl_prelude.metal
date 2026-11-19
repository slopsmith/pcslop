// The alpha test, depth and stencil, blending, the ROP and colour packing,
// ported from raster.rs so both backends draw the same pixels, reading and
// writing the VRAM buffer. The S_ macros are the draw's back end state,
// which msl.rs bakes in. msl_tex.metal samples textures.

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

[[visible]] extern float4 sample_tex(constant TexDesc &t, const device uchar *images, const device uchar *samplers, float4 coords, uint op);

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
    if (S_CLIP_ALL) return true;
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
    if (!S_HAS_DEPTH || (S_ZB & 3) == 0) return true;
    uint bpp = S_Z16 ? 2 : 4;
    uint a = u.depth_base + (y * u.depth_pitch + x) * bpp;
    if (a + bpp > u.vram_size) return true;
    if (S_CB_CLEAR) {
        write_px(vram, a, bpp, u.cb_clear_value);
        return true;
    }
    uint old = read_px(vram, a, bpp);
    float ds = u.depth_scale != 0.0 ? u.depth_scale : 16777215.0;
    uint z24 = uint(clamp(round(z * ds + u.depth_bias), 0.0, 16777215.0));
    uint zval = S_Z16 ? z24 >> 8 : z24;
    uint zold = S_Z16 ? old & 0xffff : old >> 8;
    bool z_on = (S_ZB & 2) != 0;
    bool z_pass = !z_on || test(S_ZS & 7, zval, zold);
    bool stencil_on = (S_ZB & 1) != 0 && !S_Z16;
    uint nv = old;
    bool pass = z_pass;
    if (stencil_on) {
        bool back = (S_ZB & (1 << 4)) != 0 && !front;
        uint func = (S_ZS >> (back ? 15 : 3)) & 7;
        uint fail_op = (S_ZS >> (back ? 18 : 6)) & 7;
        uint zpass_op = (S_ZS >> (back ? 21 : 9)) & 7;
        uint zfail_op = (S_ZS >> (back ? 24 : 12)) & 7;
        uint ref = u.stencil_ref & 0xff, mask = (u.stencil_ref >> 8) & 0xff, wmask = (u.stencil_ref >> 16) & 0xff;
        uint s = old & 0xff;
        bool s_pass = test(func, ref & mask, s & mask);
        uint op = !s_pass ? fail_op : !z_pass ? zfail_op : zpass_op;
        uint ns = stencil_op(op, s, ref);
        nv = (nv & ~wmask) | (ns & wmask);
        pass = s_pass && z_pass;
    }
    if (pass && z_on && (S_ZB & 4) != 0) nv = S_Z16 ? zval : (zval << 8) | (nv & 0xff);
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
    if (!(S_FOG & 1)) return c;
    float amount;
    switch ((S_FOG >> 1) & 3) {
    case 0: amount = value; break;
    case 1: amount = 1.0 - exp2(-16.0 * value); break;
    case 2: amount = 1.0 - exp2(-16.0 * value * value); break;
    default: amount = u.fog_factor; break;
    }
    amount = clamp(amount, 0.0, 1.0);
    return float4(mix(c.xyz, float3(u.fog_colour[0], u.fog_colour[1], u.fog_colour[2]), amount), c.w);
}

static void backend(constant Uniforms &u, device uchar *vram, device atomic_uint *counter, uint x, uint y, float4 colour, float z, bool front) {
    if (S_ALPHA & (1 << 11)) {
        uint a = uint(round(clamp(colour.w, 0.0, 1.0) * 255.0));
        const uint alpha_to_z[8] = {0, 1, 3, 2, 5, 6, 4, 7};
        if (!test(alpha_to_z[(S_ALPHA >> 8) & 7], a, u.alpha_func & 0xff)) return;
    }
    if (!depth_stencil(u, vram, x, y, z, front)) return;
    atomic_fetch_add_explicit(counter, 1, memory_order_relaxed);
    uint sel[4];
    for (uint k = 0; k < 4; k++) {
        uint s = (S_OUT_FMT >> (8 + 2 * k)) & 3;
        sel[k] = s == 0 ? 3 : s - 1;
    }
    float4 src = clamp(float4(colour[sel[0]], colour[sel[1]], colour[sel[2]], colour[sel[3]]), 0.0, 1.0);
    float4 cc = float4(float(u.constant_colour & 0xff), float((u.constant_colour >> 8) & 0xff), float((u.constant_colour >> 16) & 0xff), float(u.constant_colour >> 24)) / 255.0;
    for (uint i = 0; i < S_NCB; i++) {
        uint fmt = S_FMT(i);
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
        if (S_BLEND & 1) {
            for (uint k = 0; k < 4; k++) {
                uint cntl = (k == 3 && (S_BLEND & 2)) ? S_ABLEND : S_BLEND;
                out[k] = combine(cntl, k, src, dst, cc);
            }
        }
        uint packed = 0, mask = 0;
        shift = 0;
        for (uint k = 0; k < n; k++) {
            uint mx = (1u << w[k]) - 1u;
            packed |= uint(round(clamp(out[k], 0.0, 1.0) * float(mx))) << shift;
            if (S_CMASK & (1u << k)) mask |= mx << shift;
            shift += w[k];
        }
        if (S_ROP & 4) packed = rop2((S_ROP >> 8) & 15, packed, old);
        uint nv = (old & ~mask) | (packed & mask);
        if (nv != old) write_px(vram, a, bpp, nv);
    }
}

// As Raster::offset_depth in raster.rs, whose slopes are per 1/16 pixel.
static float poly_offset(constant Uniforms &u, float z, bool front) {
    if ((S_POLY & (front ? 1u : 2u)) == 0) return z;
    float scale = front ? u.poly[0] : u.poly[2], offset = front ? u.poly[1] : u.poly[3];
    float ds = u.depth_scale != 0.0 ? u.depth_scale : 16777215.0;
    float slope = max(abs(dfdx(z)), abs(dfdy(z))) / 16.0;
    return z + (scale * slope * ds + offset) / ds;
}

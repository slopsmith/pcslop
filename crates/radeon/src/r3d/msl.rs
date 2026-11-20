//! Translates a fragment (US) program into Metal Shading Language: the same
//! instruction semantics as us.rs, unrolled into straight-line code, with
//! textures, constants and the back end state left to a uniform buffer.

use super::raster::Raster;
use super::us::Program;
use super::vsgen::VsDesc;
use std::fmt::Write;

const PRELUDE: &str = concat!(include_str!("msl_common.metal"), include_str!("msl_prelude.metal"));

/// The library holding `sample_tex`, which every fragment shader links.
pub(crate) const TEXTURES: &str = concat!(include_str!("msl_common.metal"), include_str!("msl_tex.metal"));

const CAPTURED_VS: &str = r#"
vertex VOut vs(uint vid [[vertex_id]], const device float4 *vb [[buffer(0)]]) {
    const device float4 *v = vb + vid * @STRIDE@;
    float4 p = v[0];
    float w = (PERSPECTIVE && p.w > 0.0) ? 1.0 / p.w : 1.0;
    VOut o;
    o.position = float4((p.x / 2048.0 - 1.0) * w, (1.0 - p.y / 2048.0) * w, 0.0, w);
    o.z = p.z;
    o.front = v[1].x;
    o.fog = v[1].y;
@COPY@    return o;
}
"#;

/// The vertex outputs: `flat[i]` has a bit for each component of slot `i`
/// that takes the provoking vertex's value, which goes in `vf{i}`.
fn outputs(slots: &[usize], perspective: bool, flat: &[u32]) -> String {
    let interp = if perspective { "center_perspective" } else { "center_no_perspective" };
    let mut varyings = format!("    float fog [[{interp}]];\n");
    for (i, _) in slots.iter().enumerate() {
        let _ = writeln!(varyings, "    float4 v{i} [[{interp}]];");
        if flat.get(i).is_some_and(|&m| m != 0) {
            let _ = writeln!(varyings, "    float4 vf{i} [[flat]];");
        }
    }
    let fields = format!("    float4 position [[position]];\n    float z [[center_no_perspective]];\n    float front [[flat]];\n{varyings}");
    format!(
        "#define PERSPECTIVE {perspective}\nstruct VOut {{\n{fields}}};\nstruct VOutV {{\n{fields}    float clip [[clip_distance]] [8];\n}};\n"
    )
}

/// The back end state a fragment shader is compiled for: the colour
/// buffers' formats, depth and stencil, the alpha test, blending, the ROP,
/// fog and the output swizzle, as the prelude's `S_` macros read them.
/// What the state makes irrelevant is zero, so draws that differ only
/// there share a pipeline; values drivers change often stay in `Uniforms`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Backend {
    ncb: u32,
    formats: [u32; 4],
    has_depth: u32,
    z16: u32,
    zb: u32,
    zs: u32,
    cb_clear: u32,
    blend: u32,
    ablend: u32,
    cmask: u32,
    rop: u32,
    alpha: u32,
    out_fmt: u32,
    fog: u32,
    clip_all: u32,
    poly: u32,
}

impl Backend {
    pub fn new(r: &Raster) -> Self {
        let ncb = r.colour.len().min(4);
        let mut formats = [0; 4];
        for (f, cb) in formats.iter_mut().zip(&r.colour) {
            *f = cb.format;
        }
        let mut b = Backend {
            ncb: ncb as u32,
            formats,
            cmask: r.channel_mask & 0xf,
            out_fmt: r.out_fmt & 0xff00 | (r.out_fmt & 0x1f == 2) as u32,
            clip_all: (r.clip_rule == 0xffff) as u32,
            poly: r.poly_offset.0 & 3,
            ..Default::default()
        };
        if let Some(d) = r.depth
            && r.zb_cntl & 3 != 0
        {
            b.has_depth = 1;
            b.z16 = d.z16 as u32;
            b.zb = r.zb_cntl & 0x17;
            if r.cb_clear.is_some() {
                b.cb_clear = 1;
                b.zb &= 3;
            } else {
                b.zs = r.zs_cntl;
                if b.zb & 1 == 0 || d.z16 {
                    b.zb &= !0x11;
                    b.zs &= 7;
                }
                if b.zb & 2 == 0 {
                    b.zs &= !7;
                }
            }
        }
        const COMBINE: u32 = 7 << 12 | 63 << 16 | 63 << 24;
        if r.blend & 1 != 0 {
            b.blend = r.blend & (3 | COMBINE);
            if r.blend & 2 != 0 {
                b.ablend = r.ablend & COMBINE;
            }
        }
        if r.rop & 4 != 0 {
            b.rop = r.rop & 0xf04;
        }
        if r.alpha_func & (1 << 11) != 0 {
            b.alpha = r.alpha_func & 0xf00;
        }
        if r.fog & 1 != 0 {
            b.fog = r.fog & 7;
        }
        b
    }

    /// Words for a pipeline cache key.
    pub fn key(&self) -> [u32; 19] {
        let [f0, f1, f2, f3] = self.formats;
        [
            self.ncb, f0, f1, f2, f3, self.has_depth, self.z16, self.zb, self.zs, self.cb_clear, self.blend, self.ablend,
            self.cmask, self.rop, self.alpha, self.out_fmt, self.fog, self.clip_all, self.poly,
        ]
    }

    fn defines(&self) -> String {
        let [f0, f1, f2, f3] = self.formats;
        let mut d = format!("#define S_FMT(i) ((i) == 0 ? {f0}u : (i) == 1 ? {f1}u : (i) == 2 ? {f2}u : {f3}u)\n");
        for (name, v) in [
            ("NCB", self.ncb),
            ("HAS_DEPTH", self.has_depth),
            ("Z16", self.z16),
            ("ZB", self.zb),
            ("ZS", self.zs),
            ("CB_CLEAR", self.cb_clear),
            ("BLEND", self.blend),
            ("ABLEND", self.ablend),
            ("CMASK", self.cmask),
            ("ROP", self.rop),
            ("ALPHA", self.alpha),
            ("OUT_FMT", self.out_fmt),
            ("FOG", self.fog),
            ("CLIP_ALL", self.clip_all),
            ("POLY", self.poly),
        ] {
            let _ = writeln!(d, "#define S_{name} {v}u");
        }
        d
    }
}

/// Generates the vertex and fragment functions (`vs` and `fs`) for a draw
/// of triangles the CPU set up, whose rasterizer fills pixel stack
/// registers `slots`.
pub(crate) fn shader(prog: &Program, slots: &[usize], perspective: bool, backend: &Backend) -> String {
    let mut copy = String::new();
    for (i, _) in slots.iter().enumerate() {
        let _ = writeln!(copy, "    o.v{i} = v[{}];", 2 + i);
    }
    let mut src = backend.defines();
    src.push_str(PRELUDE);
    src.push_str(&outputs(slots, perspective, &[]));
    src.push_str(&CAPTURED_VS.replace("@COPY@", &copy).replace("@STRIDE@", &(2 + slots.len()).to_string()));
    src.push_str(&fragment(prog, slots, &[], false));
    src
}

/// Generates `vs` and `fs` for a draw whose vertices the GPU processes, as
/// `vs` describes.
pub(crate) fn shader_gpu(prog: &Program, slots: &[usize], perspective: bool, backend: &Backend, vs: &VsDesc) -> String {
    let mut src = backend.defines();
    src.push_str(PRELUDE);
    let flat = vs.flat(slots);
    src.push_str(&outputs(slots, perspective, &flat));
    src.push_str(&vs.msl(slots, &flat));
    src.push_str(&fragment(prog, slots, &flat, true));
    src
}

/// With `gpu_vertices`, depth comes from the rasterized position, with the
/// polygon offset applied here, and the face from Metal.
fn fragment(prog: &Program, slots: &[usize], flat: &[u32], gpu_vertices: bool) -> String {
    let mut f = String::from(
        "fragment void fs(VOut in [[stage_in]],\n    device uchar *vram [[buffer(0), raster_order_group(0)]],\n    constant Uniforms &u [[buffer(3)]],\n    device atomic_uint *counter [[buffer(4)]],\n    const device uchar *images [[buffer(5)]],\n    const device uchar *samplers [[buffer(6)]],\n    bool front_facing [[front_facing]]) {\n    float4 t[32];\n    for (uint i = 0; i < 32; i++) t[i] = float4(0);\n",
    );
    for (i, s) in slots.iter().enumerate() {
        match flat.get(i).copied().unwrap_or(0) {
            0 => {
                let _ = writeln!(f, "    t[{s}] = in.v{i};");
            }
            m => {
                let pick = [0, 1, 2, 3].map(|c| if m & (1 << c) != 0 { "true" } else { "false" }).join(", ");
                let _ = writeln!(f, "    t[{s}] = select(in.v{i}, in.vf{i}, bool4({pick}));");
            }
        }
    }
    if gpu_vertices {
        f.push_str("    bool front = front_facing;\n    float depth = poly_offset(u, in.position.z, front);\n");
    } else {
        f.push_str("    bool front = in.front != 0.0;\n    float depth = in.z;\n");
    }
    f.push_str("    float4 out = float4(0);\n    bool killed = false;\n");
    for node in &prog.nodes {
        if let Some((start, len)) = node.tex {
            for i in start..start + len {
                let Some(&inst) = prog.tex.get(i) else { break };
                tex_inst(&mut f, inst);
            }
        }
        let (start, len) = node.alu;
        for i in start..start + len {
            let Some(word) = prog.alu.get(i) else { break };
            alu_inst(&mut f, word);
        }
    }
    f.push_str(
        "    int x = int(in.position.x), y = int(in.position.y);\n    if (killed || x < 0 || y < 0 || !inside_clip(u, x, y)) return;\n    backend(u, vram, counter, uint(x), uint(y), blend_fog(u, out, in.fog), depth, front);\n}\n",
    );
    f
}

fn tex_inst(f: &mut String, inst: u32) {
    let src = inst & 31;
    let dst = (inst >> 6) & 31;
    let id = (inst >> 11) & 15;
    let op = (inst >> 15) & 7;
    if op == 2 {
        let _ = writeln!(f, "    killed = killed || any(t[{src}] < 0.0);");
    } else {
        let _ = writeln!(f, "    t[{dst}] = sample_tex(u.tex[{id}], images, samplers, t[{src}], {op});");
    }
}

fn reg(addr: u32) -> String {
    if addr < 32 { format!("t[{addr}]") } else { format!("u.consts[{}]", addr - 32) }
}

fn modifier(e: String, m: u32) -> String {
    match m {
        1 => format!("(-{e})"),
        2 => format!("abs({e})"),
        3 => format!("(-abs({e}))"),
        _ => e,
    }
}

fn rgb_arg(sel: u32) -> String {
    const C: [&str; 3] = ["x", "y", "z"];
    match sel {
        0 | 4 | 8 => format!("r{}.xyz", sel / 4),
        1..=3 | 5..=7 | 9..=11 => format!("float3(r{}.{})", sel / 4, C[(sel % 4 - 1) as usize]),
        12..=14 => format!("float3(a{}.w)", sel - 12),
        15 => "p".into(),
        16..=18 => format!("float3(p.{})", C[(sel - 16) as usize]),
        19 => "float3(pa)".into(),
        20 => "float3(0.0)".into(),
        21 => "float3(1.0)".into(),
        22 => "float3(0.5)".into(),
        23..=25 => format!("r{}.yzx", sel - 23),
        26..=28 => format!("r{}.zxy", sel - 26),
        _ => {
            let i = sel - 29;
            format!("float3(a{i}.w, r{i}.z, r{i}.y)")
        }
    }
}

fn alpha_arg(sel: u32) -> String {
    const C: [&str; 3] = ["x", "y", "z"];
    match sel {
        0..=8 => format!("r{}.{}", sel / 3, C[(sel % 3) as usize]),
        9..=11 => format!("a{}.w", sel - 9),
        12..=14 => format!("p.{}", C[(sel - 12) as usize]),
        15 => "pa".into(),
        17 => "1.0".into(),
        18 => "0.5".into(),
        _ => "0.0".into(),
    }
}

fn alu_inst(f: &mut String, word: &[u32; 4]) {
    let [rgb_addr, rgb_inst, a_addr, a_inst] = *word;
    f.push_str("    {\n");
    for j in 0..3 {
        let _ = writeln!(f, "        float4 r{j} = {};", reg((rgb_addr >> (6 * j)) & 63));
        let _ = writeln!(f, "        float4 a{j} = {};", reg((a_addr >> (6 * j)) & 63));
    }
    let _ = writeln!(f, "        float3 p = presub3({}, r0.xyz, r1.xyz);", (rgb_inst >> 21) & 3);
    let _ = writeln!(f, "        float pa = presub({}, a0.w, a1.w);", (a_inst >> 21) & 3);
    for j in 0..3 {
        let a = (rgb_inst >> (7 * j)) & 0x7f;
        let _ = writeln!(f, "        float3 c{j} = {};", modifier(rgb_arg(a & 31), a >> 5));
        let a = (a_inst >> (7 * j)) & 0x7f;
        let _ = writeln!(f, "        float s{j} = {};", modifier(alpha_arg(a & 31), a >> 5));
    }
    let rgb_op = (rgb_inst >> 23) & 15;
    let dp = match rgb_op {
        2 => "dot(c0, c1) + s0 * s1",
        3 => "c0.x * c1.x + c0.y * c1.y + c2.z",
        _ => "dot(c0, c1)",
    };
    let _ = writeln!(f, "        float dp = {dp};");
    let alpha = match (a_inst >> 23) & 15 {
        0 => "s0 * s1 + s2",
        1 => "dp",
        2 => "min(s0, s1)",
        3 => "max(s0, s1)",
        5 => "s2 > 0.5 ? s0 : s1",
        6 => "s2 >= 0.0 ? s0 : s1",
        7 => "s0 - floor(s0)",
        8 => "exp2(s0)",
        9 => "log2(s0)",
        10 => "1.0 / s0",
        11 => "1.0 / sqrt(abs(s0))",
        _ => "0.0",
    };
    let _ = writeln!(f, "        float ra = omod({alpha}, {});", (a_inst >> 27) & 7);
    if a_inst & (1 << 30) != 0 {
        f.push_str("        ra = clamp(ra, 0.0, 1.0);\n");
    }
    let rgb = match rgb_op {
        0 => "c0 * c1 + c2",
        1..=3 => "float3(dp)",
        4 => "min(c0, c1)",
        5 => "max(c0, c1)",
        7 => "select(c1, c0, c2 > 0.5)",
        8 => "select(c1, c0, c2 >= 0.0)",
        9 => "c0 - floor(c0)",
        10 => "float3(ra)",
        _ => "float3(0.0)",
    };
    let m = (rgb_inst >> 27) & 7;
    let _ = writeln!(f, "        float3 rc = {rgb};");
    let _ = writeln!(f, "        rc = float3(omod(rc.x, {m}), omod(rc.y, {m}), omod(rc.z, {m}));");
    if rgb_inst & (1 << 30) != 0 {
        f.push_str("        rc = clamp(rc, 0.0, 1.0);\n");
    }
    let dst = (rgb_addr >> 18) & 31;
    let wmask = (rgb_addr >> 23) & 7;
    let omask = (rgb_addr >> 26) & 7;
    for (c, name) in ["x", "y", "z"].iter().enumerate() {
        if wmask & (1 << c) != 0 {
            let _ = writeln!(f, "        t[{dst}].{name} = rc.{name};");
        }
        if omask & (1 << c) != 0 {
            let _ = writeln!(f, "        out.{name} = rc.{name};");
        }
    }
    let adst = (a_addr >> 18) & 31;
    if a_addr & (1 << 23) != 0 {
        let _ = writeln!(f, "        t[{adst}].w = ra;");
    }
    if a_addr & (1 << 24) != 0 {
        f.push_str("        out.w = ra;\n");
    }
    if a_addr & (1 << 27) != 0 {
        f.push_str("        depth = ra;\n");
    }
    f.push_str("    }\n");
}

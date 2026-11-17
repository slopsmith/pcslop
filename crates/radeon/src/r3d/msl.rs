//! Translates a fragment (US) program into Metal Shading Language: the same
//! instruction semantics as us.rs, unrolled into straight-line code, with
//! textures, constants and the back end state left to a uniform buffer.

use super::us::Program;
use std::fmt::Write;

const PRELUDE: &str = include_str!("msl_prelude.metal");

/// Generates the vertex and fragment functions (`vs` and `fs`) for a draw
/// whose rasterizer fills pixel stack registers `slots`.
pub(crate) fn shader(prog: &Program, slots: &[usize], perspective: bool) -> String {
    let interp = if perspective { "center_perspective" } else { "center_no_perspective" };
    let mut varyings = format!("    float fog [[{interp}]];\n");
    let mut copy = String::new();
    for (i, _) in slots.iter().enumerate() {
        let _ = writeln!(varyings, "    float4 v{i} [[{interp}]];");
        let _ = writeln!(copy, "    o.v{i} = v[{}];", 2 + i);
    }
    let mut src = PRELUDE
        .replace("@VARYINGS@", &varyings)
        .replace("@COPY@", &copy)
        .replace("@STRIDE@", &(2 + slots.len()).to_string())
        .replace("@PERSPECTIVE@", if perspective { "true" } else { "false" });
    src.push_str(&fragment(prog, slots));
    src
}

fn fragment(prog: &Program, slots: &[usize]) -> String {
    let mut f = String::from(
        "fragment void fs(VOut in [[stage_in]],\n    device uchar *vram [[buffer(0), raster_order_group(0)]],\n    const device uchar *texmem [[buffer(1)]],\n    const device uchar *stage [[buffer(2)]],\n    constant Uniforms &u [[buffer(3)]],\n    device atomic_uint *counter [[buffer(4)]]) {\n    float4 t[32];\n    for (uint i = 0; i < 32; i++) t[i] = float4(0);\n",
    );
    for (i, s) in slots.iter().enumerate() {
        let _ = writeln!(f, "    t[{s}] = in.v{i};");
    }
    f.push_str("    float4 out = float4(0);\n    float depth = in.z;\n    bool killed = false;\n");
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
        "    int x = int(in.position.x), y = int(in.position.y);\n    if (killed || x < 0 || y < 0 || !inside_clip(u, x, y)) return;\n    backend(u, vram, counter, uint(x), uint(y), blend_fog(u, out, in.fog), depth, in.front != 0.0);\n}\n",
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
        let _ = writeln!(f, "    t[{dst}] = sample_tex(u.tex[{id}], texmem, stage, t[{src}], {op});");
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

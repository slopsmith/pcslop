//! Translates a vertex (PVS) program into Metal Shading Language with the
//! semantics of pvs.rs, unrolled into straight-line code. The code runs on
//! locals the caller declares: `inp[32]`, `o[16]`, `tmp[64]`, `alt[64]`,
//! `int4 a0`, and the constants in `consts` (`nconsts` of them).

use super::pvs::{DualMath, Dst, Engine, Inst, Src};
use std::fmt::Write;

/// Helpers the translated code calls.
pub(crate) const HELPERS: &str = r#"
static float4 cst(const device float4 *c, uint n, int i) {
    return uint(i) < n ? c[i] : float4(0);
}

static float4 at_n(thread float4 *a, int n, int i) {
    return i < n ? a[i] : float4(0);
}

static float4 ex2_full(float x) {
    float f = floor(x);
    return float4(precise::exp2(f), x - f, precise::exp2(x), 1.0);
}

static float4 lg2_full(float x) {
    float l = precise::log2(abs(x));
    float f = floor(l);
    return float4(f, precise::divide(abs(x), precise::exp2(f)), l, 1.0);
}

static float4 lit(float4 a, float4 b, float4 c) {
    float ndoth = a.w, ndotl = b.w, e = clamp(c.w, -128.0, 128.0);
    float spec = ndotl > 0.0 ? precise::pow(max(ndoth, 0.0), e) : 0.0;
    return float4(1.0, max(ndotl, 0.0), spec, 1.0);
}
"#;

const COMP: [&str; 4] = ["x", "y", "z", "w"];

fn array(class: u8) -> (&'static str, i32) {
    match class {
        0 => ("tmp", 64),
        1 => ("inp", 32),
        _ => ("alt", 64),
    }
}

fn index(base: i32, rel: Option<u8>) -> String {
    match rel {
        Some(c) => format!("max({base} + a0.{}, 0)", COMP[c as usize]),
        None => base.to_string(),
    }
}

fn register(s: &Src) -> String {
    let i = index(s.base, s.rel);
    if s.class == 2 {
        return format!("cst(consts, nconsts, {i})");
    }
    let (a, n) = array(s.class);
    match s.rel {
        Some(_) => format!("at_n({a}, {n}, {i})"),
        None if s.base < n => format!("{a}[{i}]"),
        None => "float4(0)".into(),
    }
}

fn component(s: &Src, r: &str, c: usize) -> String {
    let mut v = match s.swizzle[c] {
        sw @ 0..=3 => format!("{r}.{}", COMP[sw as usize]),
        5 => "1.0".into(),
        _ => "0.0".into(),
    };
    if s.abs {
        v = format!("abs({v})");
    }
    if s.negate[c] {
        v = format!("(-{v})");
    }
    v
}

fn vector(op: u32) -> Option<&'static str> {
    Some(match op {
        1 => "float4(s0.x * s1.x + s0.y * s1.y + s0.z * s1.z + s0.w * s1.w)",
        2 => "s0 * s1",
        3 => "s0 + s1",
        4 => "s0 * s1 + s2",
        5 => "float4(1.0, s0.y * s1.y, s0.z, s1.w)",
        6 => "s0 - floor(s0)",
        7 => "fmax(s0, s1)",
        8 => "fmin(s0, s1)",
        9 => "select(float4(0.0), float4(1.0), s0 >= s1)",
        10 => "select(float4(0.0), float4(1.0), s0 < s1)",
        11 => "2.0 * s0 * s1 + s2",
        12 => "clamp(s0 * s1, 0.0, 1.0)",
        13 => "floor(s0)",
        14 => "floor(s0 + 0.5)",
        25 => "select(s1, s2, s0 >= 0.0)",
        26 => "select(float4(0.0), float4(1.0), s0 > s1)",
        27 => "select(float4(0.0), float4(1.0), s0 == s1)",
        28 => "select(float4(0.0), float4(1.0), s0 != s1)",
        _ => return None,
    })
}

fn math(op: u32, a: &str, b: &str, c: &str) -> Option<String> {
    Some(match op {
        1 => format!("ex2_full({a}.x)"),
        2 => format!("lg2_full({a}.x)"),
        3 => format!("float4(precise::exp({a}.x))"),
        4 => format!("lit({a}, {b}, {c})"),
        5 | 13 | 14 | 15 => format!("float4(precise::pow({a}.x, {c}.x))"),
        6 | 19 => format!("float4(precise::divide(1.0, {a}.x))"),
        7 => format!("float4({a}.x == 0.0 ? FLT_MAX : precise::divide(1.0, {a}.x))"),
        8 | 20 => format!("float4(precise::rsqrt(abs({a}.x)))"),
        9 => format!("float4({a}.x == 0.0 ? FLT_MAX : precise::rsqrt(abs({a}.x)))"),
        10 => format!("float4({a}.x * {b}.x)"),
        11 => format!("float4(precise::exp2({a}.x))"),
        12 | 18 => format!("float4(precise::log2(abs({a}.x)))"),
        16 => format!("float4(precise::sin({a}.x))"),
        17 => format!("float4(precise::cos({a}.x))"),
        _ => return None,
    })
}

fn write(f: &mut String, d: &Dst, value: &str) {
    let v = if d.sat { format!("clamp({value}, 0.0, 1.0)") } else { value.to_string() };
    let _ = writeln!(f, "        float4 v = {v};");
    if d.kind == 1 {
        for c in (0..4).filter(|c| d.mask & (1 << c) != 0) {
            let _ = writeln!(f, "        a0.{0} = int(floor(v.{0}));", COMP[c]);
        }
        return;
    }
    let (a, n) = match d.kind {
        0 => ("tmp", 64),
        2 | 3 => ("o", 16),
        4 => ("alt", 64),
        5 => ("inp", 32),
        _ => return,
    };
    let target = match d.rel {
        Some(_) => {
            let _ = writeln!(f, "        int di = {};", index(d.base, d.rel));
            let _ = writeln!(f, "        if (di < {n}) {{");
            format!("{a}[di]")
        }
        None if d.base < n => {
            f.push_str("        {\n");
            format!("{a}[{}]", d.base)
        }
        None => return,
    };
    for c in (0..4).filter(|c| d.mask & (1 << c) != 0) {
        let src = if d.kind == 3 { "x" } else { COMP[c] };
        let _ = writeln!(f, "            {target}.{} = v.{src};", COMP[c]);
    }
    f.push_str("        }\n");
}

fn dual(f: &mut String, m: &DualMath) {
    let r = register(&m.src);
    let _ = writeln!(f, "        float4 dr = {r};");
    let (x, y) = (component(&m.src, "dr", 0), component(&m.src, "dr", 1));
    let _ = writeln!(f, "        float4 dx = float4({x}), dy = float4({y});");
    match math(m.op, "dx", "dy", "dy") {
        Some(e) => {
            let e = if m.sat { format!("clamp(({e}).x, 0.0, 1.0)") } else { format!("({e}).x") };
            let _ = writeln!(f, "        float dm = {e};");
        }
        None => f.push_str("        float dm = 0.0;\n"),
    }
}

/// MSL statements running `insts` in order.
pub(crate) fn translate(insts: &[Inst]) -> String {
    let mut f = String::new();
    for inst in insts {
        f.push_str("    {\n");
        for (k, s) in inst.src.iter().enumerate() {
            let _ = writeln!(f, "        float4 r{k} = {};", register(s));
            let comps: Vec<String> = (0..4).map(|c| component(s, &format!("r{k}"), c)).collect();
            let _ = writeln!(f, "        float4 s{k} = float4({});", comps.join(", "));
        }
        let has_dual = inst.dual.is_some_and(|m| math(m.op, "", "", "").is_some());
        if let Some(m) = inst.dual.filter(|_| has_dual) {
            dual(&mut f, &m);
        }
        let value = match inst.engine {
            Engine::Math(op) => math(op, "s0", "s1", "s2"),
            Engine::Macro(op) => Some(if op == 1 { "2.0 * s0 * s1 + s2" } else { "s0 * s1 + s2" }.to_string()),
            Engine::Vector(op) => vector(op).map(str::to_string),
        };
        if let Some(v) = value {
            write(&mut f, &inst.dst, &v);
        }
        if let Some(m) = inst.dual.filter(|_| has_dual) {
            let _ = writeln!(f, "        alt[{}].{} = dm;", m.index, COMP[m.comp]);
        }
        f.push_str("    }\n");
    }
    f
}

/// A compute kernel running the program once per thread on 32 input vectors
/// from `ins` and writing the 16 outputs to `outs`, to test the translation.
#[cfg(test)]
pub(crate) fn kernel(insts: &[Inst]) -> String {
    format!(
        "#include <metal_stdlib>\nusing namespace metal;\n{HELPERS}\nkernel void k(const device float4 *ins [[buffer(0)]], device float4 *outs [[buffer(1)]], const device float4 *consts [[buffer(2)]], constant uint &nconsts [[buffer(3)]], uint id [[thread_position_in_grid]]) {{\n    float4 inp[32], o[16], tmp[64], alt[64];\n    int4 a0 = int4(0);\n    for (int i = 0; i < 32; i++) inp[i] = ins[id * 32 + i];\n    for (int i = 0; i < 16; i++) o[i] = float4(0, 0, 0, 1);\n    for (int i = 0; i < 64; i++) {{ tmp[i] = float4(0); alt[i] = float4(0); }}\n{}    for (int i = 0; i < 16; i++) outs[id * 16 + i] = o[i];\n}}\n",
        translate(insts)
    )
}

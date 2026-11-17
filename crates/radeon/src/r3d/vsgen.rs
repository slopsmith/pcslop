//! The vertex stage on the GPU: a Metal vertex function that fetches a
//! vertex's dwords, splits them as the PSC does, runs the vertex program,
//! lays out its outputs as VAP_OUT_VTX_FMT says, applies the viewport and
//! fills the varyings the RS instructions build, with the semantics of the
//! CPU path in mod.rs and raster.rs. Metal then clips, culls and rasterizes.

use super::pvs::{self, CODE_SLOTS};
use super::pvs_msl;
use std::fmt::Write;

/// Where a draw's vertex dwords come from.
#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) enum Fetch {
    /// From the packet, this many dwords per vertex.
    Immediate(u32),
    /// From arrays of structures, this many dwords per vertex from each.
    Arrays(Vec<u32>),
}

/// The state the vertex function is generated from.
#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct VsDesc {
    /// The program's instruction words in the order they run; empty with
    /// PVS_BYPASS.
    pub code: Vec<u32>,
    pub psc: [u32; 16],
    pub fetch: Fetch,
    pub fmt: [u32; 2],
    pub vte: u32,
    /// Clipping is on (not CLIP_DISABLE) and in Direct3D clip space.
    pub clip: bool,
    pub dx: bool,
    pub fog_select: u32,
    pub colour_control: u32,
    pub clamp_colours: bool,
    pub rs_inst: Vec<u32>,
    pub rs_ip: [u32; 8],
}

/// The vertex function's per-draw values, laid out as `VsUniforms` in MSL.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct VsUniforms {
    /// Per array: 1 if in the arena, else VRAM; the dword vertex 0 starts
    /// at; dwords per vertex; and the end of the readable dwords.
    pub arrays: [[i32; 4]; 16],
    pub imm_base: u32,
    pub nconsts: u32,
    pub pad: [u32; 2],
    pub scale: [f32; 4],
    pub offset: [f32; 4],
    /// GA_OFFSET's x and y, then GA_FOG_SCALE and GA_FOG_OFFSET.
    pub ga_fog: [f32; 4],
    pub solid: [f32; 4],
}

pub(crate) enum ArraySrc {
    /// The array is in VRAM from this dword on.
    Vram(usize),
    /// A copy of the array from the vertex `first` on.
    Copy { data: Vec<u32>, first: u32 },
}

/// A draw for the GPU vertex stage: triangles as vertex indices, and the
/// data the vertex function reads.
pub(crate) struct VertexJob {
    pub indices: Vec<u32>,
    pub immediate: Vec<u32>,
    pub arrays: Vec<(ArraySrc, u32)>,
    pub consts: Vec<u32>,
    pub uniforms: VsUniforms,
    pub cull: u32,
    /// VRAM byte ranges the vertex function reads.
    pub reads: Vec<(usize, usize)>,
}

impl VsDesc {
    pub fn key(&self) -> Vec<u32> {
        let mut k = vec![self.code.len() as u32];
        k.extend(&self.code);
        k.extend(self.psc);
        match &self.fetch {
            Fetch::Immediate(n) => k.extend([0, *n]),
            Fetch::Arrays(a) => {
                k.extend([1, a.len() as u32]);
                k.extend(a);
            }
        }
        k.extend(self.fmt);
        k.extend([self.vte, self.clip as u32 | (self.dx as u32) << 1, self.fog_select, self.colour_control, self.clamp_colours as u32]);
        k.push(self.rs_inst.len() as u32);
        k.extend(&self.rs_inst);
        k.extend(self.rs_ip);
        k
    }

    fn dword(&self, j: usize) -> String {
        match &self.fetch {
            Fetch::Immediate(n) if j < *n as usize => format!("arena[vu.imm_base + vid * {n} + {j}]"),
            Fetch::Immediate(_) => "0u".into(),
            Fetch::Arrays(ns) => {
                let mut at = j;
                for (a, &n) in ns.iter().enumerate() {
                    if at < n as usize {
                        return format!("dw(vram, arena, vu.arrays[{a}], vid, {at})");
                    }
                    at -= n as usize;
                }
                "0u".into()
            }
        }
    }

    /// As `Psc::inputs` in mod.rs.
    fn fetch(&self, f: &mut String) {
        let mut at = 0usize;
        for e in 0..16 {
            let cntl = (self.psc[e / 2] >> (16 * (e % 2))) & 0xffff;
            let ext = (self.psc[8 + e / 2] >> (16 * (e % 2))) & 0xffff;
            let kind = cntl & 15;
            let (s, n) = (cntl & (1 << 14) != 0, cntl & (1 << 15) != 0);
            let fx = |w: &str, shift: u32, bits: u32| format!("fixedv({w} >> {shift}, {bits}, {s}, {n})");
            let d = |i: usize| self.dword(at + i);
            let (raw, used) = match kind {
                0..=3 => {
                    let n = kind as usize + 1;
                    let c: Vec<String> = (0..4).map(|i| if i < n { format!("as_type<float>({})", d(i)) } else if i == 3 { "1.0".into() } else { "0.0".into() }).collect();
                    (format!("float4({})", c.join(", ")), n)
                }
                4 | 5 => {
                    let mut c = [0, 8, 16, 24].map(|sh| fx("w0", sh, 8));
                    if kind == 5 {
                        c.swap(0, 2);
                    }
                    (format!("float4({})", c.join(", ")), 1)
                }
                6 => (format!("float4({}, {}, 0.0, 1.0)", fx("w0", 0, 16), fx("w0", 16, 16)), 1),
                7 => (format!("float4({}, {}, {}, {})", fx("w0", 0, 16), fx("w0", 16, 16), fx("w1", 0, 16), fx("w1", 16, 16)), 2),
                8 => (format!("float4({}, {}, {}, 1.0)", fx("w0", 0, 10), fx("w0", 10, 10), fx("w0", 20, 10)), 1),
                _ => (format!("float4({}, {}, {}, 1.0)", fx("w0", 0, 11), fx("w0", 11, 11), fx("w0", 22, 10)), 1),
            };
            let dst = (cntl >> 8) & 31;
            let mask = (ext >> 12) & 15;
            let _ = writeln!(f, "    {{\n        uint w0 = {}, w1 = {};\n        float4 raw = {raw};", d(0), d(1));
            for c in 0..4 {
                if mask & (1 << c) != 0 {
                    let v = match (ext >> (3 * c)) & 7 {
                        sw @ 0..=3 => format!("raw.{}", ["x", "y", "z", "w"][sw as usize]),
                        5 => "1.0".into(),
                        _ => "0.0".into(),
                    };
                    let _ = writeln!(f, "        inp[{dst}].{} = {v};", ["x", "y", "z", "w"][c]);
                }
            }
            f.push_str("    }\n");
            at += used + ((cntl >> 4) & 15) as usize;
            if cntl & (1 << 13) != 0 {
                break;
            }
        }
    }

    /// For each of `slots`, the components the RS fills from a colour that
    /// GA_COLOR_CONTROL shades flat.
    pub fn flat(&self, slots: &[usize]) -> Vec<u32> {
        let mut regs = [0u32; 32];
        for &inst in &self.rs_inst {
            if (inst >> 3) & 7 != 0 {
                regs[((inst >> 6) & 31) as usize] = 0;
            }
            if (inst >> 14) & 7 != 0 {
                let ip = self.rs_ip[((inst >> 11) & 7) as usize];
                let col = ((ip >> 6) & 7).min(3);
                let rgb = (self.colour_control >> (4 * col)) & 3 == 1;
                let alpha = (self.colour_control >> (4 * col + 2)) & 3 == 1;
                let from = match (ip >> 9) & 15 {
                    0 => 0xf,
                    1 | 2 => 0x7,
                    4 | 8 => 0x8,
                    _ => 0,
                };
                regs[((inst >> 17) & 31) as usize] = from & (if rgb { 0x7 } else { 0 } | if alpha { 0x8 } else { 0 });
            }
        }
        slots.iter().map(|&s| regs[s]).collect()
    }

    /// The vertex function `vs` writing `VOutV` with the varyings for the
    /// pixel stack registers in `slots`, flat ones as `flat` says.
    pub fn msl(&self, slots: &[usize], flat: &[u32]) -> String {
        let mut f = String::from(pvs_msl::HELPERS);
        f.push_str(VS_HELPERS);
        f.push_str("vertex VOutV vs(uint vid [[vertex_id]], const device uint *vram [[buffer(0)]], const device uint *arena [[buffer(1)]],\n    constant VsUniforms &vu [[buffer(2)]], const device float4 *consts [[buffer(3)]]) {\n");
        f.push_str("    uint nconsts = vu.nconsts;\n    float4 inp[32], o[16], tmp[64], alt[64];\n    int4 a0 = int4(0);\n");
        f.push_str("    for (int i = 0; i < 32; i++) inp[i] = float4(0, 0, 0, 1);\n    for (int i = 0; i < 16; i++) o[i] = float4(0, 0, 0, 1);\n    for (int i = 0; i < 64; i++) { tmp[i] = float4(0); alt[i] = float4(0); }\n");
        self.fetch(&mut f);
        if self.code.is_empty() {
            f.push_str("    for (int i = 0; i < 16; i++) o[i] = inp[i];\n");
        } else {
            let insts: Vec<pvs::Inst> = self.code.chunks_exact(4).map(pvs::decode).collect();
            f.push_str(&pvs_msl::translate(&insts));
        }
        self.setup(&mut f, slots, flat);
        f
    }

    /// As `setup_vertex` and `Viewport::apply` in mod.rs, and `attrs`,
    /// `flat_shade` and `rs` in raster.rs.
    fn setup(&self, f: &mut String, slots: &[usize], flat: &[u32]) {
        let [fmt0, fmt1] = self.fmt;
        let mut slot = 1;
        let mut next = || {
            let s = if slot < 16 { format!("o[{slot}]") } else { "float4(0)".into() };
            slot += 1;
            s
        };
        if fmt0 & (1 << 16) != 0 {
            next();
        }
        for c in 0..4 {
            let v = if fmt0 & (2 << c) != 0 { next() } else { "float4(0)".into() };
            let _ = writeln!(f, "    float4 col{c} = {v};");
        }
        let mut counts = [0u32; 8];
        for (t, n) in counts.iter_mut().enumerate() {
            *n = (fmt1 >> (3 * t)) & 7;
            let v = if *n != 0 { next() } else { "float4(0)".into() };
            let _ = writeln!(f, "    float4 tex{t} = {v};");
        }
        let vte = self.vte;
        let bit = |b: u32| vte & (1 << b) != 0;
        f.push_str("    float4 p = o[0];\n    float w = p.w;\n");
        let _ = writeln!(f, "    float rcp = {};", if bit(10) { "w != 0.0 ? 1.0 / w : 0.0" } else { "w" });
        let _ = writeln!(f, "    float x = p.x{0}, y = p.y{0}, z = p.z{1};", if bit(8) { "" } else { " * rcp" }, if bit(9) { "" } else { " * rcp" });
        for (i, c) in ["x", "y", "z"].iter().enumerate() {
            if bit(2 * i as u32) {
                let _ = writeln!(f, "    {c} *= vu.scale.{c};");
            }
            if bit(2 * i as u32 + 1) {
                let _ = writeln!(f, "    {c} += vu.offset.{c};");
            }
        }
        let fog = match self.fog_select {
            k @ 0..=3 => format!("col{k}.w"),
            4 if bit(10) => "w".into(),
            4 => "1.0 / w".into(),
            _ => "z".into(),
        };
        let _ = writeln!(f, "    float fog = {fog} * vu.ga_fog.z + vu.ga_fog.w;");
        f.push_str("    float xw = x + vu.ga_fog.x, yw = y + vu.ga_fog.y;\n    VOutV out;\n");
        let clips = !bit(8) && bit(10);
        if clips {
            let lin = |c: &str, k: usize| {
                let s = if bit(2 * k as u32) { format!("p.{c} * vu.scale.{c}") } else { format!("p.{c}") };
                let o = if bit(2 * k as u32 + 1) { format!("vu.offset.{c}") } else { "0.0".into() };
                (s, o)
            };
            let (sx, ox) = lin("x", 0);
            let (sy, oy) = lin("y", 1);
            let (sz, oz) = lin("z", 2);
            let _ = writeln!(f, "    float lx = ({sx} + ({ox} + vu.ga_fog.x) * w) / 2048.0 - w;");
            let _ = writeln!(f, "    float ly = w - ({sy} + ({oy} + vu.ga_fog.y) * w) / 2048.0;");
            let lz = if bit(9) { "z * w".to_string() } else { format!("{sz} + {oz} * w") };
            let _ = writeln!(f, "    float lz = {lz};");
            f.push_str("    if (w > 0.0) {\n        lx = (round(xw * 16.0) / 16.0 / 2048.0 - 1.0) * w;\n        ly = (1.0 - round(yw * 16.0) / 16.0 / 2048.0) * w;\n    }\n");
            f.push_str("    out.position = float4(lx, ly, lz, w);\n");
            if self.clip {
                let near = if self.dx { "p.z" } else { "p.z + w" };
                let _ = writeln!(f, "    out.clip[0] = {near};\n    out.clip[1] = w - p.z;");
            } else {
                f.push_str("    out.clip[0] = 1.0;\n    out.clip[1] = 1.0;\n");
            }
        } else {
            f.push_str("    float W = (PERSPECTIVE && rcp > 0.0) ? 1.0 / rcp : 1.0;\n");
            f.push_str("    out.position = float4((round(xw * 16.0) / 16.0 / 2048.0 - 1.0) * W, (1.0 - round(yw * 16.0) / 16.0 / 2048.0) * W, z * W, W);\n");
            f.push_str("    out.clip[0] = 1.0;\n    out.clip[1] = 1.0;\n");
        }
        if self.clamp_colours {
            for c in 0..4 {
                let _ = writeln!(f, "    col{c} = clamp(col{c}, 0.0, 1.0);");
            }
        }
        for c in 0..4 {
            for (part, comps) in [(0, &["x", "y", "z"][..]), (1, &["w"][..])] {
                if (self.colour_control >> (4 * c + 2 * part)) & 3 == 0 {
                    for k in comps {
                        let _ = writeln!(f, "    col{c}.{k} = vu.solid.{k};");
                    }
                }
            }
        }
        let mut packed = Vec::new();
        for (t, &n) in counts.iter().enumerate() {
            for c in 0..n.min(4) as usize {
                if packed.len() < 32 {
                    packed.push(format!("tex{t}.{}", ["x", "y", "z", "w"][c]));
                }
            }
        }
        packed.resize(32, "0.0".into());
        packed.push("fog".into());
        let mut regs: Vec<Option<String>> = vec![None; 32];
        for &inst in &self.rs_inst {
            if (inst >> 3) & 7 != 0 {
                let ip = self.rs_ip[(inst & 7) as usize];
                let ptr = (ip & 63) as usize;
                let comps = [0, 1, 2, 3].map(|c| match (ip >> (13 + 3 * c)) & 7 {
                    s @ 0..=3 => packed.get(ptr + s as usize).cloned().unwrap_or("0.0".into()),
                    5 => "1.0".into(),
                    _ => "0.0".into(),
                });
                regs[((inst >> 6) & 31) as usize] = Some(format!("float4({})", comps.join(", ")));
            }
            if (inst >> 14) & 7 != 0 {
                let ip = self.rs_ip[((inst >> 11) & 7) as usize];
                let c = format!("col{}", ((ip >> 6) & 7).min(3));
                let v = match (ip >> 9) & 15 {
                    0 => c,
                    1 => format!("float4({c}.xyz, 0.0)"),
                    2 => format!("float4({c}.xyz, 1.0)"),
                    4 => format!("float4(0.0, 0.0, 0.0, {c}.w)"),
                    5 => "float4(0.0)".into(),
                    6 => "float4(0.0, 0.0, 0.0, 1.0)".into(),
                    8 => format!("float4(1.0, 1.0, 1.0, {c}.w)"),
                    9 => "float4(1.0, 1.0, 1.0, 0.0)".into(),
                    _ => "float4(1.0)".into(),
                };
                regs[((inst >> 17) & 31) as usize] = Some(v);
            }
        }
        for (i, &s) in slots.iter().enumerate() {
            let v = regs.get(s).cloned().flatten().unwrap_or("float4(0.0)".into());
            let _ = writeln!(f, "    out.v{i} = {v};");
            if flat.get(i).is_some_and(|&m| m != 0) {
                let _ = writeln!(f, "    out.vf{i} = out.v{i};");
            }
        }
        f.push_str("    out.z = 0.0;\n    out.front = 0.0;\n    out.fog = fog;\n    return out;\n}\n");
    }
}

const VS_HELPERS: &str = r#"
struct VsUniforms {
    int4 arrays[16];
    uint imm_base, nconsts;
    uint pad[2];
    float4 scale, offset, ga_fog, solid;
};

static uint dw(const device uint *vram, const device uint *arena, int4 a, uint vid, uint k) {
    int at = a.y + int(vid) * a.z + int(k);
    if (at < 0 || at >= a.w) return 0;
    return a.x ? arena[at] : vram[at];
}

static float fixedv(uint raw, uint n, bool sgn, bool norm) {
    if (sgn) {
        float v = float(int(raw << (32 - n)) >> (32 - n));
        return norm ? max(v / float((1u << (n - 1)) - 1u), -1.0) : v;
    }
    float v = float(raw & ((1u << n) - 1u));
    return norm ? v / float((1u << n) - 1u) : v;
}
"#;

/// Triangles of primitive type `prim` over `n` vertices, as positions in
/// the vertex sequence, in the order the CPU path sets them up but rotated
/// so that vertex `provoking` of each comes first, where Metal takes flat
/// values from.
pub(crate) fn triangles(prim: u32, n: usize, provoking: usize) -> Vec<u32> {
    let mut out = Vec::new();
    let mut tri = |a: usize, b: usize, c: usize| {
        let t = [a as u32, b as u32, c as u32];
        out.extend([t[provoking], t[(provoking + 1) % 3], t[(provoking + 2) % 3]]);
    };
    match prim {
        4 => (0..n / 3).for_each(|i| tri(3 * i, 3 * i + 1, 3 * i + 2)),
        5 | 15 => (2..n).for_each(|i| tri(0, i - 1, i)),
        6 => (2..n).for_each(|i| if i % 2 == 0 { tri(i - 2, i - 1, i) } else { tri(i - 1, i - 2, i) }),
        13 => (0..n / 4).for_each(|i| {
            tri(4 * i, 4 * i + 1, 4 * i + 2);
            tri(4 * i, 4 * i + 2, 4 * i + 3);
        }),
        14 => (1..n / 2).for_each(|i| {
            let (a, b, c, d) = (2 * i - 2, 2 * i - 1, 2 * i + 1, 2 * i);
            tri(a, b, c);
            tri(a, c, d);
        }),
        _ => {}
    }
    out
}

/// How many constants `code` can read: one past the highest it names, or
/// all of them if it addresses constants relative to A0.
pub(crate) fn consts_used(code: &[u32]) -> usize {
    let mut n = 0;
    for inst in code.chunks_exact(4).map(pvs::decode) {
        let dual = inst.dual.map(|m| m.src);
        for s in inst.src.iter().chain(dual.iter()).filter(|s| s.class == 2) {
            if s.rel.is_some() {
                return usize::MAX;
            }
            n = n.max(s.base as usize + 1);
        }
    }
    n
}

/// The program's instruction words from `first` to `last`, wrapping as
/// `pvs::Program::decode` does.
pub(crate) fn code(mem: &[u32], first: usize, last: usize) -> Vec<u32> {
    let (first, last) = (first % CODE_SLOTS, last % CODE_SLOTS);
    let mut out = Vec::new();
    let mut i = first;
    loop {
        let Some(w) = mem.get(4 * i..4 * i + 4) else { break };
        out.extend(w);
        if i == last {
            break;
        }
        i = (i + 1) % CODE_SLOTS;
    }
    out
}

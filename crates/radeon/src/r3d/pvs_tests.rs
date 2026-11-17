//! Runs random vertex programs through the interpreter and, translated to
//! MSL, on the GPU, and expects the same outputs.

use super::pvs::{self, CODE_SLOTS, CONST_START, INPUTS, OUTPUTS};
use super::{V4, pvs_msl};
use objc2::rc::autoreleasepool;
use objc2_foundation::NSString;
use objc2_metal::*;
use std::ptr::NonNull;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }

    fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }

    fn value(&mut self) -> f32 {
        (self.below(16_001) as f32 - 8000.0) / 2000.0
    }
}

const VECTOR_OPS: [u32; 20] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 25, 26, 27, 28, 15];
const MATH_OPS: [u32; 21] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 18, 19, 20, 14, 21];

fn source(rng: &mut Rng, relative: bool) -> u32 {
    let class = rng.below(4);
    let base = if class == 2 { rng.below(10) } else { rng.below(8) };
    let mut s = class | base << 5;
    for c in 0..4 {
        s |= rng.below(6) << (13 + 3 * c);
    }
    s |= rng.below(16) << 25;
    if rng.chance(15) {
        s |= 1 << 3;
    }
    if relative && rng.chance(20) {
        s |= 1 << 4 | rng.below(4) << 29;
    }
    s
}

fn instruction(rng: &mut Rng, relative: bool) -> [u32; 4] {
    let mut d0 = match rng.below(10) {
        0..=5 => VECTOR_OPS[rng.below(20) as usize],
        6..=8 => MATH_OPS[rng.below(21) as usize] | 1 << 6,
        _ => rng.below(2) | 1 << 7,
    };
    let kind = [0, 0, 0, 2, 2, 3, 4, 4, 5, 1][rng.below(10) as usize];
    let base = if rng.chance(5) { 100 } else { rng.below(8) };
    d0 |= kind << 8 | base << 13 | (1 + rng.below(15)) << 20;
    if rng.chance(15) {
        d0 |= if d0 & (1 << 6) != 0 { 1 << 25 } else { 1 << 24 };
    }
    if relative && rng.chance(10) {
        d0 |= 1 << 31 | rng.below(4) << 29;
    }
    if d0 & 0xc0 == 0 && rng.chance(25) {
        d0 |= 1 << 28;
        if rng.chance(30) {
            d0 |= 1 << 25;
        }
    }
    [d0, source(rng, relative), source(rng, relative), source(rng, relative)]
}

/// Copies input vector `i` into A0.
fn arl(i: u32) -> [u32; 4] {
    let s = 1 | i << 5 | 1 << 16 | 2 << 19 | 3 << 22;
    [3 | 1 << 8 | 0xf << 20, s, 1 | 4 << 13 | 4 << 16 | 4 << 19 | 4 << 22, 0]
}

fn gpu_run(code: &str, inputs: &[f32], consts: &[f32], vertices: usize) -> Option<Vec<f32>> {
    autoreleasepool(|_| {
        let device = MTLCreateSystemDefaultDevice()?;
        let lib = match device.newLibraryWithSource_options_error(&NSString::from_str(code), None) {
            Ok(l) => l,
            Err(e) => panic!("MSL error: {}\n{code}", e.localizedDescription()),
        };
        let f = lib.newFunctionWithName(&NSString::from_str("k"))?;
        let pipeline = device.newComputePipelineStateWithFunction_error(&f).ok()?;
        let buffer = |data: &[f32]| unsafe {
            device.newBufferWithBytes_length_options(NonNull::new(data.as_ptr().cast_mut().cast())?, data.len().max(1) * 4, MTLResourceOptions::StorageModeShared)
        };
        let ins = buffer(inputs)?;
        let cs = buffer(consts)?;
        let outs = device.newBufferWithLength_options(vertices * OUTPUTS * 16, MTLResourceOptions::StorageModeShared)?;
        let n = (consts.len() / 4) as u32;
        let queue = device.newCommandQueue()?;
        let cmd = queue.commandBuffer()?;
        let enc = cmd.computeCommandEncoder()?;
        enc.setComputePipelineState(&pipeline);
        unsafe {
            enc.setBuffer_offset_atIndex(Some(&ins), 0, 0);
            enc.setBuffer_offset_atIndex(Some(&outs), 0, 1);
            enc.setBuffer_offset_atIndex(Some(&cs), 0, 2);
            enc.setBytes_length_atIndex(NonNull::from(&n).cast(), 4, 3);
        }
        enc.dispatchThreads_threadsPerThreadgroup(MTLSize { width: vertices, height: 1, depth: 1 }, MTLSize { width: 1, height: 1, depth: 1 });
        enc.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        let p = outs.contents().as_ptr().cast::<f32>();
        Some(unsafe { std::slice::from_raw_parts(p, vertices * OUTPUTS * 4) }.to_vec())
    })
}

fn close(a: f32, b: f32) -> bool {
    if a.is_nan() || b.is_nan() {
        return a.is_nan() && b.is_nan();
    }
    if a.abs() > 1e30 || b.abs() > 1e30 {
        return a.abs() > 1e30 && b.abs() > 1e30 && a.signum() == b.signum();
    }
    (a - b).abs() <= 2e-4 * a.abs().max(b.abs()).max(1.0)
}

/// How many output components differ between the interpreter and the GPU
/// for `code` placed from slot `first`, over `vertices` random inputs.
fn compare(code: &[[u32; 4]], first: usize, consts: &[V4], rng: &mut Rng, vertices: usize) -> usize {
    let mut mem = vec![0u32; 2048 * 4];
    for (k, inst) in code.iter().enumerate() {
        let slot = (first + k) % CODE_SLOTS;
        mem[4 * slot..4 * slot + 4].copy_from_slice(inst);
    }
    for (k, c) in consts.iter().enumerate() {
        for i in 0..4 {
            mem[(CONST_START + k) * 4 + i] = c[i].to_bits();
        }
    }
    let program = pvs::Program { mem: &mem, first, last: (first + code.len() - 1) % CODE_SLOTS, const_base: 0 };
    let decoded = program.decode();
    let inputs: Vec<[V4; INPUTS]> = (0..vertices).map(|_| std::array::from_fn(|_| [rng.value(), rng.value(), rng.value(), rng.value()])).collect();
    let flat: Vec<f32> = inputs.iter().flatten().flatten().copied().collect();
    let cflat: Vec<f32> = consts.iter().flatten().copied().collect();
    let Some(gpu) = gpu_run(&pvs_msl::kernel(&decoded.insts), &flat, &cflat, vertices) else {
        eprintln!("no Metal device; skipping");
        return 0;
    };
    let mut bad = 0;
    for (v, inp) in inputs.iter().enumerate() {
        let cpu = decoded.run(inp);
        for (o, out) in cpu.iter().enumerate() {
            for c in 0..4 {
                let g = gpu[(v * OUTPUTS + o) * 4 + c];
                if !close(out[c], g) {
                    bad += 1;
                    if bad <= 4 {
                        eprintln!("vertex {v} output {o}.{c}: cpu {} gpu {g}", out[c]);
                    }
                }
            }
        }
    }
    bad
}

fn random_consts(rng: &mut Rng) -> Vec<V4> {
    (0..12).map(|_| [rng.value(), rng.value(), rng.value(), rng.value()]).collect()
}

#[test]
fn every_opcode_alone_matches_the_interpreter() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let consts = random_consts(&mut rng);
    for engine in 0..3usize {
        let ops: Vec<u32> = match engine {
            0 => VECTOR_OPS.to_vec(),
            1 => MATH_OPS.to_vec(),
            _ => vec![0, 1],
        };
        for op in ops {
            let flags = [0, 1 << 6, 1 << 7][engine];
            let all = |class: u32, i: u32| class | i << 5 | 1 << 16 | 2 << 19 | 3 << 22;
            let inst = [op | flags | 2 << 8 | 0xf << 20, all(1, 0), all(1, 1), all(2, 2)];
            assert_eq!(compare(&[inst], 0, &consts, &mut rng, 256), 0, "engine {engine} op {op}");
        }
    }
}

#[test]
fn random_programs_match_the_interpreter() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut failures = Vec::new();
    for n in 0..300 {
        let consts = random_consts(&mut rng);
        let mut code = vec![arl(rng.below(4))];
        for _ in 0..1 + rng.below(8) {
            code.push(instruction(&mut rng, true));
        }
        let first = if n % 7 == 0 { CODE_SLOTS - 1 - rng.below(3) as usize } else { rng.below(200) as usize };
        if compare(&code, first, &consts, &mut rng, 16) > 0 {
            failures.push((n, code));
        }
    }
    assert!(failures.is_empty(), "{} programs differ, first {:x?}", failures.len(), failures.first());
}

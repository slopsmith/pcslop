//! CPUID for a single-core Pentium 4 "Northwood" (family 15, model 2, stepping 7).

pub const SIGNATURE: u32 = 0x0000_0f27;

pub mod edx {
    pub const FPU: u32 = 1 << 0;
    pub const VME: u32 = 1 << 1;
    pub const DE: u32 = 1 << 2;
    pub const PSE: u32 = 1 << 3;
    pub const TSC: u32 = 1 << 4;
    pub const MSR: u32 = 1 << 5;
    pub const CX8: u32 = 1 << 8;
    pub const APIC: u32 = 1 << 9;
    pub const SEP: u32 = 1 << 11;
    pub const MTRR: u32 = 1 << 12;
    pub const PGE: u32 = 1 << 13;
    pub const CMOV: u32 = 1 << 15;
    pub const CLFSH: u32 = 1 << 19;
    pub const MMX: u32 = 1 << 23;
    pub const FXSR: u32 = 1 << 24;
    pub const SSE: u32 = 1 << 25;
    pub const SSE2: u32 = 1 << 26;
}

pub const FEATURES_EDX: u32 = edx::FPU
    | edx::VME
    | edx::DE
    | edx::PSE
    | edx::TSC
    | edx::MSR
    | edx::CX8
    | edx::SEP
    | edx::PGE
    | edx::CMOV
    | edx::CLFSH
    | edx::MMX
    | edx::FXSR
    | edx::SSE
    | edx::SSE2;

const BRAND: &[u8; 48] = b"86slopper Virtual CPU\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

fn brand_regs(part: usize) -> [u32; 4] {
    let mut out = [0; 4];
    for (i, word) in out.iter_mut().enumerate() {
        let at = part * 16 + i * 4;
        *word = u32::from_le_bytes(BRAND[at..at + 4].try_into().unwrap());
    }
    out
}

/// Returns `[eax, ebx, ecx, edx]` for the given leaf.
pub fn cpuid(leaf: u32, _subleaf: u32) -> [u32; 4] {
    match leaf {
        0 => [2, u32::from_le_bytes(*b"Genu"), u32::from_le_bytes(*b"ntel"), u32::from_le_bytes(*b"ineI")],
        1 => [SIGNATURE, 8 << 8 | 1 << 16, 0, FEATURES_EDX],
        2 => [0x665b_5001, 0, 0, 0x007a_7040],
        0x8000_0000 => [0x8000_0004, 0, 0, 0],
        0x8000_0002..=0x8000_0004 => brand_regs((leaf - 0x8000_0002) as usize),
        0x8000_0001 => [0; 4],
        0x4000_0000..=0x7fff_ffff => [0; 4],
        _ if leaf < 0x8000_0000 => cpuid(2, 0),
        _ => [0; 4],
    }
}

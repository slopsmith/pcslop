use super::*;
use x86::cpu::{Segment, attr, cr0};

const RAM: usize = 4 << 20;

/// Hypervisor.framework allows one VM per process.
static VM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct FlatBus {
    ram: GuestMemory,
    gart: Option<Gart>,
}

/// A graphics aperture at `APERTURE` translated through a GATT in RAM, as on
/// the 440BX with its GTLB on. An OUT to `GTLB_FLUSH` flushes the GTLB.
struct Gart {
    gatt: u32,
    epoch: u64,
}

const APERTURE: u32 = 0xe000_0000;
const GTLB_FLUSH: u16 = 0xcfc;

impl FlatBus {
    fn resolve(&self, addr: u32) -> Option<u32> {
        let Some(g) = &self.gart else { return Some(addr) };
        let off = addr.wrapping_sub(APERTURE);
        if off >= 16 << 20 {
            return Some(addr);
        }
        let at = (g.gatt + (off >> 12) * 4) as usize;
        let entry = u32::from_le_bytes(self.ram.as_slice()[at..at + 4].try_into().unwrap());
        (entry & 1 != 0).then_some((entry & !0xfff) | (off & 0xfff))
    }
}

impl Bus for FlatBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.resolve(addr).and_then(|a| self.ram.as_slice().get(a as usize).copied()).unwrap_or(0xff)
    }

    fn write8(&mut self, addr: u32, value: u8) {
        if let Some(b) = self.resolve(addr).and_then(|a| self.ram.as_mut_slice().get_mut(a as usize)) {
            *b = value;
        }
    }

    fn io_read(&mut self, _port: u16, _size: u8) -> u32 {
        0xffff_ffff
    }

    fn io_write(&mut self, port: u16, _size: u8, _value: u32) {
        if port == GTLB_FLUSH
            && let Some(g) = &mut self.gart
        {
            g.epoch += 1;
        }
    }

    fn aperture_page(&mut self, addr: u32) -> Option<u32> {
        let page = self.resolve(addr & !0xfff).filter(|_| addr.wrapping_sub(APERTURE) < 16 << 20)?;
        ((page as usize) < RAM).then_some(page)
    }

    fn aperture_epoch(&self) -> u64 {
        self.gart.as_ref().map_or(0, |g| g.epoch)
    }
}

fn flat_cpu(eip: u32, esp: u32) -> Cpu {
    let mut cpu = Cpu::new();
    cpu.cr0 = cr0::PE | cr0::ET;
    let data = Segment {
        selector: 0x10,
        attr: attr::P | attr::S | attr::RW | attr::ACCESSED | attr::DB | attr::G,
        base: 0,
        limit: 0xffff_ffff,
    };
    cpu.segs = [data; 6];
    cpu.segs[CS] = Segment { selector: 0x08, attr: data.attr | attr::CODE, ..data };
    cpu.eip = eip;
    cpu.regs[4] = esp;
    cpu
}

fn bus_with(code_at: u32, code: &[u8]) -> FlatBus {
    let mut bus = FlatBus { ram: GuestMemory::new(RAM).unwrap(), gart: None };
    bus.ram.as_mut_slice()[code_at as usize..code_at as usize + code.len()].copy_from_slice(code);
    bus
}

fn run_interp(cpu: &mut Cpu, bus: &mut FlatBus) {
    for _ in 0..100_000 {
        if interp::step(cpu, bus) != interp::Exit::Continue || cpu.halted {
            return;
        }
    }
    panic!("interpreter did not halt");
}

thread_local! {
    /// Set by the tier-2 variants of tests: every block goes through tier 2 first.
    static TIER2: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn in_tier2(test: fn()) {
    TIER2.with(|t| t.set(true));
    test();
}

#[test]
fn random_programs_match_the_interpreter_in_tier_2() {
    in_tier2(random_programs_match_the_interpreter);
}

#[test]
fn random_programs_with_forward_branches_match_the_interpreter_in_tier_2() {
    in_tier2(random_programs_with_forward_branches_match_the_interpreter);
}

#[test]
fn random_programs_with_paging_match_the_interpreter_in_tier_2() {
    in_tier2(random_programs_match_the_interpreter_with_paging);
}

#[test]
fn displacements_any_access_width_cannot_encode_match_the_interpreter_in_tier_2() {
    fn test() {
        let code = [
            0x8b, 0x83, 0x1d, 0x01, 0, 0, // mov eax, [ebx+0x11d]
            0x66, 0x8b, 0x8b, 0x01, 0x01, 0, 0, // mov cx, [ebx+0x101]
            0x89, 0x83, 0x3f, 0x01, 0, 0, // mov [ebx+0x13f], eax
            0x8b, 0x53, 0xfd, // mov edx, [ebx-3]
            0xf4, // hlt
        ];
        compare(&code, |c| c.regs[3] = 0x3000);
    }
    in_tier2(test);
}

#[test]
fn mixed_code_matches_the_interpreter_in_tier_2() {
    in_tier2(random_mmx_programs_match_the_interpreter);
    in_tier2(random_sse_programs_match_the_interpreter);
    in_tier2(sysenter_sysexit_and_fs_loads_match_the_interpreter);
    in_tier2(loop_with_memory_and_stack);
}

fn run_jit(cpu: &mut Cpu, bus: &mut FlatBus) -> Stats {
    run_jit_with(cpu, bus, |_| {}, |_| {})
}

fn run_jit_with(cpu: &mut Cpu, bus: &mut FlatBus, before: impl FnOnce(&mut Jit), after: impl FnOnce(&mut Jit)) -> Stats {
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus.ram.as_ptr(), RAM).unwrap() };
    jit.vm_fill = true;
    jit.tier2_eager |= TIER2.with(|t| t.get());
    before(&mut jit);
    let kicker = jit.kicker();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let kicks = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(1));
            kicker.kick();
        }
    });
    let _join = scopeguard(move || {
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = kicks.join();
    });
    for _ in 0..1000 {
        if !supported(cpu) || cpu.halted {
            break;
        }
        match jit.run(cpu, bus).unwrap() {
            RunExit::Interp(_) if cpu.halted => break,
            RunExit::Interp(e) if std::env::var_os("DBG").is_some() => eprintln!("exit {e:?} eip {:#x} cr2 {:#x} esp {:#x} cs {:#x}", cpu.eip, cpu.cr2, cpu.regs[4], cpu.segs[1].selector),
            _ => {}
        }
    }
    assert!(cpu.halted, "JIT run did not reach HLT; eip {:#x} {:?}", cpu.eip, jit.stats);
    after(&mut jit);
    jit.stats
}

/// Runs `f` when dropped, also while unwinding.
fn scopeguard(f: impl FnOnce()) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    Guard(Some(f))
}

const ARITH_MASK: u32 = x86::cpu::flags::ARITH | x86::cpu::flags::IF;

fn compare(code: &[u8], setup: impl Fn(&mut Cpu)) -> Stats {
    let mut a = flat_cpu(0x1000, 0x8000);
    setup(&mut a);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, code);
    let mut bus_b = bus_with(0x1000, code);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs, "registers");
    assert_eq!(a.eip, b.eip);
    assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "flags {:#x} vs {:#x}", a.eflags, b.eflags);
    assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "memory differs");
    stats
}

#[test]
fn execution_counters_count_block_entries_and_exits() {
    let code = [
        0xb9, 0x0a, 0, 0, 0, // mov ecx, 10
        0x49, // dec ecx
        0x75, 0xfd, // jnz 0x1005
        0xf4, // hlt
    ];
    let mut cpu = flat_cpu(0x1000, 0x8000);
    let mut bus = bus_with(0x1000, &code);
    let mut counts = Vec::new();
    run_jit_with(&mut cpu, &mut bus, |j| j.counting = true, |j| counts = j.block_counts());
    assert_eq!(cpu.regs[1], 0);
    let first = counts.iter().find(|c| c.0 == 0x1000).expect("the first block was not counted");
    let looped = counts.iter().find(|c| c.0 == 0x1005).expect("the loop block was not counted");
    assert_eq!(first.1, 1, "block entries are miscounted, so profiles rank blocks wrongly: {counts:x?}");
    let taken = |c: &(u32, u64, Vec<(u32, u64)>)| c.2.iter().find(|e| e.0 == 0x1005).map(|e| e.1);
    assert_eq!(taken(first), Some(1), "a side exit is miscounted, so branch profiles lie: {counts:x?}");
    assert_eq!(looped.1, 9, "loop entries are miscounted, so profiles rank blocks wrongly: {counts:x?}");
    assert_eq!(taken(looped), Some(8), "a back edge is miscounted, so branch profiles lie: {counts:x?}");
}

#[test]
fn xlat_looks_up_al_in_the_table_at_ebx() {
    let code = [
        0xbb, 0x00, 0x30, 0, 0, // mov ebx, 0x3000
        0xb0, 0x05, // mov al, 5
        0xd7, // xlat
        0x88, 0xc1, // mov cl, al
        0xb8, 0xff, 0x12, 0x34, 0x56, // mov eax, 0x563412ff
        0xd7, // xlat: AL 0xff, upper bits kept
        0xf4, // hlt
    ];
    let stats = compare(&code, |_| {});
    assert_eq!(stats.interp_exits, 1, "XLAT left translated code: {stats:?}");
}

#[test]
fn fprem_sets_the_quotient_bits_and_remainder_sign() {
    let code = [
        0xdd, 0x05, 0x00, 0x30, 0, 0, // fld qword [0x3000]: 2
        0xdd, 0x05, 0x08, 0x30, 0, 0, // fld qword [0x3008]: 13, or -4
        0xd9, 0xf8, // fprem
        0xdf, 0xe0, // fnstsw ax
        0xdd, 0x1d, 0x10, 0x30, 0, 0, // fstp qword [0x3010]
        0xdd, 0xd8, // fstp st0
        0xf4, // hlt
    ];
    for x in [13.0f64, -4.0, 7.5, -0.75] {
        let mut a = flat_cpu(0x1000, 0x8000);
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        for bus in [&mut bus_a, &mut bus_b] {
            let m = bus.ram.as_mut_slice();
            m[0x3000..0x3008].copy_from_slice(&2.0f64.to_le_bytes());
            m[0x3008..0x3010].copy_from_slice(&x.to_le_bytes());
        }
        run_interp(&mut a, &mut bus_a);
        let stats = run_jit(&mut b, &mut bus_b);
        assert_eq!(a.regs[0] & 0xffff, b.regs[0] & 0xffff, "FPREM of {x} reports a different quotient");
        assert_eq!(bus_a.ram.as_slice()[0x3010..0x3018], bus_b.ram.as_slice()[0x3010..0x3018], "FPREM of {x} leaves a different remainder");
        assert!(stats.interp_exits <= 1, "FPREM left translated code: {stats:?}");
    }
}

#[test]
fn loop_with_memory_and_stack() {
    let code = [
        0xb8, 0x05, 0, 0, 0, // mov eax, 5
        0xbb, 0x07, 0, 0, 0, // mov ebx, 7
        0x01, 0xd8, // add eax, ebx
        0xb9, 0x0a, 0, 0, 0, // mov ecx, 10
        0x01, 0xca, // loop: add edx, ecx
        0x49, // dec ecx
        0x75, 0xfb, // jnz loop
        0x50, // push eax
        0x5e, // pop esi
        0x89, 0x35, 0x00, 0x20, 0, 0, // mov [0x2000], esi
        0x8b, 0x3d, 0x00, 0x20, 0, 0, // mov edi, [0x2000]
        0x83, 0xff, 0x0c, // cmp edi, 12
        0x0f, 0x94, 0xc0, // sete al
        0xf4, // hlt
    ];
    let stats = compare(&code, |_| {});
    assert!(stats.blocks >= 2);
    assert_eq!(stats.interp_exits, 1);
}

#[test]
fn flags_from_8bit_and_carry_chains() {
    let code = [
        0xb0, 0xf0, // mov al, 0xf0
        0x04, 0x20, // add al, 0x20
        0x0f, 0x92, 0xc3, // setb bl
        0x80, 0xfc, 0x01, // cmp ah, 1
        0x0f, 0x97, 0xc7, // seta bh
        0xb9, 0xff, 0xff, 0xff, 0xff, // mov ecx, -1
        0x83, 0xc1, 0x01, // add ecx, 1
        0x11, 0xca, // adc edx, ecx
        0x19, 0xf6, // sbb esi, esi
        0x41, // inc ecx (keeps CF)
        0xd1, 0xe7, // shl edi, 1
        0xc1, 0xef, 0x03, // shr edi, 3
        0x0f, 0xaf, 0xc1, // imul eax, ecx
        0x84, 0xc0, // test al, al
        0xf4,
    ];
    compare(&code, |c| {
        c.regs[7] = 0x8000_0001;
        c.regs[2] = 0x7fff_ffff;
    });
}

#[test]
fn call_and_return() {
    let code = [
        0xe8, 0x06, 0, 0, 0, // call +6
        0x89, 0xc3, // mov ebx, eax
        0xf4, // hlt
        0x90, 0x90, 0x90, // padding
        0xb8, 0x2a, 0, 0, 0, // mov eax, 42
        0xc3, // ret
    ];
    compare(&code, |_| {});
}

#[test]
fn returns_deeper_than_the_prediction_ring_or_to_a_changed_address() {
    let mut code = vec![
        0xb9, 100, 0, 0, 0, // mov ecx, 100
        0xe8, 0x16, 0, 0, 0, // call f
        0x89, 0xc3, // mov ebx, eax
        0xe8, 0x2f, 0, 0, 0, // call g
        0x46, // inc esi, skipped by g
        0xf4, // hlt
    ];
    code.resize(0x20, 0x90);
    code.extend([
        0x40, // f: inc eax
        0x49, // dec ecx
        0x74, 0x05, // jz +5
        0xe8, 0xf7, 0xff, 0xff, 0xff, // call f
        0x01, 0xc2, // add edx, eax
        0xc3, // ret
    ]);
    code.resize(0x40, 0x90);
    code.extend([
        0x83, 0x04, 0x24, 0x01, // g: add dword [esp], 1
        0xc3, // ret
    ]);
    compare(&code, |_| {});
}

#[test]
fn a_return_after_its_caller_was_rewritten_runs_the_new_code() {
    let mut code = vec![
        0xe8, 0xfb, 0x0f, 0, 0, // call 0x2000
        0xb8, 0x01, 0, 0, 0, // mov eax, 1, which the callee turns into mov eax, 2
        0xf4, // hlt
    ];
    code.resize(0x1000, 0x90);
    code.extend([
        0xc6, 0x05, 0x06, 0x10, 0, 0, 0x02, // mov byte [0x1006], 2
        0xc3, // ret
    ]);
    compare(&code, |_| {});
}

#[test]
fn x87_loops_keep_values_in_registers_and_check_the_stack_on_entry() {
    let mut code = vec![
        0xd9, 0xe8, // fld1
        0xd9, 0xee, // fldz
        0xb9, 50, 0, 0, 0, // mov ecx, 50
        0xe8, 0xf7, 0, 0, 0, // call 0x1105
        0xd9, 0x1d, 0x00, 0x30, 0, 0, // fstp dword [0x3000]
        0xd9, 0x1d, 0x04, 0x30, 0, 0, // fstp dword [0x3004]
        0xd9, 0xee, // fldz, leaving ST(1) empty for the loop
        0xb9, 3, 0, 0, 0, // mov ecx, 3
        0xe8, 0xdf, 0, 0, 0, // call 0x1105
        0xd9, 0x1d, 0x08, 0x30, 0, 0, // fstp dword [0x3008]
        0xf4, // hlt
    ];
    code.resize(0x105, 0x90);
    code.extend([
        0xd8, 0xc1, // fadd st0, st1
        0xd9, 0xc0, // fld st0
        0xd8, 0xc8, // fmul st0, st0
        0xdd, 0xd8, // fstp st0
        0x49, // dec ecx
        0x75, 0xf5, // jnz 0x1105
        0xc3, // ret
    ]);
    let stats = compare(&code, |_| {});
    assert!(stats.x87_loops >= 1, "the loop flushed its x87 registers every iteration: {stats:?}");
}

#[test]
fn calls_and_returns_with_x87_values_in_registers() {
    let mut code = vec![
        0xb8, 0x20, 0x10, 0, 0, // mov eax, 0x1020
        0xd9, 0xe8, // fld1
        0xff, 0xd0, // call eax
        0xd9, 0x1d, 0x00, 0x30, 0, 0, // fstp dword [0x3000]
        0xd9, 0x1d, 0x04, 0x30, 0, 0, // fstp dword [0x3004]
        0xf4, // hlt
    ];
    code.resize(0x20, 0x90);
    code.extend([
        0xd9, 0xeb, // fldpi
        0xc3, // ret
    ]);
    compare(&code, |_| {});
}

#[test]
fn folded_addresses_that_wrap_around_4_gib_match_the_interpreter() {
    let code = [
        0xbb, 0xf0, 0xff, 0xff, 0xff, // mov ebx, 0xfffffff0
        0xbe, 0x10, 0x30, 0, 0, // mov esi, 0x3010
        0x8b, 0x83, 0x10, 0x30, 0, 0, // mov eax, [ebx+0x3010]: 0x3000
        0x01, 0x83, 0x14, 0x30, 0, 0, // add [ebx+0x3014], eax
        0x8b, 0x0c, 0x33, // mov ecx, [ebx+esi]: 0x3000
        0x89, 0x4b, 0x7f, // mov [ebx+0x7f], ecx: 0x6f
        0xb9, 0xfc, 0xff, 0xff, 0xff, // mov ecx, -4
        0x8b, 0x14, 0x8e, // mov edx, [esi+ecx*4]: an index counting up to 0
        0xbf, 0x10, 0x30, 0, 0, // mov edi, 0x3010
        0x8b, 0x57, 0xf0, // mov edx, [edi-0x10]: no wrap
        0x8d, 0x47, 0x00, // lea eax, [edi]
        0x0f, 0xc1, 0x40, 0x04, // xadd [eax+4], eax: the store uses the old EAX
        0x87, 0x47, 0x08, // xchg [edi+8], eax
        0xf4, // hlt
    ];
    let stats = compare(&code, |_| {});
    assert!(stats.wrapped_accesses >= 1, "the wrapping accesses never took the folded path: {stats:?}");
}

#[test]
fn an_instruction_across_a_page_does_not_reuse_the_folded_operand_before_it() {
    let mut code = vec![0x90; 0xff9];
    code.extend([
        0x8b, 0x44, 0x24, 0x0c, // mov eax, [esp+0xc]
        0x68, 0xc0, 0x99, 0x8b, 0x08, // push 0x088b99c0, ending on the next page
        0x5b, // pop ebx
        0xf4, // hlt
    ]);
    compare(&code, |_| {});
}

#[test]
fn bulk_rep_movs_and_stos_match_the_interpreter_with_overlaps() {
    let ops: [&[u8]; 6] = [&[0xf3, 0xa4], &[0xf3, 0x66, 0xa5], &[0xf3, 0xa5], &[0xf3, 0xaa], &[0xf3, 0x66, 0xab], &[0xf3, 0xab]];
    for (op, df) in ops.iter().flat_map(|op| [(op, false), (op, true)]) {
        for count in [0u32, 1, 3, 4, 15, 16, 17, 33, 100] {
            for gap in [-40i32, -17, -16, -15, -8, -1, 1, 4, 15, 16, 17, 64, 400] {
                let mut code = op.to_vec();
                code.push(0xf4);
                let src = 0x3000u32;
                let dst = (src as i32 + gap) as u32;
                let mut a = flat_cpu(0x1000, 0x8000);
                (a.regs[6], a.regs[7], a.regs[1], a.regs[0]) = (src, dst, count, 0x1234_56a5);
                if df {
                    a.eflags |= x86::cpu::flags::DF;
                }
                let mut b = a.clone();
                let mut bus_a = bus_with(0x1000, &code);
                let mut bus_b = bus_with(0x1000, &code);
                for (i, byte) in bus_a.ram.as_mut_slice()[0x2c00..0x3800].iter_mut().enumerate() {
                    *byte = (i * 7 + 3) as u8;
                }
                bus_b.ram.as_mut_slice()[0x2c00..0x3800].copy_from_slice(&bus_a.ram.as_slice()[0x2c00..0x3800]);
                run_interp(&mut a, &mut bus_a);
                run_jit(&mut b, &mut bus_b);
                assert_eq!(a.regs, b.regs, "{op:02x?} df {df} count {count} gap {gap}: registers");
                assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "{op:02x?} df {df} count {count} gap {gap}: memory");
            }
        }
    }
}

#[test]
fn lazy_fpu_switches_of_cr0_ts_stay_translated() {
    let code = [
        0x0f, 0x20, 0xc0, // mov eax, cr0
        0x83, 0xc8, 0x0a, // or eax, 0xa: TS and MP, as XP's context switch sets them
        0x0f, 0x22, 0xc0, // mov cr0, eax
        0x0f, 0x20, 0xc3, // mov ebx, cr0
        0x0f, 0x06, // clts
        0x0f, 0x20, 0xc1, // mov ecx, cr0
        0xd9, 0xe8, // fld1
        0xd9, 0x1d, 0x00, 0x30, 0, 0, // fstp dword [0x3000]
        0xf4, // hlt
    ];
    let stats = compare(&code, |_| {});
    assert!(stats.interp_exits <= 1, "TS changes or the x87 code after CLTS left translated code: {stats:?}");
}

#[test]
fn rep_ret_runs_translated() {
    let code = [
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0xe8, 0x04, 0, 0, 0, // call +4
        0x49, // dec ecx
        0x75, 0xf8, // jnz -8
        0xf4, // hlt
        0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, // inc eax, past the interpreter's tail
        0xf3, 0xc3, // repz ret
    ];
    let stats = compare(&code, |_| {});
    assert!(stats.interp_exits < 8, "glibc returns with repz ret, so each one leaving translated code is slow: {stats:?}");
}

#[test]
fn prefixed_branches_and_parity_conditions_run_translated() {
    let mut code = vec![
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0x89, 0xc8, // loop: mov eax, ecx
        0x83, 0xe0, 0x07, // and eax, 7
        0x0f, 0x9a, 0xc3, // setp bl
        0x0f, 0x9b, 0xc7, // setnp bh
        0x01, 0xde, // add esi, ebx
        0x0f, 0x4a, 0xf9, // cmovp edi, ecx
        0x0f, 0x4b, 0xd1, // cmovnp edx, ecx
        0x83, 0xf8, 0x03, // cmp eax, 3
        0xf2, 0x7a, 0x01, // bnd jp +1
        0x45, // inc ebp
        0xf3, 0x7b, 0x01, // rep jnp +1
        0x4d, // dec ebp
        0x85, 0xc0, // test eax, eax
        0x0f, 0x8a, 0x01, 0, 0, 0, // jp +1
        0x47, // inc edi
        0xf2, 0xe8, 0x0c, 0, 0, 0, // bnd call +12
    ];
    code.extend([0x90; 12]); // past the interpreter's tail
    code.extend([0x49, 0xf2, 0x75, 0xcb, 0xf4]); // dec ecx; bnd jnz loop; hlt
    code.extend([0x40, 0xf2, 0xc3]); // inc eax; bnd ret
    let stats = compare(&code, |_| {});
    assert!(stats.interp_exits < 8, "Firefox's builds branch on parity after UCOMISS and prefix branches: {stats:?}");
}

#[test]
fn bit_tests_with_a_register_index_match_the_interpreter() {
    let mut rng = Rng(0xb17);
    for round in 0..200 {
        let mut code = Vec::new();
        for _ in 0..20 {
            let op = [0xa3, 0xab, 0xb3, 0xbb][rng.below(4) as usize];
            let (reg, rm) = (rng.below(4) as u8, rng.below(4) as u8);
            match rng.below(3) {
                0 => code.extend([0x0f, op, modrm_rr(reg, rm)]),
                _ => code.extend([0x0f, op, 0x80 | reg << 3 | 6, 0x00, 0x01, 0, 0]), // [esi+0x100]
            }
            code.extend([0x0f, 0x92, 0xc0 | rng.below(8) as u8]); // setc r8
        }
        code.push(0xf4);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        let mut vals = [0u32; 4];
        for v in vals.iter_mut() {
            *v = match rng.below(3) {
                0 => rng.below(64 * 8),
                1 => rng.below(64 * 8).wrapping_neg(),
                _ => rng.next(),
            };
        }
        let mut a = flat_cpu(0x1000, 0x8000);
        a.regs[..4].copy_from_slice(&vals);
        a.regs[6] = 0x3100;
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        for k in 0..0x200 {
            let byte = rng.next() as u8;
            bus_a.ram.as_mut_slice()[0x2f00 + k] = byte;
            bus_b.ram.as_mut_slice()[0x2f00 + k] = byte;
        }
        let in_range = vals.iter().all(|&v| (v as i32) >= -0x800 && (v as i32) < 0x800);
        if !in_range {
            a.regs[..4].iter_mut().zip(b.regs[..4].iter_mut()).for_each(|(x, y)| {
                *x &= 0x3ff;
                *y = *x;
            });
        }
        run_interp(&mut a, &mut bus_a);
        let stats = run_jit(&mut b, &mut bus_b);
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.eflags & x86::cpu::flags::CF, b.eflags & x86::cpu::flags::CF, "round {round} CF\ncode: {}", hex.join(" "));
        assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "round {round} memory\ncode: {}", hex.join(" "));
        assert!(stats.interp_exits < 4, "round {round}: {stats:?}");
    }
}

#[test]
fn kernel_reads_of_the_pm_timer_stay_translated() {
    const PORT: u16 = 0x408;
    let mul = 0x2622_0000_0000_0000u64;
    let add = 12345u64;
    let mut code = vec![0xb9, 0x40, 0, 0, 0, 0x66, 0xba, 0x08, 0x04]; // mov ecx, 64; mov dx, 0x408
    code.extend([0xed, 0x89, 0xc3]); // loop: in eax, dx; mov ebx, eax
    code.extend([0x90; 12]);
    code.extend([0x49, 0x75, 0xee]); // dec ecx; jnz loop
    code.extend([0x66, 0xba, 0x09, 0x04, 0xed, 0xf4]); // mov dx, 0x409; in eax, dx; hlt
    let mut cpu = flat_cpu(0x1000, 0x8000);
    let mut bus = bus_with(0x1000, &code);
    let formula = || ((((x86::cpu::host_ticks() as u128 * mul as u128) >> 64) as u64).wrapping_add(add) & 0xff_ffff) as u32;
    let before = formula();
    let stats = run_jit_with(&mut cpu, &mut bus, |j| j.set_pm_timer(Some((PORT, mul, add))), |_| {});
    let after = formula();
    let read = cpu.regs[3];
    assert!(read.wrapping_sub(before) & 0xff_ffff <= after.wrapping_sub(before) & 0xff_ffff, "{before:#x} {read:#x} {after:#x}");
    assert_eq!(cpu.regs[0], 0xffff_ffff, "other ports still reach the bus");
    assert!(stats.interp_exits < 8, "XP's QueryPerformanceCounter reads the PM timer constantly: {stats:?}");
}

#[test]
fn one_operand_mul_and_imul_run_translated() {
    let mut code = vec![
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0xc7, 0x05, 0x00, 0x30, 0, 0, 0x07, 0xf0, 0xff, 0xff, // mov dword [0x3000], -4089
        0xb8, 0x39, 0x30, 0, 0, // loop: mov eax, 12345
        0xf7, 0x2d, 0x00, 0x30, 0, 0, // imul dword [0x3000]
        0x01, 0xd6, // add esi, edx
        0xf7, 0xe9, // imul ecx
        0x01, 0xc7, // add edi, eax
        0xf7, 0x25, 0x00, 0x30, 0, 0, // mul dword [0x3000]
        0x01, 0xd3, // add ebx, edx
    ];
    code.extend([0x90; 12]); // past the interpreter's tail
    code.extend([0x49, 0x75, 0xd8, 0xf4]); // dec ecx; jnz loop; hlt
    let stats = compare(&code, |_| {});
    assert!(stats.interp_exits < 8, "one-operand IMUL is common in XP and Linux and must not leave translated code: {stats:?}");
}

#[test]
fn popf_changes_to_ac_and_id_reach_the_cpu() {
    let code = [
        0x9c, // pushfd
        0x81, 0x34, 0x24, 0x00, 0x00, 0x24, 0x00, // xor dword [esp], AC | ID
        0x9d, // popfd
        0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // run on in translated code
        0xf4,
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    run_interp(&mut a, &mut bus_a);
    run_jit(&mut b, &mut bus_b);
    assert_eq!(a.eflags, b.eflags);
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }

    /// A general register other than ESP.
    fn reg(&mut self) -> u8 {
        let r = self.below(7) as u8;
        if r >= 4 { r + 1 } else { r }
    }
}

fn modrm_rr(reg: u8, rm: u8) -> u8 {
    0xc0 | reg << 3 | rm
}

fn random_insn(rng: &mut Rng, out: &mut Vec<u8>) {
    let mem = |out: &mut Vec<u8>, reg: u8, slot: u32| {
        out.push(reg << 3 | 5);
        out.extend((0x3000 + slot * 4).to_le_bytes());
    };
    match rng.below(40) {
        38 | 39 => match rng.below(8) {
            6 => out.push(0x9e),
            0 => out.extend([[0x06, 0x0e, 0x1e][rng.below(3) as usize], 0x58 + rng.reg()]),
            1 => out.extend([0x0f, [0xa0, 0xa8][rng.below(2) as usize], 0x58 + rng.reg()]),
            2 => {
                out.extend([0x50 + rng.reg(), 0x8f]);
                if rng.below(2) == 0 {
                    out.push(modrm_rr(0, rng.reg()));
                } else {
                    mem(out, 0, rng.below(4));
                }
            }
            3 => out.extend([0x60, 0x83, 0xc0 + rng.reg(), rng.next() as u8, 0x61]),
            4 => out.push(0x90 + rng.reg()),
            5 => out.extend([0xf7, modrm_rr(4 + rng.below(2) as u8, rng.below(8) as u8)]),
            _ => {
                out.extend([0x0f, 0xbc + rng.below(2) as u8]);
                if rng.below(2) == 0 {
                    out.push(modrm_rr(rng.reg(), rng.reg()));
                } else {
                    mem(out, rng.reg(), rng.below(4));
                }
            }
        },
        36 | 37 => match rng.below(9) {
            0 => out.extend([0x0f, 0xc8 + rng.reg()]),
            1 => {
                let kind = [0, 1, 4, 5, 6, 7][rng.below(6) as usize];
                match rng.below(3) {
                    0 => out.extend([0xc0, modrm_rr(kind, rng.below(8) as u8), rng.below(40) as u8]),
                    1 => out.extend([0xd0, modrm_rr(kind, rng.below(8) as u8)]),
                    _ => out.extend([0xd2, modrm_rr(kind, rng.below(8) as u8)]),
                }
            }
            2 => {
                let kind = [0, 1, 4, 5, 6, 7][rng.below(6) as usize];
                match rng.below(3) {
                    0 => out.extend([0x66, 0xc1, modrm_rr(kind, rng.reg()), rng.below(40) as u8]),
                    1 => out.extend([0x66, 0xd1, modrm_rr(kind, rng.reg())]),
                    _ => out.extend([0x66, 0xd3, modrm_rr(kind, rng.reg())]),
                }
            }
            3 => {
                let r = rng.reg();
                out.extend([0x9c, 0x58 + r]);
            }
            4 => {
                let r = rng.reg();
                out.extend([0x81, modrm_rr(4, r)]);
                out.extend(0x0cd5u32.to_le_bytes());
                out.extend([0x50 + r, 0x9d]);
            }
            5 => {
                if rng.below(2) == 0 {
                    out.extend([0x0f, 0xba, modrm_rr(4 + rng.below(4) as u8, rng.reg()), rng.next() as u8]);
                } else {
                    out.extend([0x66, 0x0f, 0xba, modrm_rr(4 + rng.below(4) as u8, rng.reg()), rng.next() as u8]);
                }
            }
            6 => {
                out.extend([0x0f, 0xba, (4 + rng.below(4) as u8) << 3 | 5]);
                out.extend((0x3000 + rng.below(4) * 4).to_le_bytes());
                out.push(rng.next() as u8);
            }
            7 => out.push([0xf5, 0xf8, 0xf9][rng.below(3) as usize]),
            _ => {
                out.extend([0x0f, 0x18, (rng.below(4) as u8) << 3 | 5]);
                out.extend((0x3000u32).to_le_bytes());
            }
        },
        34 | 35 => match rng.below(10) {
            0 => out.extend([0xc1, modrm_rr(rng.below(2) as u8, rng.reg()), rng.below(40) as u8]),
            1 => out.extend([0xd1, modrm_rr(rng.below(2) as u8, rng.reg())]),
            2 => out.extend([0xd3, modrm_rr(rng.below(2) as u8, rng.reg())]),
            3 => {
                if rng.below(2) == 0 {
                    out.extend([0xf6, modrm_rr(2 + rng.below(2) as u8, rng.below(8) as u8)]);
                } else {
                    out.extend([0x66, 0xf7, modrm_rr(2 + rng.below(2) as u8, rng.reg())]);
                }
            }
            4 => {
                if rng.below(2) == 0 {
                    out.extend([0x66, 0x40 + rng.reg() + 8 * rng.below(2) as u8]);
                } else {
                    out.extend([0x66, 0xff, modrm_rr(rng.below(2) as u8, rng.reg())]);
                }
            }
            5 | 6 => {
                let op = if rng.below(2) == 0 { 0xc1 } else { 0xb1 };
                match rng.below(3) {
                    0 => out.extend([0x0f, op, modrm_rr(rng.reg(), rng.reg())]),
                    1 => out.extend([0x0f, op - 1, modrm_rr(rng.below(8) as u8, rng.below(8) as u8)]),
                    _ => {
                        out.extend([0x0f, op]);
                        mem(out, rng.reg(), rng.below(4));
                    }
                }
            }
            7 => {
                out.extend([0x0f, 0xc7, 1 << 3 | 5]);
                out.extend((0x3000 + rng.below(2) * 8).to_le_bytes());
            }
            8 => {
                let op = [0xa4, 0xa5, 0xac, 0xad][rng.below(4) as usize];
                out.extend([0x0f, op]);
                if rng.below(2) == 0 {
                    out.push(modrm_rr(rng.reg(), rng.reg()));
                } else {
                    mem(out, rng.reg(), rng.below(4));
                }
                if op & 1 == 0 {
                    out.push(rng.below(40) as u8);
                }
            }
            _ => {
                out.extend([0x0f, 0x2b, (rng.below(8) as u8) << 3 | 5]);
                out.extend((0x3000 + rng.below(4) * 16).to_le_bytes());
            }
        },
        30 => {
            out.extend([0xbe]);
            out.extend((0x3000 + rng.below(16)).to_le_bytes());
            out.extend([0xbf]);
            out.extend((0x3040 + rng.below(16)).to_le_bytes());
            out.extend([0xb9]);
            out.extend(rng.below(6).to_le_bytes());
            let prefix = [0xf3, 0xf2, 0x90][rng.below(3) as usize];
            let op = [0xa4, 0xa5, 0xaa, 0xab, 0xa6, 0xa7, 0xae, 0xaf, 0xac][rng.below(9) as usize];
            out.extend([prefix, op]);
        }
        31 => {
            out.extend([0x31, 0xd2]);
            out.extend([0xbb]);
            out.extend((rng.next() | 1).to_le_bytes());
            out.extend([0xf7, 0xf3]);
        }
        32 => {
            out.extend([0x99, 0xbb]);
            out.extend(((rng.next() >> 1) | 1).to_le_bytes());
            out.extend([0xf7, 0xfb]);
        }
        33 => match rng.below(4) {
            0 => out.push([0xfc, 0xfd, 0xfc][rng.below(3) as usize]),
            1 => out.extend([0xfe, modrm_rr(rng.below(2) as u8, rng.below(8) as u8)]),
            2 => {
                out.extend([0x66, 0xf7, modrm_rr(0, rng.reg())]);
                out.extend((rng.next() as u16).to_le_bytes());
            }
            _ => {
                out.extend([0xf0, 0x01]);
                out.push(rng.reg() << 3 | 5);
                out.extend((0x3000 + rng.below(4) * 4).to_le_bytes());
            }
        },
        20 => {
            let ops = [0x01, 0x03, 0x09, 0x0b, 0x11, 0x13, 0x19, 0x1b, 0x21, 0x23, 0x29, 0x2b, 0x31, 0x33, 0x39, 0x3b, 0x85, 0x89, 0x8b, 0x87];
            out.extend([0x66, ops[rng.below(ops.len() as u32) as usize], modrm_rr(rng.reg(), rng.reg())]);
        }
        21 => {
            let op = [0, 1, 4, 5, 6, 7][rng.below(6) as usize];
            if rng.below(2) == 0 {
                out.extend([0x66, 0x81, modrm_rr(op, rng.reg())]);
                out.extend((rng.next() as u16).to_le_bytes());
            } else {
                out.extend([0x66, 0x83, modrm_rr(op, rng.reg()), rng.next() as u8]);
            }
        }
        22 => {
            out.extend([0x66, 0xb8 + rng.reg()]);
            out.extend((rng.next() as u16).to_le_bytes());
        }
        23 => out.extend([0x89, 0xe5, 0xc9]),
        24 => {
            out.push([0xa1, 0xa3, 0xa0, 0xa2][rng.below(4) as usize]);
            out.extend((0x3000 + rng.below(4) * 4).to_le_bytes());
        }
        25 => out.extend([0xd3, modrm_rr([4, 5, 7][rng.below(3) as usize], rng.reg())]),
        26 => out.extend([[0x86, 0x87][rng.below(2) as usize], modrm_rr(rng.reg(), rng.reg())]),
        27 => match rng.below(3) {
            0 => out.extend([0xf7, modrm_rr(4, rng.reg())]),
            1 => out.extend([0x6b, modrm_rr(rng.reg(), rng.reg()), rng.next() as u8]),
            _ => {
                out.extend([0x69, modrm_rr(rng.reg(), rng.reg())]);
                out.extend(rng.next().to_le_bytes());
            }
        },
        28 => out.push([0x98, 0x99, 0xfa, 0xfb][rng.below(4) as usize]),
        29 => out.extend([0xe3, 0x01, 0x40 + rng.reg()]),
        0 | 1 => {
            let ops = [0x01, 0x03, 0x09, 0x0b, 0x11, 0x13, 0x19, 0x1b, 0x21, 0x23, 0x29, 0x2b, 0x31, 0x33, 0x39, 0x3b];
            out.push(ops[rng.below(16) as usize]);
            out.push(modrm_rr(rng.reg(), rng.reg()));
        }
        2 => {
            let op = rng.below(8) as u8;
            if rng.below(2) == 0 {
                out.extend([0x81, modrm_rr(op, rng.reg())]);
                out.extend(rng.next().to_le_bytes());
            } else {
                out.extend([0x83, modrm_rr(op, rng.reg()), rng.next() as u8]);
            }
        }
        3 => {
            let ops = [0x01, 0x03, 0x29, 0x2b, 0x31, 0x39, 0x89, 0x8b];
            out.push(ops[rng.below(8) as usize]);
            mem(out, rng.reg(), rng.below(4));
        }
        4 => {
            let ops = [0x00, 0x02, 0x08, 0x0a, 0x10, 0x12, 0x18, 0x1a, 0x20, 0x22, 0x28, 0x2a, 0x30, 0x32, 0x38, 0x3a, 0x84, 0x88, 0x8a];
            out.push(ops[rng.below(ops.len() as u32) as usize]);
            out.push(modrm_rr(rng.below(8) as u8, rng.below(8) as u8));
        }
        5 => {
            let op = rng.below(8) as u8;
            out.extend([0x80, modrm_rr(op, rng.below(8) as u8), rng.next() as u8]);
        }
        6 => out.push(if rng.below(2) == 0 { 0x40 } else { 0x48 } + rng.reg()),
        7 => {
            let kind = [4, 5, 7][rng.below(3) as usize];
            if rng.below(2) == 0 {
                out.extend([0xc1, modrm_rr(kind, rng.reg()), rng.below(40) as u8]);
            } else {
                out.extend([0xd1, modrm_rr(kind, rng.reg())]);
            }
        }
        8 => {
            out.extend([0x85, modrm_rr(rng.reg(), rng.reg())]);
        }
        9 => {
            out.extend([0x0f, 0xaf, modrm_rr(rng.reg(), rng.reg())]);
        }
        10 => {
            let op = [0xb6, 0xb7, 0xbe, 0xbf][rng.below(4) as usize];
            out.extend([0x0f, op, modrm_rr(rng.reg(), rng.below(8) as u8)]);
        }
        11 => {
            let cc = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13, 14, 15][rng.below(14) as usize];
            if rng.below(2) == 0 {
                out.extend([0x0f, 0x90 + cc, modrm_rr(0, rng.below(8) as u8)]);
            } else {
                out.extend([0x0f, 0x40 + cc, modrm_rr(rng.reg(), rng.reg())]);
            }
        }
        12 => out.extend([0xf7, modrm_rr(2 + rng.below(2) as u8, rng.reg())]),
        13 => {
            let r = rng.reg();
            out.extend([0x50 + rng.reg(), 0x58 + r]);
        }
        14 => {
            let cc = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13, 14, 15][rng.below(14) as usize];
            out.extend([0x70 + cc, 0x01, 0x40 + rng.reg()]);
        }
        15 => {
            out.extend([0x8d, 0x44 | rng.reg() << 3, rng.reg() << 3 | rng.reg(), rng.next() as u8]);
        }
        16 => {
            out.push(0xb8 + rng.reg());
            out.extend(rng.next().to_le_bytes());
        }
        17 => out.extend([0xa8, rng.next() as u8]),
        18 => {
            out.push(0xa9);
            out.extend(rng.next().to_le_bytes());
        }
        _ => {
            out.extend([0xf7, modrm_rr(0, rng.reg())]);
            out.extend(rng.next().to_le_bytes());
        }
    }
}

#[test]
fn random_programs_match_the_interpreter() {
    let mut rng = Rng(0x86_5109);
    let mut interp_exits = 0;
    let mut translated = 0;
    for round in 0..300 {
        let mut code = Vec::new();
        for _ in 0..40 {
            random_insn(&mut rng, &mut code);
        }
        code.push(0xf4);
        let regs: Vec<u32> = (0..8).map(|_| rng.next()).collect();
        let flags = rng.next() & ARITH_MASK;
        let xmm: Vec<u128> = (0..8).map(|_| (rng.next() as u128) << 96 | (rng.next() as u128) << 32 | rng.next() as u128).collect();
        let result = std::panic::catch_unwind(|| {
            compare(&code, |c| {
                for (i, v) in regs.iter().enumerate() {
                    if i != 4 {
                        c.regs[i] = *v;
                    }
                }
                c.eflags |= flags;
                c.cr4 |= x86::cpu::cr4::OSFXSR;
                c.xmm.copy_from_slice(&xmm);
            })
        });
        match result {
            Ok(stats) => {
                interp_exits += stats.interp_exits;
                translated += stats.translated_insns;
            }
            Err(e) => {
                let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
                panic!("round {round} differs: {:?}\ncode: {}", e.downcast_ref::<String>(), hex.join(" "));
            }
        }
    }
    // Besides each final HLT, only string instructions running with DF=1 use the interpreter.
    assert!(interp_exits < 360, "{interp_exits} interpreter exits");
    assert!(translated > 300 * 40);
}

#[test]
fn random_programs_with_forward_branches_match_the_interpreter() {
    let mut rng = Rng(0x5b_10c5);
    for round in 0..400 {
        let mut insns: Vec<Vec<u8>> = Vec::new();
        for _ in 0..30 {
            let mut one = Vec::new();
            match rng.below(6) {
                0 => one.extend([0x9c, 0x58 + rng.reg()]),
                1 => one.push(0x9f),
                2 => one.extend([0x0f, 0x90 + rng.below(16) as u8, modrm_rr(0, rng.below(8) as u8)]),
                _ => random_insn(&mut rng, &mut one),
            }
            insns.push(one);
        }
        let mut at = vec![0; insns.len() + 1];
        let mut out = Vec::new();
        let mut fixups = Vec::new();
        for (i, one) in insns.iter().enumerate() {
            if rng.below(3) == 0 {
                let to = (i + 1 + rng.below(6) as usize).min(insns.len());
                fixups.push((out.len(), to));
                out.extend([0x70 + rng.below(16) as u8, 0]);
            }
            at[i] = out.len();
            out.extend(one);
        }
        at[insns.len()] = out.len();
        for (pos, to) in fixups {
            out[pos + 1] = i8::try_from(at[to] - (pos + 2)).unwrap_or(0) as u8;
        }
        out.push(0xf4);
        let regs: Vec<u32> = (0..8).map(|_| rng.next()).collect();
        let flags = rng.next() & ARITH_MASK;
        let result = std::panic::catch_unwind(|| {
            compare(&out, |c| {
                for (i, v) in regs.iter().enumerate() {
                    if i != 4 {
                        c.regs[i] = *v;
                    }
                }
                c.eflags |= flags;
                c.cr4 |= x86::cpu::cr4::OSFXSR;
            })
        });
        if let Err(e) = result {
            let hex: Vec<String> = out.iter().map(|b| format!("{b:02x}")).collect();
            panic!("round {round} differs: {:?}\ncode: {}", e.downcast_ref::<String>(), hex.join(" "));
        }
    }
}

#[test]
fn user_mode_code_matches_the_interpreter() {
    let code = [
        0xb8, 0x05, 0, 0, 0, // mov eax, 5
        0x01, 0xc0, // add eax, eax
        0x50, // push eax
        0x5b, // pop ebx
        0xfa, // cli: #GP at CPL 3 (IOPL 0), handled by the interpreter
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    for seg in &mut a.segs {
        seg.selector |= 3;
        seg.attr |= 3 << 5;
    }
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    for _ in 0..5 {
        interp::step(&mut a, &mut bus_a);
    }
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus_b.ram.as_ptr(), RAM).unwrap() };
    assert!(supported(&b));
    let exit = jit.run(&mut b, &mut bus_b).unwrap();
    assert!(matches!(exit, RunExit::Interp(_)));
    assert_eq!(a.regs, b.regs);
    assert_eq!(a.eip, b.eip);
    assert_eq!(jit.stats.translated_insns, 4);
}

#[test]
fn instruction_across_a_page_end() {
    let mut code = vec![0x90; 0xffd];
    code.extend([0xb8, 0x78, 0x56, 0x34, 0x12, 0x40, 0xf4]);
    let stats = compare(&code, |_| {});
    assert_eq!(stats.interp_exits, 1, "only the final HLT should be interpreted");

    let mut cpu = flat_cpu(0x1000, 0x8000);
    let mut bus = bus_with(0x1000, &code);
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus.ram.as_ptr(), RAM).unwrap() };
    for round in 0..2 {
        cpu.eip = 0x1000;
        cpu.halted = false;
        while !cpu.halted {
            jit.run(&mut cpu, &mut bus).unwrap();
        }
        assert_eq!(cpu.regs[0], if round == 0 { 0x1234_5679 } else { 0x9934_5679 });
        bus.ram.as_mut_slice()[0x2001] = 0x99;
    }
}

#[test]
fn code_rewritten_every_iteration_matches_the_interpreter() {
    let code = [
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0x88, 0x0d, 0x0d, 0x10, 0, 0, // loop: mov [0x100d], cl (imm of the next mov)
        0xb8, 0, 0, 0, 0, // mov eax, imm32
        0x01, 0xc3, // add ebx, eax
        0x49, // dec ecx
        0x75, 0xf0, // jnz loop
        0xf4,
    ];
    let stats = compare(&code, |c| c.regs[3] = 0);
    assert!(stats.stale_blocks > 0, "the page should have switched to self-checking blocks: {stats:?}");
}

#[test]
fn rewritten_x87_code_leaves_its_self_checking_block_cleanly() {
    let code = [
        0xb9, 0x40, 0, 0, 0, // mov ecx, 64
        0xd9, 0xee, // fldz
        0x88, 0x0d, 0x0f, 0x10, 0, 0, // loop: mov [0x100f], cl (imm of the next mov)
        0xb8, 0, 0, 0, 0, // mov eax, imm32
        0x01, 0xc3, // add ebx, eax
        0xdb, 0x05, 0x00, 0x30, 0x00, 0x00, // fild dword [0x3000]
        0xdd, 0xd9, // fstp st(1)
        0xff, 0x05, 0x00, 0x30, 0x00, 0x00, // inc dword [0x3000]
        0x49, // dec ecx
        0x75, 0xe2, // jnz loop
        0xf4,
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert!(stats.stale_blocks > 0, "the page should have switched to self-checking blocks: {stats:?}");
    assert_eq!(a.regs, b.regs);
    assert_eq!(a.fpu, b.fpu, "a stale self-checking block must not lose or misplace cached x87 values");
    assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "memory differs");
}

#[test]
fn atomics_match_the_interpreter_when_the_compare_succeeds() {
    let programs: [&[u8]; 6] = [
        &[0xa1, 0, 0x30, 0, 0, 0xf0, 0x0f, 0xb1, 0x0d, 0, 0x30, 0, 0, 0xf4],
        &[0xa0, 0, 0x30, 0, 0, 0xf0, 0x0f, 0xb0, 0x2d, 0, 0x30, 0, 0, 0xf4],
        &[0x8b, 0x05, 0, 0x30, 0, 0, 0x8b, 0x15, 4, 0x30, 0, 0, 0xf0, 0x0f, 0xc7, 0x0d, 0, 0x30, 0, 0, 0xf4],
        &[0xf0, 0x0f, 0xc1, 0x1d, 0, 0x30, 0, 0, 0xf4],
        &[0x89, 0xc1, 0x0f, 0xb1, 0xc8, 0xf4],
        &[0x0f, 0xb1, 0xc0, 0xf4],
    ];
    for code in programs {
        compare(code, |c| {
            c.regs[1] = 0x1111_2222;
            c.regs[3] = 0x3333_4444;
            c.regs[5] = 0x5555_6666;
        });
    }
}

/// Identity-maps the first 4 MiB through a page table at 0x11000 (or one PSE
/// page) and turns paging on, so shadow faults go through the in-VM walk.
fn paged(cpu: &mut Cpu, bus: &mut FlatBus, big: bool) {
    let mem = bus.ram.as_mut_slice();
    let pde = if big { 0x87u32 } else { 0x11007 };
    mem[0x10000..0x10004].copy_from_slice(&pde.to_le_bytes());
    for i in 0..1024u32 {
        let pte = (i << 12) | 7;
        let at = 0x11000 + i as usize * 4;
        mem[at..at + 4].copy_from_slice(&pte.to_le_bytes());
    }
    cpu.cr3 = 0x10000;
    cpu.cr4 |= x86::cpu::cr4::PSE | x86::cpu::cr4::OSFXSR;
    cpu.cr0 |= x86::cpu::cr0::PG | x86::cpu::cr0::WP;
}

#[test]
fn cr3_switches_in_the_vm_drop_the_old_non_global_mappings() {
    let code = [
        0xbb, 0x00, 0x00, 0x02, 0x00, // mov ebx, 0x20000
        0xba, 0x00, 0x00, 0x01, 0x00, // mov edx, 0x10000
        0xb9, 0x05, 0, 0, 0, // mov ecx, 5
        0xa1, 0x00, 0x50, 0, 0, // loop: mov eax, [0x5000]
        0x01, 0xc6, // add esi, eax
        0x0f, 0x22, 0xdb, // mov cr3, ebx
        0xa1, 0x00, 0x50, 0, 0, // mov eax, [0x5000]
        0x01, 0xc7, // add edi, eax
        0x0f, 0x22, 0xda, // mov cr3, edx
        0x49, // dec ecx
        0x75, 0xe9, // jnz loop
        0xf4, // hlt
    ];
    let setup = |cpu: &mut Cpu, bus: &mut FlatBus| {
        paged(cpu, bus, false);
        let mem = bus.ram.as_mut_slice();
        for (dir, table, data) in [(0x10000usize, 0x11000usize, 0x5000u32), (0x20000, 0x21000, 0x6000)] {
            mem[dir..dir + 4].copy_from_slice(&(table as u32 | 7).to_le_bytes());
            for i in 0..1024u32 {
                let pte = if i == 5 { data | 7 } else { i << 12 | 0x107 };
                mem[table + 4 * i as usize..table + 4 * i as usize + 4].copy_from_slice(&pte.to_le_bytes());
            }
        }
        mem[0x5000..0x5004].copy_from_slice(&0x1111_1111u32.to_le_bytes());
        mem[0x6000..0x6004].copy_from_slice(&0x2222_2222u32.to_le_bytes());
        cpu.cr4 |= x86::cpu::cr4::PGE;
    };
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    setup(&mut a, &mut bus_a);
    setup(&mut b, &mut bus_b);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!((a.regs[6], a.regs[7]), (0x5555_5555, 0xaaaa_aaaa));
    assert!(stats.vm_cr3_switches >= 8, "CR3 writes left translated code: {stats:?}");
}

#[test]
fn code_at_an_address_hypervisor_framework_reads_as_an_smccc_query_runs() {
    let code = [
        0xb8, 0x03, 0xff, 0x00, 0x83, // mov eax, 0x8300ff03
        0xff, 0xe0, // jmp eax
    ];
    let target = [
        0x40, // inc eax
        0xf4, // hlt
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut bus_a = bus_with(0x1000, &code);
    paged(&mut a, &mut bus_a, false);
    let mem = bus_a.ram.as_mut_slice();
    mem[0x10000 + 4 * (0x8300_ff03usize >> 22)..][..4].copy_from_slice(&0x12007u32.to_le_bytes());
    let pte_at = 0x12000 + 4 * ((0x8300_ff03usize >> 12) & 0x3ff);
    mem[pte_at..pte_at + 4].copy_from_slice(&0x5007u32.to_le_bytes());
    mem[0x5f03..0x5f03 + target.len()].copy_from_slice(&target);
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap(), gart: None };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!(b.regs[0], 0x8300_ff04);
}

#[test]
fn invlpg_in_the_vm_drops_the_old_mapping() {
    let code = [
        0x8b, 0x35, 0x00, 0x50, 0, 0, // mov esi, [0x5000]
        0xc7, 0x05, 0x14, 0x10, 0x01, 0x00, 0x07, 0x60, 0, 0, // mov dword [0x11014], 0x6007: PTE of 0x5000
        0x0f, 0x01, 0x3d, 0x00, 0x50, 0, 0, // invlpg [0x5000]
        0x8b, 0x3d, 0x00, 0x50, 0, 0, // mov edi, [0x5000]
        0xf4, // hlt
    ];
    let setup = |cpu: &mut Cpu, bus: &mut FlatBus| {
        paged(cpu, bus, false);
        let mem = bus.ram.as_mut_slice();
        mem[0x5000..0x5004].copy_from_slice(&0x1111_1111u32.to_le_bytes());
        mem[0x6000..0x6004].copy_from_slice(&0x2222_2222u32.to_le_bytes());
    };
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    setup(&mut a, &mut bus_a);
    setup(&mut b, &mut bus_b);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!((b.regs[6], b.regs[7]), (0x1111_1111, 0x2222_2222));
    assert!(stats.interp_exits <= 1, "INVLPG left translated code: {stats:?}");
}

#[test]
fn interpreted_invlpgs_drop_their_pages_without_a_full_flush() {
    let mut code = vec![
        0x8b, 0x35, 0x00, 0x50, 0, 0, // mov esi, [0x5000]
        0x8b, 0x2d, 0x00, 0x70, 0, 0, // mov ebp, [0x7000]
        0xc7, 0x05, 0x14, 0x10, 0x01, 0x00, 0x07, 0x60, 0, 0, // mov dword [0x11014], 0x6007: PTE of 0x5000
        0xc7, 0x05, 0x1c, 0x10, 0x01, 0x00, 0x07, 0x60, 0, 0, // mov dword [0x1101c], 0x6007: PTE of 0x7000
        0x67, 0x0f, 0x01, 0x3e, 0x00, 0x50, // invlpg [0x5000], 16-bit address
        0x67, 0x0f, 0x01, 0x3e, 0x00, 0x70, // invlpg [0x7000], 16-bit address
    ];
    // Enough NOPs that the interpreter hands back to translated code.
    code.extend([0x90; 16]);
    code.extend([
        0x8b, 0x3d, 0x00, 0x50, 0, 0, // mov edi, [0x5000]
        0x8b, 0x1d, 0x00, 0x70, 0, 0, // mov ebx, [0x7000]
        0xf4, // hlt
    ]);
    let setup = |cpu: &mut Cpu, bus: &mut FlatBus| {
        paged(cpu, bus, false);
        let mem = bus.ram.as_mut_slice();
        mem[0x5000..0x5004].copy_from_slice(&0x1111_1111u32.to_le_bytes());
        mem[0x6000..0x6004].copy_from_slice(&0x2222_2222u32.to_le_bytes());
        mem[0x7000..0x7004].copy_from_slice(&0x3333_3333u32.to_le_bytes());
    };
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    setup(&mut a, &mut bus_a);
    setup(&mut b, &mut bus_b);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!((b.regs[6], b.regs[5]), (0x1111_1111, 0x3333_3333));
    assert_eq!((b.regs[7], b.regs[3]), (0x2222_2222, 0x2222_2222));
    assert_eq!(stats.flush_multi_avoided, 1, "the INVLPGs fell back to a full shadow flush: {stats:?}");
}

/// Calls a routine at linear 0x5000, writes its page, remaps the page to
/// physical 0x6000 holding `copy` and calls it again.
fn run_moved_routine(copy: &[u8]) -> (Cpu, Stats) {
    let routine = [0xb8, 0x05, 0, 0, 0, 0x01, 0xd8, 0xc3]; // mov eax, 5; add eax, ebx; ret
    let mut code = vec![0xe8, 0xfb, 0x3f, 0, 0]; // call 0x5000
    code.extend([0x89, 0xc1]); // mov ecx, eax
    code.extend([0xc7, 0x05, 0x00, 0x58, 0, 0, 0x78, 0x56, 0x34, 0x12]); // mov dword [0x5800], 0x12345678
    code.extend([0xc7, 0x05, 0x14, 0x10, 0x01, 0x00, 0x07, 0x60, 0, 0]); // mov dword [0x11014], 0x6007: PTE of 0x5000
    code.extend([0x0f, 0x01, 0x3d, 0x00, 0x50, 0, 0]); // invlpg [0x5000]
    let rel = 0x5000u32.wrapping_sub(0x1000 + code.len() as u32 + 5);
    code.push(0xe8);
    code.extend(rel.to_le_bytes()); // call 0x5000
    code.push(0xf4); // hlt
    let setup = |cpu: &mut Cpu, bus: &mut FlatBus| {
        paged(cpu, bus, false);
        cpu.regs[3] = 0x100;
        let mem = bus.ram.as_mut_slice();
        mem[0x5000..0x5000 + routine.len()].copy_from_slice(&routine);
        mem[0x6000..0x6000 + copy.len()].copy_from_slice(copy);
    };
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, &code);
    let mut bus_b = bus_with(0x1000, &code);
    setup(&mut a, &mut bus_a);
    setup(&mut b, &mut bus_b);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!(b.regs[1], 0x105, "translated code miscomputes before its page moves");
    (b, stats)
}

#[test]
fn blocks_move_to_an_identical_copy_of_their_page() {
    let (cpu, stats) = run_moved_routine(&[0xb8, 0x05, 0, 0, 0, 0x01, 0xd8, 0xc3]);
    assert_eq!(cpu.regs[0], 0x105);
    assert!(stats.revived_blocks >= 1, "the copy was translated again: {stats:?}");
}

#[test]
fn blocks_do_not_move_to_a_page_with_other_code() {
    let (cpu, stats) = run_moved_routine(&[0xb8, 0x07, 0, 0, 0, 0x01, 0xd8, 0xc3]);
    assert_eq!(cpu.regs[0], 0x107, "the old translation ran on the changed copy");
    assert_eq!(stats.revived_blocks, 0, "changed code reused stale translations: {stats:?}");
}

#[test]
fn random_programs_match_the_interpreter_with_paging() {
    let mut rng = Rng(0x5ad0_77);
    let mut fills = 0;
    for round in 0..200 {
        let mut code = Vec::new();
        for _ in 0..40 {
            random_insn(&mut rng, &mut code);
        }
        code.push(0xf4);
        let regs: Vec<u32> = (0..8).map(|_| rng.next()).collect();
        let big = round % 2 == 1;
        let mut a = flat_cpu(0x1000, 0x8000);
        for (i, v) in regs.iter().enumerate() {
            if i != 4 {
                a.regs[i] = *v;
            }
        }
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        paged(&mut a, &mut bus_a, big);
        paged(&mut b, &mut bus_b, big);
        run_interp(&mut a, &mut bus_a);
        if std::env::var_os("DBG").is_some() {
            eprintln!("round {round} code at 0x4d: {:02x?} interp regs {:08x?}", &code[0x40..0x58], a.regs);
        }
        let stats = run_jit(&mut b, &mut bus_b);
        fills += stats.vm_fills;
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "round {round} flags");
        let (ma, mb) = (bus_a.ram.as_slice(), bus_b.ram.as_slice());
        if let Some(i) = (0..RAM).find(|&i| ma[i] != mb[i]) {
            panic!("round {round}: memory differs at {i:#x}: interp {:#04x} jit {:#04x}\ncode: {}", ma[i], mb[i], hex.join(" "));
        }
    }
    assert!(fills > 0, "the in-VM walk never ran");
}

#[test]
fn untranslatable_instruction_across_a_page_end() {
    let mut code = vec![0x90; 0xfff];
    code.extend([0x0f, 0xa2, 0x40, 0xf4]);
    compare(&code, |_| {});
}

fn random_mmx(rng: &mut Rng, out: &mut Vec<u8>) {
    const REG_OPS: [u8; 39] = [
        0xfc, 0xfd, 0xfe, 0xd4, 0xf8, 0xf9, 0xfa, 0xfb, 0xec, 0xed, 0xdc, 0xdd, 0xe8, 0xe9, 0xd8, 0xd9, 0x74, 0x75, 0x76, 0x64,
        0x65, 0x66, 0xd5, 0xe0, 0xe3, 0xda, 0xde, 0xea, 0xee, 0x60, 0x61, 0x62, 0x63, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0xe5,
    ];
    const MORE_OPS: [u8; 7] = [0xe4, 0xf5, 0xdb, 0xdf, 0xeb, 0xef, 0x6f];
    let mm = rng.below(8) as u8;
    match rng.below(6) {
        0 | 1 => {
            let all: Vec<u8> = REG_OPS.iter().chain(MORE_OPS.iter()).copied().collect();
            let op = all[rng.below(all.len() as u32) as usize];
            out.extend([0x0f, op, modrm_rr(mm, rng.below(8) as u8)]);
        }
        2 => {
            let all: Vec<u8> = REG_OPS.iter().chain(MORE_OPS.iter()).copied().collect();
            let op = all[rng.below(all.len() as u32) as usize];
            out.extend([0x0f, op, mm << 3 | 5]);
            out.extend((0x3000 + rng.below(8) * 8).to_le_bytes());
        }
        3 => match rng.below(4) {
            0 => out.extend([0x0f, 0x6e, modrm_rr(mm, rng.reg())]),
            1 => out.extend([0x0f, 0x7e, modrm_rr(mm, rng.reg())]),
            2 => {
                out.extend([0x0f, 0x7f, mm << 3 | 5]);
                out.extend((0x3000 + rng.below(8) * 8).to_le_bytes());
            }
            _ => out.extend([0x0f, 0x7f, modrm_rr(mm, rng.below(8) as u8)]),
        },
        _ => {
            let (op, sub) = match rng.below(3) {
                0 => (0x71, [2, 4, 6][rng.below(3) as usize]),
                1 => (0x72, [2, 4, 6][rng.below(3) as usize]),
                _ => (0x73, [2, 6][rng.below(2) as usize]),
            };
            out.extend([0x0f, op, modrm_rr(sub, mm), rng.below(70) as u8]);
        }
    }
}

#[test]
fn random_mmx_programs_match_the_interpreter() {
    let mut rng = Rng(0x3d_f00d);
    for round in 0..300 {
        let mut code = Vec::new();
        for _ in 0..30 {
            random_mmx(&mut rng, &mut code);
        }
        code.push(0xf4);
        let regs: Vec<u32> = (0..8).map(|_| rng.next()).collect();
        let mms: Vec<u64> = (0..8).map(|_| (rng.next() as u64) << 32 | rng.next() as u64).collect();
        let mem: Vec<u8> = (0..64).map(|_| rng.next() as u8).collect();
        let mut a = flat_cpu(0x1000, 0x8000);
        for (i, v) in regs.iter().enumerate() {
            if i != 4 {
                a.regs[i] = *v;
            }
        }
        for (i, v) in mms.iter().enumerate() {
            a.fpu.set_mm(i, *v);
        }
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        bus_a.ram.as_mut_slice()[0x3000..0x3040].copy_from_slice(&mem);
        bus_b.ram.as_mut_slice()[0x3000..0x3040].copy_from_slice(&mem);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        run_interp(&mut a, &mut bus_a);
        let stats = run_jit(&mut b, &mut bus_b);
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.fpu, b.fpu, "round {round} MMX state\ncode: {}", hex.join(" "));
        assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "round {round} memory\ncode: {}", hex.join(" "));
        assert!(stats.interp_exits <= 1, "round {round}: {} interpreter exits\ncode: {}", stats.interp_exits, hex.join(" "));
    }
}

/// A ModRM byte and disp32 for `[disp32]` with the given reg field.
fn mem_abs(out: &mut Vec<u8>, op: u8, reg: u8, addr: u32) {
    out.extend([op, reg << 3 | 5]);
    out.extend(addr.to_le_bytes());
}

fn random_x87(rng: &mut Rng, out: &mut Vec<u8>) {
    let i = rng.below(8) as u8;
    let k = rng.below(8);
    match rng.below(27) {
        24 => mem_abs(out, 0xdb, [5, 7][rng.below(2) as usize], 0x3540 + 10 * k),
        25 => out.extend([0xdd, 0xc0 | i]),
        0 => mem_abs(out, 0xdd, 0, 0x3000 + 8 * k),
        1 => mem_abs(out, 0xd9, 0, 0x3100 + 4 * k),
        2 => mem_abs(out, 0xdb, 0, 0x3200 + 4 * k),
        3 => mem_abs(out, [0xdf, 0xdf][k as usize % 2], [0, 5][k as usize % 2], 0x3300 + 8 * k),
        4 | 5 => {
            let op = [0xd8, 0xdc, 0xde][rng.below(3) as usize];
            let reg = [0, 1, 4, 5, 6, 7][rng.below(6) as usize];
            out.extend([op, 0xc0 | reg << 3 | i]);
        }
        6 => mem_abs(out, [0xdc, 0xd8, 0xda][rng.below(3) as usize], rng.below(8) as u8, 0x3000 + 8 * k),
        7 => out.extend([0xd9, 0xc8 | i]),
        8 => out.extend([0xd9, 0xc0 | i]),
        9 => out.extend([0xdd, [0xd0, 0xd8][rng.below(2) as usize] | i]),
        10 => out.extend([0xd9, [0xe0, 0xe1, 0xee, 0xe8, 0xfa, 0xfc, 0xe4, 0xe5, 0xf8][rng.below(9) as usize]]),
        11 => out.extend([0xd8, 0xd0 | i]),
        12 => out.extend([0xd8, 0xd8 | i]),
        13 => out.extend([[0xde, 0xd9], [0xda, 0xe9]][rng.below(2) as usize]),
        14 => out.extend([[0xdb, 0xdf][rng.below(2) as usize], [0xe8, 0xf0][rng.below(2) as usize] | i]),
        15 => out.extend([0xdf, 0xe0]),
        16 => out.extend([0xdd, 0xe0 | i]),
        17 => mem_abs(out, 0xdd, [2, 3][rng.below(2) as usize], 0x3400 + 8 * k),
        18 => mem_abs(out, 0xd9, [2, 3][rng.below(2) as usize], 0x3480 + 4 * k),
        19 => {
            let (op, reg) = [(0xdb, 2), (0xdb, 3), (0xdf, 2), (0xdf, 3), (0xdf, 7)][rng.below(5) as usize];
            mem_abs(out, op, reg, 0x34c0 + 8 * k);
        }
        20 => mem_abs(out, 0xd9, 5, 0x3500 + 2 * (k % 6)),
        21 => mem_abs(out, [0xd9, 0xdd][rng.below(2) as usize], 7, 0x3520 + 2 * k),
        22 => match rng.below(5) {
            0 => out.extend([0x0f, 0x6f, modrm_rr(rng.below(8) as u8, rng.below(8) as u8)]),
            1 => out.extend([0x0f, 0x77]),
            2 => out.extend([0xdb, 0xe2]),
            _ => out.push(0x9b),
        },
        _ => mem_abs(out, [0xd8, 0xdc][rng.below(2) as usize], [2, 3][rng.below(2) as usize], 0x3100 + 4 * k),
    }
}

fn x87_memory(rng: &mut Rng) -> Vec<u8> {
    let mut mem = vec![0u8; 0x600];
    let pick = |rng: &mut Rng| -> f64 {
        match rng.below(10) {
            0 => 0.0,
            1 => -0.0,
            2 => 1.0e300,
            3 => 1.0 / 3.0,
            4 => (rng.next() as i32) as f64 * 1.0e-6,
            _ => (rng.below(2001) as f64 - 1000.0) / 8.0,
        }
    };
    for k in 0..8 {
        let d = pick(rng);
        mem[8 * k..8 * k + 8].copy_from_slice(&d.to_le_bytes());
        let f = pick(rng) as f32;
        mem[0x100 + 4 * k..0x104 + 4 * k].copy_from_slice(&f.to_le_bytes());
        let n = rng.next() as i32 >> rng.below(31);
        mem[0x200 + 4 * k..0x204 + 4 * k].copy_from_slice(&n.to_le_bytes());
        let q = ((rng.next() as u64) << 32 | rng.next() as u64) as i64 >> rng.below(63);
        mem[0x300 + 8 * k..0x308 + 8 * k].copy_from_slice(&q.to_le_bytes());
        let low = if rng.below(2) == 0 { 0 } else { rng.next() as u64 & 0x7ff };
        let mant = (1u64 << 63) | (rng.next() as u64) << 11 | low;
        let se = (0x3fff - 30 + rng.below(60)) as u16 | (rng.below(2) as u16) << 15;
        mem[0x540 + 10 * k..0x548 + 10 * k].copy_from_slice(&mant.to_le_bytes());
        mem[0x548 + 10 * k..0x54a + 10 * k].copy_from_slice(&se.to_le_bytes());
    }
    for (j, cw) in [0x37fu16, 0x27f, 0xf7f, 0x77f, 0xb7f, 0x07f].iter().enumerate() {
        mem[0x500 + 2 * j..0x502 + 2 * j].copy_from_slice(&cw.to_le_bytes());
    }
    mem
}

#[test]
fn fninit_and_emms_match_the_interpreter() {
    let code = [
        0xd9, 0xe8, // fld1
        0xd9, 0x05, 0x00, 0x31, 0, 0, // fld dword [0x3100]
        0xdb, 0xe3, // fninit
        0xd9, 0xeb, // fldpi
        0xd9, 0x35, 0x00, 0x30, 0, 0, // fnstenv [0x3000]
        0x0f, 0x6e, 0xc0, // movd mm0, eax
        0x0f, 0x77, // emms
        0xd9, 0x35, 0x40, 0x30, 0, 0, // fnstenv [0x3040]
        0x0f, 0x6e, 0xc8, // movd mm1, eax
        0xdb, 0xe3, // fninit
        0xd9, 0x35, 0x80, 0x30, 0, 0, // fnstenv [0x3080]
        0xd9, 0xe8, // fld1
        0xdd, 0x1d, 0xc0, 0x30, 0, 0, // fstp qword [0x30c0]
        0xf4, // hlt
    ];
    let stats = compare(&code, |c| {
        c.fpu.cw = 0x27f;
        (c.fpu.fip, c.fpu.fop, c.fpu.fdp) = (0x6666, 0x123, 0x5555);
    });
    assert!(stats.interp_exits <= 4, "FNINIT and EMMS should stay translated: {stats:?}");
}

#[test]
fn x87_sine_cosine_and_tangent_match_the_interpreter() {
    let mut exits = 0;
    let mut runs = 0;
    for &x in &[0.5f64, -0.5, 1.0, 3.0, -7.25, 100.0, 1.0e5, -1.0e6, 1.0e-9, 0.0, -0.0, 1.0e7, 1.5707963267948966, 3.141592653589793, 1.0e-30] {
        for op in [0xfe, 0xff, 0xf2, 0xfb] {
            let code = [
                0xdd, 0x05, 0x00, 0x30, 0, 0, // fld qword [0x3000]
                0xd9, op, // the instruction under test
                0xdd, 0x1d, 0x08, 0x30, 0, 0, // fstp qword [0x3008]
                0xdd, 0x1d, 0x10, 0x30, 0, 0, // fstp qword [0x3010]
                0xdf, 0xe0, // fnstsw ax
                0xf4, // hlt
            ];
            let mut a = flat_cpu(0x1000, 0x8000);
            let mut b = a.clone();
            let mut bus_a = bus_with(0x1000, &code);
            let mut bus_b = bus_with(0x1000, &code);
            bus_a.ram.as_mut_slice()[0x3000..0x3008].copy_from_slice(&x.to_le_bytes());
            bus_b.ram.as_mut_slice()[0x3000..0x3008].copy_from_slice(&x.to_le_bytes());
            run_interp(&mut a, &mut bus_a);
            let stats = run_jit(&mut b, &mut bus_b);
            assert_eq!(a.regs, b.regs, "x {x:e} op {op:#x}: registers (FSW in AX)");
            assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "x {x:e} op {op:#x}: results differ");
            exits += stats.interp_exits;
            runs += 1;
        }
    }
    assert!(exits < runs * 2 - 20, "{exits} interpreter exits in {runs} runs: in-range inputs should stay translated");
}

#[test]
fn random_x87_programs_match_the_interpreter() {
    let mut rng = Rng(0x87_87);
    let (mut exits, mut insns) = (0, 0);
    for round in 0..400 {
        let mut code = Vec::new();
        for k in 0..4 {
            mem_abs(&mut code, 0xdd, 0, 0x3000 + 8 * k);
        }
        let n = 30;
        for _ in 0..n {
            random_x87(&mut rng, &mut code);
            if rng.below(4) == 0 {
                code.extend([0xeb, 0x00]);
            }
        }
        code.push(0xf4);
        let mem = x87_memory(&mut rng);
        let mut a = flat_cpu(0x1000, 0x8000);
        a.fpu.cw = [0x37f, 0x27f, 0x07f, 0xc7f, 0x77f, 0xb7f][round % 6];
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        bus_a.ram.as_mut_slice()[0x3000..0x3600].copy_from_slice(&mem);
        bus_b.ram.as_mut_slice()[0x3000..0x3600].copy_from_slice(&mem);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        run_interp(&mut a, &mut bus_a);
        let stats = run_jit(&mut b, &mut bus_b);
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "round {round} flags\ncode: {}", hex.join(" "));
        assert_eq!(a.fpu, b.fpu, "round {round} x87 state\ncode: {}", hex.join(" "));
        assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "round {round} memory\ncode: {}", hex.join(" "));
        exits += stats.interp_exits;
        insns += n + 4;
    }
    assert!(exits * 4 < insns, "{exits} interpreter exits for {insns} x87 instructions");
}

/// An x87 instruction tier 2 compiles that keeps the stack between 1 and 8
/// deep: `depth` is how many registers are full.
fn random_tier2_x87(rng: &mut Rng, out: &mut Vec<u8>, depth: &mut u32) {
    let k = rng.below(8);
    let i = rng.below((*depth).max(1)) as u8;
    let push = *depth < 8;
    let pop = *depth > 1;
    match rng.below(14) {
        0 if push => {
            let (op, base) = [(0xdd, 0x3000), (0xd9, 0x3100), (0xdb, 0x3200)][rng.below(3) as usize];
            mem_abs(out, op, 0, base + if op == 0xdd { 8 * k } else { 4 * k });
            *depth += 1;
        }
        1 if push => {
            out.extend([0xd9, 0xc0 | i]);
            *depth += 1;
        }
        2 if push => {
            out.extend([0xd9, [0xe8, 0xee][rng.below(2) as usize]]);
            *depth += 1;
        }
        3 => {
            let p = pop && rng.below(2) == 0;
            mem_abs(out, [0xd9, 0xdd][rng.below(2) as usize], 2 + p as u8, 0x3400 + 8 * k);
            *depth -= p as u32;
        }
        4 if pop => {
            mem_abs(out, 0xdb, 3, 0x3480 + 4 * k);
            *depth -= 1;
        }
        5 => out.extend([0xd9, 0xc8 | i]),
        6 => out.extend([0xd9, [0xe0, 0xe1][rng.below(2) as usize]]),
        7 => {
            let reg = [0, 1, 4, 5][rng.below(4) as usize];
            let op = [0xd8, 0xdc][rng.below(2) as usize];
            out.extend([op, 0xc0 | reg << 3 | i]);
        }
        8 if pop && i > 0 => {
            let reg = [0, 1, 4, 5][rng.below(4) as usize];
            out.extend([0xde, 0xc0 | reg << 3 | i]);
            *depth -= 1;
        }
        9 => {
            let reg = [0, 1, 4, 5][rng.below(4) as usize];
            mem_abs(out, [0xd8, 0xdc][rng.below(2) as usize], reg, if rng.below(2) == 0 { 0x3100 + 4 * k } else { 0x3000 + 8 * k });
        }
        10 => out.extend([0xd8, 0xd0 | i]),
        11 if pop => {
            out.extend([0xdb, 0xf0 | i]);
        }
        12 => out.extend([0xdf, 0xe0]),
        _ => mem_abs(out, 0xd8, 2, 0x3100 + 4 * k),
    }
}

/// Tier 2 keeps neither PE nor C1, FIP, FDP and FOP, nor what empty
/// registers hold, and computes overflows and NaNs as plain doubles.
#[test]
fn random_x87_programs_match_the_interpreter_in_tier_2() {
    TIER2.with(|t| t.set(true));
    let mut rng = Rng(0x87_87);
    let mut compared = 0;
    for round in 0..400 {
        let mut code = Vec::new();
        for k in 0..4 {
            mem_abs(&mut code, 0xdd, 0, 0x3000 + 8 * k);
        }
        let mut depth = 4;
        for _ in 0..30 {
            random_tier2_x87(&mut rng, &mut code, &mut depth);
            if rng.below(4) == 0 {
                code.extend([0xeb, 0x00]);
            }
        }
        code.push(0xf4);
        let mem = x87_memory(&mut rng);
        let mut a = flat_cpu(0x1000, 0x8000);
        a.fpu.cw = [0x37f, 0x27f, 0x07f, 0x77f, 0xb7f, 0xf7f, 0x47f][round % 7];
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        bus_a.ram.as_mut_slice()[0x3000..0x3600].copy_from_slice(&mem);
        bus_b.ram.as_mut_slice()[0x3000..0x3600].copy_from_slice(&mem);
        run_interp(&mut a, &mut bus_a);
        let clears_exceptions = code.windows(2).any(|w| w == [0xdb, 0xe2]);
        if a.fpu.sw & 0x1f != 0 || clears_exceptions {
            continue;
        }
        run_jit(&mut b, &mut bus_b);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        let relaxed = 0x220;
        let regs = |c: &Cpu| { let mut r = c.regs; r[0] &= !relaxed; r };
        assert_eq!(regs(&a), regs(&b), "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "round {round} flags\ncode: {}", hex.join(" "));
        assert_eq!((a.fpu.cw, a.fpu.sw & !relaxed as u16, a.fpu.tags), (b.fpu.cw, b.fpu.sw & !relaxed as u16, b.fpu.tags), "round {round} x87 control\ncode: {}", hex.join(" "));
        for p in 0..8 {
            if a.fpu.tags & 1 << p != 0 {
                assert_eq!(a.fpu.regs[p], b.fpu.regs[p], "round {round} R{p}\ncode: {}", hex.join(" "));
            }
        }
        let (ma, mb) = (bus_a.ram.as_slice(), bus_b.ram.as_slice());
        if let Some(i) = (0..RAM).find(|&i| ma[i] != mb[i] && !(0x3520..0x3540).contains(&i)) {
            panic!("round {round} memory at {i:#x}\ncode: {}", hex.join(" "));
        }
        compared += 1;
    }
    assert!(compared > 100, "only {compared} rounds stayed clear of exceptions");
}

fn sse_op(out: &mut Vec<u8>, pfx: u8, op: u8, reg: u8, rm: Option<u8>, addr: u32) {
    if pfx != 0 {
        out.push(pfx);
    }
    out.extend([0x0f, op]);
    match rm {
        Some(rm) => out.push(modrm_rr(reg, rm)),
        None => {
            out.push(reg << 3 | 5);
            out.extend(addr.to_le_bytes());
        }
    }
}

fn random_sse(rng: &mut Rng, out: &mut Vec<u8>) {
    let reg = rng.below(8) as u8;
    let rm = if rng.below(3) == 0 { None } else { Some(rng.below(8) as u8) };
    let slot = 0x3000 + 16 * rng.below(16);
    let any_pfx = [0, 0x66, 0xf3, 0xf2][rng.below(4) as usize];
    let packed_pfx = [0, 0x66][rng.below(2) as usize];
    const INT_OPS: [u8; 46] = [
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x74, 0x75, 0x76, 0xd4, 0xd5, 0xd8, 0xd9,
        0xda, 0xdc, 0xdd, 0xde, 0xe0, 0xe3, 0xe4, 0xe5, 0xe8, 0xe9, 0xea, 0xec, 0xed, 0xee, 0xf4, 0xf5, 0xf6, 0xf8, 0xf9, 0xfa, 0xfb,
        0xfc, 0xfd, 0xfe, 0xdb,
    ];
    const SHIFT_OPS: [u8; 8] = [0xd1, 0xd2, 0xd3, 0xe1, 0xe2, 0xf1, 0xf2, 0xf3];
    match rng.below(28) {
        27 => sse_op(out, packed_pfx, 0x50, rng.reg(), Some(reg), 0),
        24 => sse_op(out, 0x66, SHIFT_OPS[rng.below(8) as usize], reg, rm, slot),
        25 => {
            out.extend([0xc7, 0x05, 0x80, 0x31, 0, 0]);
            out.extend(rng.below(70).to_le_bytes());
            out.extend([0xc7, 0x05, 0x84, 0x31, 0, 0, 0, 0, 0, 0]);
            sse_op(out, 0x66, SHIFT_OPS[rng.below(8) as usize], reg, None, 0x3180);
        }
        26 => {
            out.extend([0x0f, 0xae, 3 << 3 | 5]);
            out.extend((0x31c0 + 4 * rng.below(4)).to_le_bytes());
        }
        20 | 21 => sse_op(out, 0x66, INT_OPS[rng.below(INT_OPS.len() as u32) as usize], reg, rm, slot),
        22 => {
            let (op, sub) = [(0x71, 2), (0x71, 4), (0x71, 6), (0x72, 2), (0x72, 4), (0x72, 6), (0x73, 2), (0x73, 3), (0x73, 6), (0x73, 7)]
                [rng.below(10) as usize];
            out.extend([0x66, 0x0f, op, modrm_rr(sub, reg), rng.below(70) as u8]);
        }
        23 => match rng.below(3) {
            0 => {
                sse_op(out, 0x66, 0xc4, reg, rm.map(|_| rng.reg()), slot + 2 * rng.below(8));
                out.push(rng.next() as u8);
            }
            1 => out.extend([0x66, 0x0f, 0xc5, modrm_rr(rng.reg(), reg), rng.next() as u8]),
            _ => out.extend([0x66, 0x0f, 0xd7, modrm_rr(rng.reg(), reg)]),
        },
        0 => sse_op(out, packed_pfx, [0x10, 0x11, 0x28, 0x29][rng.below(4) as usize], reg, rm, slot),
        1 => sse_op(out, [0xf3, 0xf2][rng.below(2) as usize], [0x10, 0x11][rng.below(2) as usize], reg, rm, slot + 4 * rng.below(3)),
        2 => {
            let op = [0x12, 0x13, 0x16, 0x17][rng.below(4) as usize];
            let rm = if op & 1 == 1 || packed_pfx == 0x66 { None } else { rm };
            sse_op(out, packed_pfx, op, reg, rm, slot + 8 * rng.below(2));
        }
        3 => sse_op(out, packed_pfx, [0x14, 0x15, 0x54, 0x55, 0x56, 0x57][rng.below(6) as usize], reg, rm, slot),
        4 => sse_op(out, 0x66, [0xdb, 0xdf, 0xeb, 0xef][rng.below(4) as usize], reg, rm, slot),
        5 => {
            if rng.below(2) == 0 {
                sse_op(out, packed_pfx, 0xc6, reg, rm, slot);
            } else {
                sse_op(out, 0x66, 0x70, reg, rm, slot);
            }
            out.push(rng.next() as u8);
        }
        6 => match rng.below(4) {
            0 => sse_op(out, 0x66, 0x6e, reg, rm.map(|_| rng.reg()), slot + 4 * rng.below(4)),
            1 => sse_op(out, 0x66, 0x7e, reg, rm.map(|_| rng.reg()), slot + 4 * rng.below(4)),
            2 => sse_op(out, 0xf3, 0x7e, reg, rm, slot + 8 * rng.below(2)),
            _ => sse_op(out, 0x66, 0xd6, reg, rm, slot + 8 * rng.below(2)),
        },
        7 => sse_op(out, [0x66, 0xf3][rng.below(2) as usize], [0x6f, 0x7f][rng.below(2) as usize], reg, rm, slot),
        8..=11 => sse_op(out, any_pfx, [0x58, 0x59, 0x5c, 0x5e, 0x5d, 0x5f, 0x51][rng.below(7) as usize], reg, rm, slot),
        12 | 13 => {
            sse_op(out, any_pfx, 0xc2, reg, rm, slot);
            out.push(rng.below(8) as u8);
        }
        14 => sse_op(out, packed_pfx, [0x2e, 0x2f][rng.below(2) as usize], reg, rm, slot),
        15 => sse_op(out, [0xf3, 0xf2][rng.below(2) as usize], 0x2a, reg, rm.map(|_| rng.reg()), slot + 4 * rng.below(4)),
        16 => sse_op(out, [0xf3, 0xf2][rng.below(2) as usize], [0x2c, 0x2d][rng.below(2) as usize], rng.reg(), rm, slot),
        17 => sse_op(out, [0xf3, 0xf2][rng.below(2) as usize], 0x5a, reg, rm, slot),
        18 => {
            out.extend([0x0f, 0xae, 2 << 3 | 5]);
            out.extend((0x3100 + 4 * rng.below(6)).to_le_bytes());
        }
        _ => sse_op(out, any_pfx, [0x58, 0x59][rng.below(2) as usize], reg, rm, slot),
    }
}

fn sse_value(rng: &mut Rng) -> u32 {
    match rng.below(12) {
        0 => 0,
        1 => 0x8000_0000,
        2 => 0x7f80_0000,
        3 => 0xff80_0000,
        4 => 0x7fc0_0000 | rng.below(0x1000),
        5 => 0x7f80_0001 | rng.below(0x1000),
        6 => rng.below(0x80_0000) | (rng.below(2) << 31),
        7 => rng.next(),
        _ => (rng.next() & 0x87ff_ffff) | 0x3800_0000,
    }
}

#[test]
fn random_sse_programs_match_the_interpreter() {
    let mut rng = Rng(0x55e2);
    let (mut exits, mut insns) = (0, 0);
    for round in 0..400 {
        let mut code = Vec::new();
        let n = 30;
        for _ in 0..n {
            random_sse(&mut rng, &mut code);
        }
        code.push(0xf4);
        let mut mem = vec![0u8; 0x200];
        for k in 0..64 {
            let v = if rng.below(3) == 0 {
                (rng.next() as u64) << 32 | rng.next() as u64
            } else {
                sse_value(&mut rng) as u64
            };
            mem[4 * k..4 * k + 4].copy_from_slice(&(v as u32).to_le_bytes());
        }
        for (j, csr) in [0x1f80u32, 0x3f80, 0x5f80, 0x7f80, 0x9f80, 0x1fc0].iter().enumerate() {
            mem[0x100 + 4 * j..0x104 + 4 * j].copy_from_slice(&csr.to_le_bytes());
        }
        let mut a = flat_cpu(0x1000, 0x8000);
        a.cr4 |= x86::cpu::cr4::OSFXSR;
        for i in 0..8 {
            a.xmm[i] = (0..4).fold(0u128, |v, k| v | (sse_value(&mut rng) as u128) << (32 * k));
            if i != 4 {
                a.regs[i] = rng.next() >> rng.below(32);
            }
        }
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        bus_a.ram.as_mut_slice()[0x3000..0x3200].copy_from_slice(&mem);
        bus_b.ram.as_mut_slice()[0x3000..0x3200].copy_from_slice(&mem);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        run_interp(&mut a, &mut bus_a);
        let stats = run_jit(&mut b, &mut bus_b);
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
        assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK, "round {round} flags\ncode: {}", hex.join(" "));
        for i in 0..8 {
            assert_eq!(a.xmm[i], b.xmm[i], "round {round} xmm{i}\ncode: {}", hex.join(" "));
        }
        let de = x86::fpu::exc::DE as u32;
        assert_eq!(a.mxcsr | de, b.mxcsr | de, "round {round} mxcsr\ncode: {}", hex.join(" "));
        for ram in [bus_a.ram.as_mut_slice(), bus_b.ram.as_mut_slice()] {
            for k in 0..4 {
                ram[0x31c0 + 4 * k] |= de as u8;
            }
        }
        assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "round {round} memory\ncode: {}", hex.join(" "));
        exits += stats.interp_exits;
        insns += n;
    }
    assert!(exits * 3 < insns, "{exits} interpreter exits for {insns} SSE instructions");
}

#[test]
fn mixed_x87_and_sse_keep_their_own_flags_and_rounding() {
    let mut rng = Rng(0x8787_55e2);
    for round in 0..300 {
        let mut code = Vec::new();
        for k in 0..4 {
            mem_abs(&mut code, 0xdd, 0, 0x3000 + 8 * k);
        }
        for _ in 0..24 {
            if rng.below(2) == 0 {
                random_x87(&mut rng, &mut code);
            } else {
                random_sse(&mut rng, &mut code);
            }
        }
        code.push(0xf4);
        let mut mem = x87_memory(&mut rng);
        for (j, csr) in [0x1f80u32, 0x3f80, 0x5f80, 0x7f80, 0x9f80, 0x1fc0].iter().enumerate() {
            mem[0x100 + 4 * j..0x104 + 4 * j].copy_from_slice(&csr.to_le_bytes());
        }
        let mut a = flat_cpu(0x1000, 0x8000);
        a.cr4 |= x86::cpu::cr4::OSFXSR;
        a.fpu.cw = [0x37f, 0x27f, 0x07f, 0xc7f, 0x77f, 0xb7f][round % 6];
        a.mxcsr = [0x1f80, 0x3f80, 0x5f80, 0x7f80][round % 4];
        for i in 0..8 {
            a.xmm[i] = (0..4).fold(0u128, |v, k| v | (sse_value(&mut rng) as u128) << (32 * k));
        }
        let mut b = a.clone();
        let mut bus_a = bus_with(0x1000, &code);
        let mut bus_b = bus_with(0x1000, &code);
        bus_a.ram.as_mut_slice()[0x3000..0x3600].copy_from_slice(&mem);
        bus_b.ram.as_mut_slice()[0x3000..0x3600].copy_from_slice(&mem);
        let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
        run_interp(&mut a, &mut bus_a);
        run_jit(&mut b, &mut bus_b);
        let de = x86::fpu::exc::DE as u32;
        assert_eq!(a.mxcsr | de, b.mxcsr | de, "round {round} mxcsr\ncode: {}", hex.join(" "));
        assert_eq!(a.fpu, b.fpu, "round {round} x87 state\ncode: {}", hex.join(" "));
        for i in 0..8 {
            assert_eq!(a.xmm[i], b.xmm[i], "round {round} xmm{i}\ncode: {}", hex.join(" "));
        }
        assert_eq!(a.regs, b.regs, "round {round} registers\ncode: {}", hex.join(" "));
    }
}

#[test]
fn sysenter_sysexit_and_fs_loads_match_the_interpreter() {
    let user = [
        0x40, // inc eax
        0x89, 0xe2, // mov edx, esp
        0x0f, 0x34, // sysenter
        0x50, // push eax
        0x0f, 0xa2, // cpuid: an exit back to the host with FS = 0x3b
        0x58, // pop eax
        0x64, 0x8b, 0x1d, 0, 0, 0, 0, // mov ebx, fs:[0]
        0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // keeps the interpreter out of the kernel
        0xeb, 0xe0, // jmp 0x1000
    ];
    let kernel = [
        0xfe, 0x05, 0x3c, 0x40, 0, 0, // inc byte [0x403c]: the 0x3b descriptor's base moves by 64 KiB
        0x89, 0xd5, // mov ebp, edx
        0x6a, 0x30, // push 0x30
        0x0f, 0xa1, // pop fs
        0x64, 0x8b, 0x0d, 0, 0, 0, 0, // mov ecx, fs:[0]
        0x01, 0xce, // add esi, ecx
        0xb9, 0x23, 0, 0, 0, // mov ecx, 0x23
        0x8e, 0xd9, // mov ds, cx
        0x8e, 0xc1, // mov es, cx
        0x0f, 0x20, 0xd7, // mov edi, cr2
        0x83, 0xf8, 0x10, // cmp eax, 16: later rounds run with no exits in between
        0x74, 0x0e, // je the final pop fs
        0x6a, 0x3b, // push 0x3b
        0x0f, 0xa1, // pop fs
        0xba, 0x05, 0x10, 0, 0, // mov edx, 0x1005
        0x89, 0xe9, // mov ecx, ebp
        0xfb, // sti
        0x0f, 0x35, // sysexit
        0x6a, 0x3b, // push 0x3b
        0x0f, 0xa1, // pop fs
        0xf4, // hlt
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut bus_a = bus_with(0x1000, &user);
    let mem = bus_a.ram.as_mut_slice();
    mem[0x2000..0x2000 + kernel.len()].copy_from_slice(&kernel);
    let gdt: [u64; 8] = [
        0,
        0x00cf_9b00_0000_ffff,
        0x00cf_9300_0000_ffff,
        0x00cf_fb00_0000_ffff,
        0x00cf_f300_0000_ffff,
        0,
        0x0040_9200_5000_0fff,
        0x0040_f300_5100_0fff,
    ];
    for (i, d) in gdt.iter().enumerate() {
        mem[0x4000 + 8 * i..0x4008 + 8 * i].copy_from_slice(&d.to_le_bytes());
    }
    mem[0x5000..0x5004].copy_from_slice(&0x1111_1111u32.to_le_bytes());
    for k in 0..32usize {
        let at = 0x5100 + (k << 16);
        mem[at..at + 4].copy_from_slice(&(0x2222_0000 + k as u32).to_le_bytes());
    }
    paged(&mut a, &mut bus_a, false);
    a.gdtr = x86::cpu::DescriptorTable { base: 0x4000, limit: 0x3f };
    a.sysenter_cs = 8;
    a.sysenter_esp = 0x9000;
    a.sysenter_eip = 0x2000;
    for (seg, sel) in [(0, 0x23), (1, 0x1b), (2, 0x23), (3, 0x23)] {
        a.segs[seg].selector = sel;
        a.segs[seg].attr |= 3 << 5;
    }
    assert!(interp::load_segment(&mut a, &mut bus_a, 4, 0x3b));
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap(), gart: None };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!(a.eip, b.eip);
    assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK);
    assert_eq!(a.segs, b.segs);
    assert!(bus_a.ram.as_slice() == bus_b.ram.as_slice(), "memory differs, such as a descriptor's Accessed bit");
    assert!(stats.interp_exits <= 17, "system calls left translated code: {stats:?}");
}

#[test]
fn a_segment_keeps_its_cached_base_after_its_descriptor_changes() {
    let code = [
        0x66, 0xb8, 0x30, 0x00, // mov ax, 0x30
        0x8e, 0xe0, // mov fs, ax
        0xc7, 0x05, 0x34, 0x40, 0, 0, 0, 0, 0, 0, // mov dword [0x4034], 0: the descriptor is gone
        0x0f, 0xa2, // cpuid: an exit to the host
        0x64, 0x8b, 0x1d, 0, 0, 0, 0, // mov ebx, fs:[0]
        0xf4, // hlt
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut bus_a = bus_with(0x1000, &code);
    let mem = bus_a.ram.as_mut_slice();
    let gdt: [u64; 7] = [0, 0x00cf_9b00_0000_ffff, 0x00cf_9300_0000_ffff, 0, 0, 0, 0x0040_9200_5000_0fff];
    for (i, d) in gdt.iter().enumerate() {
        mem[0x4000 + 8 * i..0x4008 + 8 * i].copy_from_slice(&d.to_le_bytes());
    }
    mem[0x5000..0x5004].copy_from_slice(&0x1111_1111u32.to_le_bytes());
    a.gdtr = x86::cpu::DescriptorTable { base: 0x4000, limit: 0x37 };
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap(), gart: None };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs[3], 0x1111_1111, "the interpreter forgot the cached FS base");
    assert_eq!(b.regs, a.regs, "the JIT lost FS's cached base when its descriptor changed");
    assert_eq!(b.segs[4].base, 0x5000, "FS should keep the base the load cached");
}

fn ring_test_machine(user: &[u8], kernel: &[u8]) -> (Cpu, FlatBus) {
    let mut cpu = flat_cpu(0x2000, 0x9000);
    let mut bus = bus_with(0x1000, user);
    let mem = bus.ram.as_mut_slice();
    mem[0x2000..0x2000 + kernel.len()].copy_from_slice(kernel);
    let gdt: [u64; 8] = [
        0,
        0x00cf_9b00_0000_ffff,
        0x00cf_9300_0000_ffff,
        0x00cf_fb00_0000_ffff,
        0x00cf_f300_0000_ffff,
        0,
        0x0040_9300_5000_0fff,
        0x0040_f300_5100_0fff,
    ];
    for (i, d) in gdt.iter().enumerate() {
        mem[0x4000 + 8 * i..0x4008 + 8 * i].copy_from_slice(&d.to_le_bytes());
    }
    paged(&mut cpu, &mut bus, false);
    cpu.gdtr = x86::cpu::DescriptorTable { base: 0x4000, limit: 0x3f };
    cpu.sysenter_cs = 8;
    cpu.sysenter_esp = 0x9000;
    cpu.sysenter_eip = 0x2100;
    for (seg, sel) in [(0, 0x23), (2, 0x10), (3, 0x23)] {
        cpu.segs[seg].selector = sel;
    }
    cpu.segs[0].attr |= 3 << 5;
    cpu.segs[3].attr |= 3 << 5;
    assert!(interp::load_segment(&mut cpu, &mut bus, 4, 0x3b));
    assert!(interp::load_segment(&mut cpu, &mut bus, 5, 0));
    (cpu, bus)
}

#[test]
fn iret_within_the_kernel_and_to_user_mode_matches_the_interpreter() {
    let user = [
        0x40, // inc eax
        0x89, 0xe2, // mov edx, esp
        0x0f, 0x34, // sysenter
    ];
    let mut kernel = vec![
        0x9c, // pushfd
        0x6a, 0x08, // push 0x08
        0x68, 0x0b, 0x20, 0, 0, // push 0x200b
        0xcf, // iretd, to the next instruction at CPL 0
        0x90, 0x90, // padding to 0x200b
        0x6a, 0x23, // push 0x23
        0x68, 0x00, 0x80, 0, 0, // push 0x8000
        0x68, 0x02, 0x02, 0, 0, // push 0x202 (IF)
        0x6a, 0x1b, // push 0x1b
        0x68, 0x00, 0x10, 0, 0, // push 0x1000
        0xcf, // iretd, to user mode
    ];
    kernel.resize(0x100, 0x90);
    kernel.extend([
        0x83, 0xf8, 0x03, // cmp eax, 3
        0x75, 0x01, // jne over the hlt
        0xf4, // hlt
        0xbc, 0x00, 0x90, 0, 0, // mov esp, 0x9000
        0xe9, 0xfb, 0xfe, 0xff, 0xff, // jmp 0x200b
    ]);
    let (mut a, mut bus_a) = ring_test_machine(&user, &kernel);
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap(), gart: None };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs);
    assert_eq!(a.eip, b.eip);
    assert_eq!(a.eflags & ARITH_MASK, b.eflags & ARITH_MASK);
    assert_eq!(a.segs, b.segs);
    assert!(stats.interp_exits <= 1, "IRET left translated code: {stats:?}");
}

#[test]
fn x87_results_survive_a_store_into_the_running_code() {
    let code = [
        0xdd, 0x05, 0x00, 0x30, 0, 0, // fld qword [0x3000]
        0xdc, 0x05, 0x08, 0x30, 0, 0, // fadd qword [0x3008]
        0xd9, 0xc0, // fld st(0)
        0xde, 0xc9, // fmulp
        0xc6, 0x05, 0x1a, 0x10, 0, 0, 0x90, // mov byte [0x101a], 0x90: a store into this page
        0x90, 0xf4, // nop, hlt
    ];
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut bus_a = bus_with(0x1000, &code);
    bus_a.ram.as_mut_slice()[0x3000..0x3008].copy_from_slice(&1.5f64.to_le_bytes());
    bus_a.ram.as_mut_slice()[0x3008..0x3010].copy_from_slice(&2.25f64.to_le_bytes());
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap(), gart: None };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert!(stats.smc_invalidations > 0, "the store did not hit the running block: {stats:?}");
    assert_eq!(a.fpu, b.fpu);
}

#[test]
fn code_mapped_at_two_linear_addresses_pushes_each_return_address() {
    let code = [
        0xe8, 0xfb, 0x0f, 0x00, 0x00, // call 0x2000
        0x89, 0xc3, // mov ebx, eax
        0xe8, 0xf4, 0x3f, 0x00, 0x00, // call 0x5000
        0xf4,
    ];
    let thunk = [0xe8, 0x00, 0x00, 0x00, 0x00, 0x58, 0xc3]; // call next; pop eax; ret
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut bus_a = bus_with(0x1000, &code);
    bus_a.ram.as_mut_slice()[0x2000..0x2000 + thunk.len()].copy_from_slice(&thunk);
    paged(&mut a, &mut bus_a, false);
    bus_a.ram.as_mut_slice()[0x11014..0x11018].copy_from_slice(&0x2007u32.to_le_bytes());
    let mut b = a.clone();
    let mut bus_b = FlatBus { ram: GuestMemory::new(RAM).unwrap(), gart: None };
    bus_b.ram.as_mut_slice().copy_from_slice(bus_a.ram.as_slice());
    run_interp(&mut a, &mut bus_a);
    run_jit(&mut b, &mut bus_b);
    assert_eq!((a.regs[3], a.regs[0]), (0x2005, 0x5005));
    assert_eq!(b.regs, a.regs);
}

#[test]
fn rdtsc_reads_the_host_clock_without_leaving_translated_code() {
    let mut code = vec![
        0x0f, 0x31, // rdtsc
        0x89, 0xc6, // mov esi, eax
        0x89, 0xd7, // mov edi, edx
    ];
    code.extend([0x40; 16]); // inc eax, past the interpreter's tail
    code.extend([0x0f, 0x31, 0xf4]); // rdtsc; hlt
    let mut cpu = flat_cpu(0x1000, 0x8000);
    cpu.start_host_tsc();
    let mut bus = bus_with(0x1000, &code);
    let before = cpu.read_tsc();
    let stats = run_jit(&mut cpu, &mut bus);
    let after = cpu.read_tsc();
    let first = (cpu.regs[7] as u64) << 32 | cpu.regs[6] as u64;
    let second = (cpu.regs[2] as u64) << 32 | cpu.regs[0] as u64;
    assert!(before <= first && first <= second && second <= after, "the guest TSC must agree with the host clock and never run backwards: {before} {first} {second} {after}");
    assert_eq!(stats.interp_exits, 1, "Linux reads the TSC constantly for scheduling, so RDTSC must not exit: {stats:?}");
}

#[test]
fn framebuffer_writes_skip_the_host_and_are_reported_once() {
    let code = [
        0xbf, 0x00, 0x00, 0x00, 0xe0, // mov edi, 0xe0000000
        0xb8, 0x44, 0x33, 0x22, 0x11, // mov eax, 0x11223344
        0x89, 0x07, // mov [edi], eax
        0x89, 0x87, 0x00, 0x30, 0x00, 0x00, // mov [edi+0x3000], eax
        0x8b, 0x1f, // mov ebx, [edi]
        0xf4,
    ];
    let len = 1 << 20;
    let layout = std::alloc::Layout::from_size_align(len, 16 << 10).unwrap();
    let fb = unsafe { std::alloc::alloc_zeroed(layout) };
    let _free = scopeguard(move || unsafe { std::alloc::dealloc(fb, layout) });
    let mut cpu = flat_cpu(0x1000, 0x8000);
    let mut bus = bus_with(0x1000, &code);
    let mut reports = Vec::new();
    let stats = run_jit_with(
        &mut cpu,
        &mut bus,
        |j| unsafe { j.set_framebuffer(Some((0xe000_0000, fb, len))).unwrap() },
        |j| {
            reports.push(j.take_framebuffer_written().unwrap());
            reports.push(j.take_framebuffer_written().unwrap());
        },
    );
    let pixels = unsafe { std::slice::from_raw_parts(fb, len) };
    assert_eq!(&pixels[..4], &[0x44, 0x33, 0x22, 0x11]);
    assert_eq!(&pixels[0x3000..0x3004], &[0x44, 0x33, 0x22, 0x11]);
    assert_eq!(cpu.regs[3], 0x1122_3344);
    assert_eq!(stats.mmio_exits, 0, "framebuffer stores must not leave the VM, or drawing crawls: {stats:?}");
    assert_eq!(reports, [true, false], "the display must redraw after writes, and only then");
}

#[test]
fn a_block_compiled_at_the_end_of_the_code_cache_still_reaches_the_runtime() {
    let mut code = Vec::new();
    for _ in 0..31 {
        code.extend([0xd9, 0x03, 0xd9, 0x5b, 0x04]); // fld dword [ebx]; fstp dword [ebx+4]
    }
    code.extend([0xff, 0x43, 0x08, 0xf4]); // inc dword [ebx+8]; hlt
    let mut cpu = flat_cpu(0x1000, 0x8000);
    cpu.regs[3] = 0x3000;
    let mut bus = bus_with(0x1000, &code);
    run_jit_with(&mut cpu, &mut bus, |j| j.code_next = crate::layout::CODE_CACHE_END - 16, |_| {});
    assert_eq!(bus.ram.as_slice()[0x3008], 1, "a long block near the end of the cache must translate and run");
}

#[test]
fn shadowed_rom_chunks_are_read_without_exits() {
    let code = [
        0xa1, 0x00, 0x00, 0x0c, 0x00, // mov eax, [0xc0000]
        0x8b, 0x1d, 0x00, 0x40, 0x0c, 0x00, // mov ebx, [0xc4000]
        0x89, 0x0d, 0x10, 0x00, 0x0c, 0x00, // mov [0xc0010], ecx
        0x89, 0x0d, 0x00, 0x80, 0x0c, 0x00, // mov [0xc8000], ecx
        0xf4,
    ];
    let mut cpu = flat_cpu(0x1000, 0x8000);
    cpu.regs[1] = 0x5555_aaaa;
    let mut bus = bus_with(0x1000, &code);
    bus.ram.as_mut_slice()[0xc0000..0xc0004].copy_from_slice(&0x1122_3344u32.to_le_bytes());
    bus.ram.as_mut_slice()[0xc4000..0xc4004].copy_from_slice(&0x5566_7788u32.to_le_bytes());
    let mut chunks = [None; 16];
    chunks[0] = Some(false);
    chunks[2] = Some(true);
    let stats = run_jit_with(&mut cpu, &mut bus, |j| j.set_shadowed_rom(chunks).unwrap(), |_| {});
    assert_eq!((cpu.regs[0], cpu.regs[3]), (0x1122_3344, 0x5566_7788));
    assert_eq!(&bus.ram.as_slice()[0xc8000..0xc8004], &0x5555_aaaau32.to_le_bytes());
    assert_eq!(stats.mmio_exits, 2, "only the unmapped chunk and the write-protected one may reach the bus: {stats:?}");
}

const APIC: u32 = 0xfee0_0000;

struct ApicBus {
    flat: FlatBus,
    tpr: u8,
    threshold: u8,
    host_tpr_accesses: u32,
    other: Vec<(u32, Option<u32>)>,
}

impl Bus for ApicBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.flat.read8(addr)
    }

    fn write8(&mut self, addr: u32, value: u8) {
        self.flat.write8(addr, value)
    }

    fn read32(&mut self, addr: u32) -> u32 {
        match addr {
            0xfee0_0080 => {
                self.host_tpr_accesses += 1;
                self.tpr.into()
            }
            _ if addr >> 12 == APIC >> 12 => {
                self.other.push((addr, None));
                0x5005_0014
            }
            _ => u32::from_le_bytes([0, 1, 2, 3].map(|i| self.flat.read8(addr + i))),
        }
    }

    fn write32(&mut self, addr: u32, value: u32) {
        match addr {
            0xfee0_0080 => {
                self.host_tpr_accesses += 1;
                self.tpr = value as u8;
            }
            _ if addr >> 12 == APIC >> 12 => self.other.push((addr, Some(value))),
            _ => (0..4).for_each(|i| self.flat.write8(addr + i, (value >> (8 * i)) as u8)),
        }
    }

    fn io_read(&mut self, _port: u16, _size: u8) -> u32 {
        0xffff_ffff
    }

    fn io_write(&mut self, _port: u16, _size: u8, _value: u32) {}

    fn apic_page(&self) -> Option<u32> {
        Some(APIC)
    }

    fn apic_tpr(&self) -> u8 {
        self.tpr
    }

    fn set_apic_tpr(&mut self, tpr: u8) {
        self.tpr = tpr;
    }

    fn tpr_threshold(&self) -> u8 {
        self.threshold
    }
}

/// A paged CPU at 0x1000 with the APIC mapped at its physical address, as XP's HAL maps it.
fn apic_setup(code: &[u8]) -> (Cpu, ApicBus) {
    let mut cpu = flat_cpu(0x1000, 0x8000);
    let mut bus = ApicBus { flat: bus_with(0x1000, code), tpr: 0, threshold: 0, host_tpr_accesses: 0, other: Vec::new() };
    paged(&mut cpu, &mut bus.flat, false);
    let mem = bus.flat.ram.as_mut_slice();
    mem[0x10000 + 4 * (APIC >> 22) as usize..][..4].copy_from_slice(&0x12003u32.to_le_bytes());
    mem[0x12000 + 4 * (APIC >> 12 & 0x3ff) as usize..][..4].copy_from_slice(&(APIC | 0x13).to_le_bytes());
    cpu.regs[2] = 0;
    (cpu, bus)
}

fn run_apic(cpu: &mut Cpu, bus: &mut ApicBus) -> (Stats, usize) {
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus.flat.ram.as_ptr(), RAM).unwrap() };
    jit.vm_fill = true;
    let mut runs = 0;
    while !cpu.halted && runs < 1000 {
        runs += 1;
        jit.run(cpu, bus).unwrap();
    }
    assert!(cpu.halted, "JIT run did not reach HLT; eip {:#x}", cpu.eip);
    (jit.stats, runs)
}

#[test]
fn apic_tpr_accesses_stay_in_the_vm() {
    let code = [
        0xbe, 0x80, 0x00, 0xe0, 0xfe, // mov esi, 0xfee00080
        0xb9, 0xe8, 0x03, 0x00, 0x00, // mov ecx, 1000
        0x89, 0x0e, // loop: mov [esi], ecx
        0x8b, 0x1e, // mov ebx, [esi]
        0x01, 0xda, // add edx, ebx
        0x49, // dec ecx
        0x75, 0xf7, // jnz loop
        0x8b, 0x46, 0x80, // mov eax, [esi-0x80]
        0xc7, 0x06, 0x20, 0x00, 0x00, 0x00, // mov dword [esi], 0x20
        0xf4,
    ];
    let (mut cpu, mut bus) = apic_setup(&code);
    let (stats, _) = run_apic(&mut cpu, &mut bus);
    let sum: u32 = (1..=1000u32).map(|i| i & 0xff).sum();
    assert_eq!(cpu.regs[2], sum, "XP's IRQL would come back wrong");
    assert_eq!(bus.tpr, 0x20);
    assert_eq!(cpu.regs[0], 0x5005_0014);
    assert_eq!(bus.other, [(APIC, None)]);
    assert!(bus.host_tpr_accesses <= 1, "every IRQL change would cost a VM exit: {} host accesses, {stats:?}", bus.host_tpr_accesses);
    assert!(stats.tpr_fast >= 2000);
}

#[test]
fn lowering_the_tpr_below_a_held_back_interrupt_leaves_the_vm() {
    let code = [
        0xc7, 0x05, 0x80, 0x00, 0xe0, 0xfe, 0x30, 0x00, 0x00, 0x00, // mov dword [0xfee00080], 0x30
        0xc7, 0x05, 0x80, 0x00, 0xe0, 0xfe, 0x50, 0x00, 0x00, 0x00, // mov dword [0xfee00080], 0x50
        0xc7, 0x05, 0x80, 0x00, 0xe0, 0xfe, 0x20, 0x00, 0x00, 0x00, // mov dword [0xfee00080], 0x20
        0xeb, 0xfe, // jmp $
    ];
    let (mut cpu, mut bus) = apic_setup(&code);
    bus.threshold = 0x40;
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus.flat.ram.as_ptr(), RAM).unwrap() };
    let kicker = jit.kicker();
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        kicker.kick();
    });
    let start = std::time::Instant::now();
    let exit = jit.run(&mut cpu, &mut bus).unwrap();
    let took = start.elapsed();
    watchdog.join().unwrap();
    assert!(matches!(exit, RunExit::Kicked));
    assert!(took < std::time::Duration::from_secs(1), "an interrupt unmasked by a TPR write waited {took:?} for delivery");
    assert_eq!(bus.tpr, 0x20);
}

#[test]
fn decodes_the_loads_and_stores_translated_code_makes() {
    let mut a = a64::Asm::new();
    a.ldr(a64::Width::W, a64::r(22), a64::r(9), 0);
    a.str(a64::Width::W, a64::r(20), a64::r(9), 0);
    a.ldst(true, a64::Width::H, true, false, a64::r(3), a64::r(9), -2);
    a.ldst_reg(false, a64::Width::B, false, false, a64::r(4), a64::r(9), a64::r(10), a64::Extend::Uxtw, false);
    a.ldst_index(true, a64::Width::W, a64::r(5), a64::r(9), 4, true);
    a.ldr(a64::Width::X, a64::r(6), a64::r(9), 0);
    let got: Vec<_> = a.finish().into_iter().map(decode_access).collect();
    let acc = |load, size, sign, rt| Some(Access { load, size, sign, rt });
    assert_eq!(got, [acc(true, 4, false, 22), acc(false, 4, false, 20), acc(true, 2, true, 3), acc(false, 1, false, 4), None, None]);
}

/// Counts down ECX from 2,000,000, storing to a 64 KiB buffer and summing
/// into XMM0 on every pass.
const LONG_LOOP: &[u8] = &[
    0xb9, 0x80, 0x84, 0x1e, 0x00, // mov ecx, 2000000
    0x01, 0xc8, // add eax, ecx
    0x69, 0xd8, 0x03, 0x00, 0x00, 0x00, // imul ebx, eax, 3
    0x89, 0xca, // mov edx, ecx
    0x81, 0xe2, 0xff, 0x3f, 0x00, 0x00, // and edx, 0x3fff
    0x89, 0x1c, 0x95, 0x00, 0x00, 0x20, 0x00, // mov [edx*4 + 0x200000], ebx
    0x66, 0x0f, 0x6e, 0xc8, // movd xmm1, eax
    0x66, 0x0f, 0xfe, 0xc1, // paddd xmm0, xmm1
    0x49, // dec ecx
    0x75, 0xde, // jnz back to the add
    0xf4, // hlt
];

#[test]
fn a_snapshot_taken_mid_run_resumes_to_the_same_result() {
    use snapshot::{Reader, Snap, Writer};
    let mut cpu = flat_cpu(0x1000, 0x8000);
    let mut bus = bus_with(0x1000, LONG_LOOP);
    paged(&mut cpu, &mut bus, false);
    let mut snap: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut resumed = None;
    let reference = {
        let mut c = cpu.clone();
        let mut b = bus_with(0x1000, LONG_LOOP);
        paged(&mut c, &mut b, false);
        run_jit(&mut c, &mut b);
        (c, b.ram.as_slice().to_vec())
    };
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jit = unsafe { Jit::new(bus.ram.as_ptr(), RAM).unwrap() };
    let kicker = jit.kicker();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let kicks = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_micros(200));
            kicker.kick();
        }
    });
    let _join = scopeguard(move || {
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = kicks.join();
    });
    for _ in 0..100_000 {
        if cpu.halted {
            break;
        }
        if snap.is_none() && cpu.regs[1] != 0 && cpu.regs[1] < 1_500_000 {
            let mut w = Writer::new();
            cpu.save(&mut w);
            snap = Some((w.buf, bus.ram.as_slice().to_vec()));
            bus.ram.as_mut_slice().fill(0x55);
            cpu = flat_cpu(0, 0);
            let (state, ram) = snap.as_ref().unwrap();
            cpu.load(&mut Reader::new(state)).unwrap();
            bus.ram.as_mut_slice().copy_from_slice(ram);
            jit.forget_guest();
            resumed = Some(cpu.regs[1]);
        }
        jit.run(&mut cpu, &mut bus).unwrap();
    }
    assert!(cpu.halted, "the resumed run did not reach HLT");
    let at = resumed.expect("no kick landed in the middle of the loop");
    assert!(at > 0 && at < 2_000_000, "snapshot at ECX {at}");
    let (want, want_ram) = reference;
    assert_eq!(cpu.regs, want.regs);
    assert_eq!(cpu.xmm, want.xmm);
    assert_eq!(cpu.eflags & ARITH_MASK, want.eflags & ARITH_MASK);
    assert!(bus.ram.as_slice() == &want_ram[..], "memory differs");
}

const GATT: u32 = 0x20000;

/// `paged`, plus linear 0x800000.. mapped onto the aperture, whose GATT maps
/// page 0 to 0x30000, page 1 to 0x31000 and leaves page 2 invalid.
fn paged_with_aperture(cpu: &mut Cpu, bus: &mut FlatBus) {
    paged(cpu, bus, false);
    bus.gart = Some(Gart { gatt: GATT, epoch: 0 });
    let mem = bus.ram.as_mut_slice();
    mem[0x10008..0x1000c].copy_from_slice(&0x12007u32.to_le_bytes());
    for i in 0..1024u32 {
        let pte = (APERTURE + (i << 12)) | 7;
        let at = 0x12000 + i as usize * 4;
        mem[at..at + 4].copy_from_slice(&pte.to_le_bytes());
    }
    for (i, entry) in [0x30001u32, 0x31001].into_iter().enumerate() {
        let at = GATT as usize + i * 4;
        mem[at..at + 4].copy_from_slice(&entry.to_le_bytes());
    }
}

fn compare_with_aperture(code: &[u8], setup: impl Fn(&mut FlatBus)) -> (Stats, FlatBus, Cpu) {
    let mut a = flat_cpu(0x1000, 0x8000);
    let mut b = a.clone();
    let mut bus_a = bus_with(0x1000, code);
    let mut bus_b = bus_with(0x1000, code);
    paged_with_aperture(&mut a, &mut bus_a);
    paged_with_aperture(&mut b, &mut bus_b);
    setup(&mut bus_a);
    setup(&mut bus_b);
    run_interp(&mut a, &mut bus_a);
    let stats = run_jit(&mut b, &mut bus_b);
    assert_eq!(a.regs, b.regs, "registers");
    let (ma, mb) = (bus_a.ram.as_slice(), bus_b.ram.as_slice());
    if let Some(i) = (0..RAM).find(|&i| ma[i] != mb[i]) {
        panic!("memory differs at {i:#x}: interp {:#04x} jit {:#04x}", ma[i], mb[i]);
    }
    (stats, bus_b, b)
}

#[test]
fn aperture_accesses_reach_the_gatt_pages_without_exits() {
    let code = [
        0xbf, 0x00, 0x00, 0x80, 0x00, // mov edi, 0x800000
        0xb8, 0x44, 0x33, 0x22, 0x11, // mov eax, 0x11223344
        0x89, 0x07, // mov [edi], eax
        0x89, 0x87, 0x04, 0x10, 0x00, 0x00, // mov [edi+0x1004], eax
        0x8b, 0x1f, // mov ebx, [edi]
        0xb9, 0xe8, 0x03, 0x00, 0x00, // mov ecx, 1000
        0x01, 0x4f, 0x08, // loop: add [edi+8], ecx
        0x49, // dec ecx
        0x75, 0xfa, // jnz loop
        0xbe, 0x00, 0x20, 0x00, 0x00, // mov esi, 0x2000
        0x81, 0xc7, 0x00, 0x01, 0x00, 0x00, // add edi, 0x100
        0xb9, 0x40, 0x00, 0x00, 0x00, // mov ecx, 64
        0xfc, 0xf3, 0xa5, // cld; rep movsd
        0x8b, 0x15, 0x00, 0x20, 0x80, 0x00, // mov edx, [0x802000]
        0xa3, 0x04, 0x20, 0x80, 0x00, // mov [0x802004], eax
        0xf4,
    ];
    let (stats, bus, cpu) = compare_with_aperture(&code, |bus| {
        for (i, b) in bus.ram.as_mut_slice()[0x2000..0x2100].iter_mut().enumerate() {
            *b = i as u8 ^ 0x5a;
        }
    });
    let ram = bus.ram.as_slice();
    assert_eq!(&ram[0x30000..0x30004], &[0x44, 0x33, 0x22, 0x11]);
    assert_eq!(&ram[0x31004..0x31008], &[0x44, 0x33, 0x22, 0x11]);
    assert_eq!(u32::from_le_bytes(ram[0x30008..0x3000c].try_into().unwrap()), 500_500);
    assert_eq!(&ram[0x30100..0x30200], &ram[0x2000..0x2100]);
    assert_eq!(cpu.regs[2], u32::MAX, "an invalid GATT entry reads all ones");
    assert!(stats.aperture_fills >= 2, "aperture pages exit on every access instead of mapping directly: {stats:?}");
    assert!(stats.mmio_exits <= 2, "only the invalid page may exit: {stats:?}");
}

#[test]
fn a_gtlb_flush_remaps_the_aperture() {
    let mut code = vec![
        0xbf, 0x00, 0x00, 0x80, 0x00, // mov edi, 0x800000
        0xc7, 0x07, 0xaa, 0xaa, 0xaa, 0xaa, // mov dword [edi], 0xaaaaaaaa
        0xc7, 0x87, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov dword [edi+0x2000], 0 (invalid: dropped)
        0x8b, 0x8f, 0x00, 0x20, 0x00, 0x00, // mov ecx, [edi+0x2000]
        0xc7, 0x05, 0x00, 0x00, 0x02, 0x00, 0x01, 0x20, 0x03, 0x00, // mov dword [GATT], 0x32001
        0xc7, 0x05, 0x08, 0x00, 0x02, 0x00, 0x01, 0x30, 0x03, 0x00, // mov dword [GATT+8], 0x33001
        0x66, 0xba, 0xfc, 0x0c, // mov dx, 0xcfc
        0xee, // out dx, al
    ];
    code.extend([0x46; 32]); // inc esi, past the interpreter's tail
    code.extend([
        0xc7, 0x07, 0xbb, 0xbb, 0xbb, 0xbb, // mov dword [edi], 0xbbbbbbbb
        0x8b, 0x07, // mov eax, [edi]
        0xc7, 0x87, 0x00, 0x20, 0x00, 0x00, 0xcc, 0xcc, 0xcc, 0xcc, // mov dword [edi+0x2000], 0xcccccccc
        0xf4,
    ]);
    let (stats, bus, cpu) = compare_with_aperture(&code, |_| {});
    let ram = bus.ram.as_slice();
    assert_eq!(&ram[0x30000..0x30004], &[0xaa; 4]);
    assert_eq!(&ram[0x32000..0x32004], &[0xbb; 4]);
    assert_eq!(&ram[0x33000..0x33004], &[0xcc; 4], "a page that was invalid before the flush must map now");
    assert_eq!((cpu.regs[0], cpu.regs[1]), (0xbbbb_bbbb, u32::MAX));
    assert!(stats.aperture_flushes >= 1 && stats.aperture_fills >= 3, "a GTLB flush left stale aperture mappings in place: {stats:?}");
}

#[test]
fn code_rewritten_through_the_aperture_is_translated_again() {
    let mut code = vec![
        0xbf, 0x00, 0x00, 0x80, 0x00, // mov edi, 0x800000
        0xc6, 0x47, 0x01, 0x01, // mov byte [edi+1], 1
    ];
    code.extend([0x46; 32]); // inc esi, past the interpreter's tail
    code.extend([
        0xbb, 0x00, 0x00, 0x03, 0x00, // mov ebx, 0x30000
        0xff, 0xd3, // call ebx
        0x89, 0xc2, // mov edx, eax
        0xc6, 0x47, 0x01, 0x02, // mov byte [edi+1], 2
    ]);
    code.extend([0x46; 32]);
    code.extend([0xff, 0xd3, 0xf4]); // call ebx; hlt
    let (_, _, cpu) = compare_with_aperture(&code, |bus| {
        bus.ram.as_mut_slice()[0x30000..0x30006].copy_from_slice(&[0xb8, 0, 0, 0, 0, 0xc3]); // mov eax, imm; ret
    });
    assert_eq!((cpu.regs[2], cpu.regs[0]), (1, 2));
}

/// Writes the runtime's code to the file named by RT_OUT, for disassembly:
/// `RT_OUT=rt.bin cargo test -p jit dump_runtime_code -- --ignored`.
#[test]
#[ignore]
fn dump_runtime_code() {
    let (code, e) = crate::runtime::build();
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    std::fs::write(std::env::var("RT_OUT").unwrap(), bytes).unwrap();
    eprintln!("resume {:#x} dispatch {:#x} exit {:#x}", e.resume, e.dispatch, e.exit);
}


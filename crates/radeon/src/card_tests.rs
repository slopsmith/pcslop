use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::*;

const AGP: u32 = 0xf800_0000;
const AGP_BUS: u32 = 0x0010_0000;
const RPTR_ADDR: u32 = AGP + 0x1_0000;
const SCRATCH_MEM: u32 = AGP_BUS + 0x1_0020;
const RING_DWORDS: u32 = 128;

/// Guest memory shared by the test and the card's worker. While the gate is
/// shut, reads on the worker's side block, which holds the worker mid-run.
#[derive(Clone)]
struct Ram {
    mem: Arc<Mutex<Vec<u8>>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    gated: bool,
}

impl Ram {
    fn new() -> Self {
        Ram { mem: Arc::new(Mutex::new(vec![0; 4 << 20])), gate: Arc::new((Mutex::new(true), Condvar::new())), gated: false }
    }

    fn for_worker(&self) -> Self {
        Ram { gated: true, ..self.clone() }
    }

    fn get(&self, addr: u32) -> u32 {
        let m = self.mem.lock().unwrap();
        u32::from_le_bytes(m[addr as usize..][..4].try_into().unwrap())
    }

    fn put(&self, addr: u32, d: u32) {
        self.mem.lock().unwrap()[addr as usize..][..4].copy_from_slice(&d.to_le_bytes());
    }

    fn shut(&self) {
        *self.gate.0.lock().unwrap() = false;
    }

    fn open(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }

    /// Opens the gate from another thread after a moment.
    fn open_later(&self) {
        let r = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            r.open();
        });
    }
}

impl BusMemory for Ram {
    fn read(&mut self, addr: u32, buf: &mut [u8]) {
        if self.gated {
            let (open, cv) = &*self.gate;
            let _g = cv.wait_while(open.lock().unwrap(), |o| !*o).unwrap();
        }
        let m = self.mem.lock().unwrap();
        buf.copy_from_slice(&m[addr as usize..][..buf.len()]);
    }

    fn write(&mut self, addr: u32, buf: &[u8]) {
        self.mem.lock().unwrap()[addr as usize..][..buf.len()].copy_from_slice(buf);
    }
}

fn pkt0(reg: u32, count: u32) -> u32 {
    (count - 1) << 16 | reg >> 2
}

/// A card with its ring at the AGP aperture's start and a worker thread
/// whose wake-ups for the board count into the returned counter.
fn threaded_card(ram: &mut Ram) -> (Card, Arc<AtomicU32>) {
    let mut c = Card::new(vec![0x55, 0xaa]);
    let mut w = |reg, v| c.mmio_write(reg, 4, v, ram);
    w(MC_FB_LOCATION, 0xf7ff_f000);
    w(MC_AGP_LOCATION, 0xf87f_f800);
    w(AGP_BASE, AGP_BUS);
    w(CP_RB_BASE, AGP);
    w(CP_RB_CNTL, 6);
    w(CP_RB_RPTR_ADDR, RPTR_ADDR);
    w(SCRATCH_ADDR, RPTR_ADDR + 0x20);
    w(SCRATCH_UMSK, 0xff);
    w(CP_CSQ_CNTL, 0x4000_0000);
    let wakes = Arc::new(AtomicU32::new(0));
    let counter = wakes.clone();
    c.start_thread(Box::new(ram.for_worker()), Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    (c, wakes)
}

fn submit(c: &mut Card, ram: &mut Ram, packets: &[u32]) {
    let mut wptr = c.mmio_read(CP_RB_WPTR, 4);
    for &d in packets {
        ram.put(AGP_BUS + 4 * wptr, d);
        wptr = (wptr + 1) % RING_DWORDS;
    }
    c.mmio_write(CP_RB_WPTR, 4, wptr, ram);
}

fn wait_idle(c: &mut Card) {
    let start = Instant::now();
    while c.mmio_read(RBBM_STATUS, 4) & RBBM_ACTIVE != 0 {
        assert!(start.elapsed() < Duration::from_secs(5), "the card never went idle");
        std::thread::yield_now();
    }
}

#[test]
fn a_write_pointer_write_returns_before_the_ring_runs() {
    let mut ram = Ram::new();
    let (mut c, _) = threaded_card(&mut ram);
    ram.shut();
    submit(&mut c, &mut ram, &[pkt0(SCRATCH_REG0 + 4, 1), 0x1234]);
    assert_eq!(c.mmio_read(SCRATCH_REG0 + 4, 4), 0);
    ram.open();
    wait_idle(&mut c);
    assert_eq!(c.mmio_read(SCRATCH_REG0 + 4, 4), 0x1234);
}

#[test]
fn engine_status_reads_busy_until_the_ring_has_run() {
    let mut ram = Ram::new();
    let (mut c, _) = threaded_card(&mut ram);
    ram.shut();
    submit(&mut c, &mut ram, &[pkt0(SCRATCH_REG0, 1), 1]);
    assert_ne!(c.mmio_read(RBBM_STATUS, 4) & RBBM_ACTIVE, 0);
    assert_ne!(c.mmio_read(CP_STAT, 4) & CP_BUSY, 0);
    ram.open();
    wait_idle(&mut c);
    assert_eq!(c.mmio_read(RBBM_STATUS, 4), RBBM_FIFOCNT_IDLE);
    assert_eq!(c.mmio_read(CP_STAT, 4), 0);
}

#[test]
fn the_read_pointer_and_its_writeback_advance_when_the_worker_has_run() {
    let mut ram = Ram::new();
    let (mut c, _) = threaded_card(&mut ram);
    ram.shut();
    submit(&mut c, &mut ram, &[pkt0(SCRATCH_REG0, 2), 5, 6]);
    assert_eq!(c.mmio_read(CP_RB_RPTR, 4), 0);
    ram.open();
    wait_idle(&mut c);
    assert_eq!(c.mmio_read(CP_RB_RPTR, 4), 3);
    assert_eq!(ram.get(AGP_BUS + 0x1_0000), 3);
}

#[test]
fn fences_in_the_ring_reach_memory_after_the_work_before_them() {
    let mut ram = Ram::new();
    let (mut c, _) = threaded_card(&mut ram);
    let ib = AGP + 0x2_0000;
    ram.put(AGP_BUS + 0x2_0000, pkt0(SCRATCH_REG0 + 8, 1));
    ram.put(AGP_BUS + 0x2_0004, 0xaa);
    submit(&mut c, &mut ram, &[pkt0(CP_IB_BASE, 2), ib, 2, pkt0(CP_RESYNC_ADDR, 2), 3, 0x77]);
    wait_idle(&mut c);
    assert_eq!(ram.get(SCRATCH_MEM + 8), 0xaa);
    assert_eq!(ram.get(SCRATCH_MEM + 12), 0x77);
    assert_eq!(c.mmio_read(SCRATCH_REG0 + 12, 4), 0x77);
}

#[test]
fn engine_register_writes_from_the_cpu_wait_for_the_ring_before_them() {
    let mut ram = Ram::new();
    let (mut c, _) = threaded_card(&mut ram);
    ram.shut();
    submit(&mut c, &mut ram, &[pkt0(SCRATCH_REG0 + 4, 1), 1]);
    ram.open_later();
    c.mmio_write(SCRATCH_REG0 + 4, 4, 2, &mut ram);
    wait_idle(&mut c);
    assert_eq!(c.mmio_read(SCRATCH_REG0 + 4, 4), 2);
}

#[test]
fn a_software_interrupt_from_the_ring_wakes_the_board_and_raises_inta() {
    let mut ram = Ram::new();
    let (mut c, wakes) = threaded_card(&mut ram);
    c.mmio_write(GEN_INT_CNTL, 4, SW_INT_TEST, &mut ram);
    submit(&mut c, &mut ram, &[pkt0(GEN_INT_STATUS, 1), SW_INT_FIRE]);
    wait_idle(&mut c);
    assert!(wakes.load(Ordering::SeqCst) > 0);
    assert!(c.take_irq_change());
    assert!(c.irq());
    c.mmio_write(GEN_INT_STATUS, 4, SW_INT_TEST, &mut ram);
    assert!(!c.irq());
}

#[test]
fn a_save_runs_the_posted_ring_first() {
    let mut ram = Ram::new();
    let (mut c, _) = threaded_card(&mut ram);
    ram.shut();
    submit(&mut c, &mut ram, &[pkt0(SCRATCH_REG0 + 20, 1), 0x55]);
    ram.open_later();
    let mut w = snapshot::Writer::new();
    w.put(&c);
    let mut restored = Card::new(vec![0x55, 0xaa]);
    snapshot::Reader::new(&w.buf).get(&mut restored).unwrap();
    assert_eq!(restored.mmio_read(SCRATCH_REG0 + 20, 4), 0x55);
    assert_eq!(restored.mmio_read(CP_RB_RPTR, 4), 2);
}

#[test]
fn the_card_saves_in_the_same_format_with_or_without_its_thread() {
    assert_eq!(<Card as snapshot::Snap>::SCHEMA, <Radeon as snapshot::Snap>::SCHEMA);
}

#[test]
fn stopping_the_thread_runs_the_ring_on_the_callers_thread_again() {
    let mut ram = Ram::new();
    let (mut c, _) = threaded_card(&mut ram);
    c.stop_thread();
    submit(&mut c, &mut ram, &[pkt0(SCRATCH_REG0, 1), 9]);
    assert_eq!(c.mmio_read(SCRATCH_REG0, 4), 9);
}

#[test]
fn a_live_refresh_skips_while_the_worker_holds_the_card_and_waits_briefly_after_six_skips() {
    let mut ram = Ram::new();
    let (mut c, _) = threaded_card(&mut ram);
    let mut out = Vec::new();
    c.mark_changed();
    ram.shut();
    submit(&mut c, &mut ram, &[pkt0(SCRATCH_REG0, 1), 1]);
    std::thread::sleep(Duration::from_millis(20));
    for _ in 0..6 {
        let start = Instant::now();
        assert!(c.render_live(&mut out).is_none());
        assert!(start.elapsed() < Duration::from_millis(20), "a skipped refresh must not wait");
    }
    ram.open_later();
    let start = Instant::now();
    assert!(c.render_live(&mut out).is_none(), "the card is still held, so the seventh refresh skips too");
    let waited = start.elapsed();
    assert!(waited >= crate::card::REFRESH_LOCK_WAIT, "the seventh refresh gives the worker time to yield");
    assert!(waited < Duration::from_millis(40), "a long hold, like a shader compile, must not stall the guest");
    wait_idle(&mut c);
    c.render_live(&mut out);
    assert_eq!(c.skipped, 0, "the refresh after the worker lets go gets the card");
}

#[cfg(target_os = "macos")]
#[test]
fn the_command_processor_thread_asks_for_performance_cores() {
    use std::os::unix::thread::JoinHandleExt;
    unsafe extern "C" {
        fn pthread_get_qos_class_np(thread: RawPthread, class: *mut u32, relative_priority: *mut i32) -> i32;
    }
    use std::os::unix::thread::RawPthread;
    let mut ram = Ram::new();
    let (c, _) = threaded_card(&mut ram);
    let (mut class, mut rel) = (0u32, 0i32);
    let handle = c.worker.as_ref().expect("threaded_card starts the worker").as_pthread_t();
    std::thread::sleep(Duration::from_millis(20));
    unsafe { pthread_get_qos_class_np(handle, &mut class, &mut rel) };
    assert_eq!(class, crate::card::QOS_CLASS_USER_INTERACTIVE, "the guest stalls when the worker lands on an efficiency core");
}

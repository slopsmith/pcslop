//! The card as the board owns it. The register file, the engines and VRAM
//! sit behind a lock. After `start_thread`, a worker thread runs the command
//! processor's ring and indirect buffers while the vCPU goes on: a
//! CP_RB_WPTR write only posts the new pointer and wakes the worker.
//!
//! The worker runs packets in short slices and lets the vCPU take the lock
//! between them. Reads of the registers drivers poll (CP_RB_RPTR, the scratch
//! registers, RBBM_STATUS, CP_STAT) come from mirrors without the lock and
//! report busy while posted work, Metal draws or fences are outstanding.
//! Writes to engine registers from the guest wait until the worker has run
//! everything posted before them; display, interrupt and other registers
//! the ring does not depend on are written at once, as on the card.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use crate::crtc::FRAME_NS;
use crate::regs::*;
use crate::{Bars, BusMemory, NoBus, Radeon, Source, VRAM_SIZE};

/// Packets the worker runs before it publishes progress and lets go of the lock.
const SLICE: u32 = 32;
/// How long an idle worker spins for more work before it parks.
const SPIN: Duration = Duration::from_micros(50);
/// How often the worker looks for finished Metal command buffers.
const GPU_POLL: Duration = Duration::from_micros(50);
/// How long after the last fence landed the worker spins instead while fences
/// wait: about a Metal round trip, so a GPU-bound guest does not keep a core busy.
const FENCE_SPIN: Duration = Duration::from_micros(500);

/// What both threads share without the lock.
#[derive(Default)]
pub(crate) struct Status {
    /// Ring kicks the vCPU has posted, and how many of them the worker has run.
    submitted: AtomicU64,
    done: AtomicU64,
    wptr: AtomicU32,
    rptr: AtomicU32,
    scratch: [AtomicU32; 8],
    mm_index: AtomicU32,
    /// Metal draws in flight or fences waiting.
    gpu_busy: AtomicBool,
    fences: AtomicBool,
    int_status: AtomicU32,
    int_cntl: AtomicU32,
    vblank_acked: AtomicU64,
    /// The worker changed what the interrupt line depends on.
    irq_changed: AtomicBool,
    now_ns: AtomicU64,
    /// vCPU threads waiting for the lock; the worker yields to them.
    want_lock: AtomicU32,
    sleeping: AtomicBool,
    stop: AtomicBool,
    worker: OnceLock<Thread>,
    idle_lock: Mutex<()>,
    idle: Condvar,
    /// The guest wrote VRAM through its mapping.
    fb_written: AtomicBool,
    /// A live refresh found the shown picture still being drawn by Metal;
    /// the worker presents it once the GPU is done.
    present_wanted: AtomicBool,
    presenter: Mutex<Option<Presenter>>,
}

/// Receives finished pictures from the worker as 0x00RRGGBB pixels, width and
/// height; it may swap the buffer for another.
pub type Presenter = Box<dyn FnMut(&mut Vec<u32>, u32, u32) + Send>;

impl Status {
    pub(crate) fn ring_pending(&self) -> bool {
        self.submitted.load(SeqCst) != self.done.load(Acquire)
    }

    fn busy(&self) -> bool {
        self.ring_pending() || self.gpu_busy.load(Acquire)
    }

    /// Copies what lock-free readers need out of `r`; true if the
    /// interrupt inputs changed.
    fn publish(&self, r: &mut Radeon) -> bool {
        self.rptr.store(r.cp.rptr, Release);
        for (i, s) in self.scratch.iter().enumerate() {
            s.store(r.regs[SCRATCH_REG0 as usize / 4 + i], Release);
        }
        self.mm_index.store(r.regs[MM_INDEX as usize / 4], Relaxed);
        let fences = r.fences_pending();
        let gpu = r.metal.as_ref().is_some_and(|m| m.busy());
        self.fences.store(fences, Release);
        self.gpu_busy.store(gpu || fences, Release);
        let a = self.int_status.swap(r.int_status, Relaxed) != r.int_status;
        let b = self.int_cntl.swap(r.reg(GEN_INT_CNTL), Relaxed) != r.reg(GEN_INT_CNTL);
        let c = self.vblank_acked.swap(r.vblank_acked[1], Relaxed) != r.vblank_acked[1];
        a || b || c
    }

    fn irq(&self) -> bool {
        let frame = self.now_ns.load(Relaxed) / FRAME_NS;
        let (status, cntl) = (self.int_status.load(Relaxed), self.int_cntl.load(Relaxed));
        crate::irq_level(status, cntl, self.vblank_acked.load(Relaxed), frame)
    }

    fn wake_worker(&self) {
        std::sync::atomic::fence(SeqCst);
        if self.sleeping.load(SeqCst)
            && let Some(t) = self.worker.get()
        {
            t.unpark();
        }
    }
}

impl Radeon {
    /// With a worker thread: true while ring work, Metal draws or fences
    /// are outstanding. Without one, polls wait for everything instead.
    pub(crate) fn engine_busy(&mut self) -> bool {
        if !self.defer_ring {
            return false;
        }
        let gpu = self.metal.as_mut().is_some_and(|m| {
            m.completed();
            m.busy()
        });
        gpu || self.fences_pending() || self.cp_running() || self.status.ring_pending()
    }

    fn gpu_poll(&mut self, mem: &mut dyn BusMemory) {
        if let Some(m) = self.metal.as_mut() {
            m.completed();
        }
        self.poll_fences(mem);
    }

    fn drop_ring_run(&mut self) {
        self.cp_drop_run();
        self.ring_kicked = false;
    }
}

/// An ATI Radeon 9700 Pro, as the board sees it.
pub struct Card {
    core: Arc<Mutex<Radeon>>,
    st: Arc<Status>,
    pub(crate) worker: Option<JoinHandle<()>>,
    vram: *mut u8,
    rom: Vec<u8>,
    tracing: bool,
    /// Live display refreshes skipped in a row because the card was busy.
    pub(crate) skipped: u32,
}

/// Live refreshes that may be skipped in a row, about 100 ms at 60 Hz,
/// before one waits for the card.
const MAX_SKIPPED_REFRESHES: u32 = 6;

/// How long a refresh that has waited `MAX_SKIPPED_REFRESHES` blocks the
/// emulation thread before it skips this frame too.
pub(crate) const REFRESH_LOCK_WAIT: Duration = Duration::from_millis(4);

// `vram` points into the VRAM `core` owns, which never moves.
unsafe impl Send for Card {}

impl Card {
    pub fn new(rom: Vec<u8>) -> Self {
        let mut r = Radeon::new(rom.clone());
        let vram = r.vram_mut().as_mut_ptr();
        let st = r.status.clone();
        st.publish(&mut r);
        Card { core: Arc::new(Mutex::new(r)), st, worker: None, vram, rom, tracing: false, skipped: 0 }
    }

    fn lock(&self) -> MutexGuard<'_, Radeon> {
        self.st.want_lock.fetch_add(1, SeqCst);
        let g = lock(&self.core, &self.st);
        self.st.want_lock.fetch_sub(1, SeqCst);
        g
    }

    fn try_lock(&self) -> Option<MutexGuard<'_, Radeon>> {
        match self.core.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    // The worker yields between slices but holds the lock through a Metal
    // shader compile, which can take a large fraction of a second.
    fn lock_within(&self, limit: Duration) -> Option<MutexGuard<'_, Radeon>> {
        self.st.want_lock.fetch_add(1, SeqCst);
        let deadline = Instant::now() + limit;
        let g = loop {
            if let Some(g) = self.try_lock() {
                break Some(g);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::yield_now();
        };
        self.st.want_lock.fetch_sub(1, SeqCst);
        g
    }

    fn threaded(&self) -> bool {
        self.worker.is_some()
    }

    /// Runs the command processor on a worker thread from now on. `bus` is
    /// guest memory as the worker reaches it; `wake` gets the board to look
    /// at the card's interrupt line after the worker changed it.
    pub fn start_thread(&mut self, bus: Box<dyn BusMemory + Send>, wake: Box<dyn Fn() + Send + Sync>) {
        if self.threaded() {
            return;
        }
        self.lock().defer_ring = true;
        let (core, st) = (self.core.clone(), self.st.clone());
        let handle = thread::Builder::new()
            .name("radeon-cp".into())
            .spawn(move || {
                interactive_qos();
                let run = std::panic::AssertUnwindSafe(|| worker(&core, &st, bus, wake));
                if std::panic::catch_unwind(run).is_err() {
                    std::process::abort();
                }
            })
            .expect("cannot start the Radeon's command processor thread");
        let _ = self.st.worker.set(handle.thread().clone());
        self.worker = Some(handle);
    }

    /// Stops the worker thread after the work posted so far; the card runs
    /// the ring on the caller's thread again.
    pub fn stop_thread(&mut self) {
        let Some(handle) = self.worker.take() else { return };
        self.drain();
        self.st.stop.store(true, SeqCst);
        handle.thread().unpark();
        let _ = handle.join();
        self.st.stop.store(false, SeqCst);
        self.lock().defer_ring = false;
    }

    /// Waits until the worker has run every ring kick posted so far.
    fn drain(&self) {
        if !self.st.ring_pending() {
            return;
        }
        self.st.wake_worker();
        let mut g = self.st.idle_lock.lock().unwrap_or_else(PoisonError::into_inner);
        while self.st.ring_pending() {
            g = self.st.idle.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn submit(&self) {
        self.st.submitted.fetch_add(1, SeqCst);
        self.st.wake_worker();
    }

    pub fn set_trace(&mut self, out: Box<dyn std::io::Write + Send>) {
        self.tracing = true;
        self.lock().set_trace(out);
    }

    pub fn note(&self, text: &str) {
        self.lock().note(text);
    }

    /// Draws the 3D engine's pixels with Metal from now on, if this Mac can.
    pub fn use_metal(&mut self) -> bool {
        self.lock().use_metal()
    }

    pub fn describe(&self) -> String {
        let s = self.lock().describe();
        if self.threaded() { s + &format!(" | cp thread ring kicks {}", self.st.submitted.load(Relaxed)) } else { s }
    }

    /// Commands, 2D operations and 3D draws run so far.
    pub fn stats(&self) -> (u64, u64, u64) {
        self.lock().stats()
    }

    pub fn set_time(&self, now_ns: u64) {
        self.st.now_ns.store(now_ns, Relaxed);
    }

    /// Level of the card's INTA line.
    pub fn irq(&self) -> bool {
        self.st.irq()
    }

    /// When the next vertical blank interrupt is due, if one is enabled and not pending.
    pub fn next_irq_ns(&self) -> Option<u64> {
        let enabled = self.st.int_cntl.load(Relaxed) & (CRTC_VBLANK_STAT | VSYNC_INT) != 0;
        let next = (self.st.now_ns.load(Relaxed) / FRAME_NS + 1) * FRAME_NS;
        (enabled && !self.irq()).then_some(next)
    }

    /// True once after the worker thread changed the interrupt line's inputs.
    pub fn take_irq_change(&self) -> bool {
        self.st.irq_changed.swap(false, AcqRel)
    }

    /// True when `poll` has something to do, or the interrupt line changed.
    pub fn wants_poll(&self) -> bool {
        (!self.threaded() && self.st.fences.load(Acquire)) || self.st.irq_changed.load(Acquire)
    }

    /// Board housekeeping: without a worker thread, applies the fences
    /// whose Metal draws have finished.
    pub fn poll(&mut self, mem: &mut dyn BusMemory) {
        if !self.threaded() && self.st.fences.load(Acquire) {
            let mut g = self.lock();
            g.gpu_poll(mem);
            self.st.publish(&mut g);
        }
    }

    /// Runs everything posted and waits for the GPU, with every scratch
    /// register written back, as a save state needs.
    pub fn finish(&mut self, mem: &mut dyn BusMemory) {
        self.drain();
        let mut g = self.lock();
        g.gpu_finish(mem);
        self.st.publish(&mut g);
    }

    pub fn reset(&mut self) {
        self.drain();
        let mut g = self.lock();
        g.gpu_sync();
        g.reset();
        g.drop_ring_run();
        g.fences = Default::default();
        if let Some(m) = g.metal.as_mut() {
            m.zpass_forget();
        }
        self.st.wptr.store(g.reg(CP_RB_WPTR), Release);
        self.st.publish(&mut g);
    }

    pub fn set_bars(&mut self, bars: Bars, config: [u8; 256]) {
        let mut g = self.lock();
        g.bars = bars;
        g.config = config;
    }

    pub fn rom(&self) -> &[u8] {
        &self.rom
    }

    /// VRAM, read without synchronising with the engines, as the guest's
    /// own mapping of it does.
    pub fn vram(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.vram, VRAM_SIZE) }
    }

    pub fn vram_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.vram, VRAM_SIZE) }
    }

    /// Where VRAM lives in host memory, for mapping it into a VM.
    pub fn vram_ptr(&self) -> *mut u8 {
        self.vram
    }

    pub fn with_vga<R>(&mut self, f: impl FnOnce(&mut vga::Vga) -> R) -> R {
        f(self.lock().vga())
    }

    /// Notes that the guest wrote VRAM through a direct mapping.
    pub fn mark_changed(&self) {
        self.st.fb_written.store(true, Relaxed);
    }

    /// The picture if it may have changed since the last call, for the display refresh on
    /// the emulation thread: while the command processor holds the card or
    /// Metal draws into the visible buffer it skips the refresh and returns
    /// None, so the guest does not wait for the GPU, and catches up later.
    /// Lets the worker thread hand pictures it finishes to `f` instead of
    /// making live refreshes wait for the GPU.
    pub fn set_presenter(&mut self, f: Presenter) {
        *self.st.presenter.lock().unwrap_or_else(PoisonError::into_inner) = Some(f);
    }

    pub fn render_live(&mut self, out: &mut Vec<u32>) -> Option<(u32, u32)> {
        let written = self.st.fb_written.swap(false, Relaxed);
        let deferred = self.threaded() && self.st.presenter.lock().unwrap_or_else(PoisonError::into_inner).is_some();
        let wait = self.skipped >= MAX_SKIPPED_REFRESHES && !deferred;
        let (frame, skipped) = {
            let guard = if wait { self.lock_within(REFRESH_LOCK_WAIT) } else { self.try_lock() };
            match guard {
                None => {
                    self.st.fb_written.fetch_or(written, Relaxed);
                    (None, true)
                }
                Some(mut g) => {
                    g.now_ns = self.st.now_ns.load(Relaxed);
                    if !(g.take_changed() || written || self.skipped > 0) {
                        (None, false)
                    } else {
                        match g.render_unless_drawing(out, wait) {
                            Some(frame) => (frame, false),
                            None if deferred => {
                                self.st.present_wanted.store(true, Release);
                                self.st.wake_worker();
                                (None, false)
                            }
                            None => (None, true),
                        }
                    }
                }
            }
        };
        self.skipped = if skipped { self.skipped + 1 } else { 0 };
        frame
    }

    /// Draws the current picture into `out` as 0x00RRGGBB pixels.
    pub fn render(&mut self, out: &mut Vec<u32>) -> Option<(u32, u32)> {
        self.lock().render(out)
    }

    pub fn vga_io_read(&mut self, port: u16, size: u8) -> Option<u32> {
        self.lock().vga_io_read(port, size)
    }

    pub fn vga_io_write(&mut self, port: u16, size: u8, v: u32) -> bool {
        self.lock().vga_io_write(port, size, v)
    }

    pub fn mmio_read(&mut self, off: u32, size: u8) -> u32 {
        self.read(Source::Mmio, off, size)
    }

    /// Writes a register. `mem` is what the card reaches by bus mastering
    /// when the write runs on the caller's thread.
    pub fn mmio_write(&mut self, off: u32, size: u8, v: u32, mem: &mut dyn BusMemory) {
        self.write(Source::Mmio, off, size, v, mem);
    }

    pub fn io_read(&mut self, off: u16, size: u8) -> u32 {
        self.read(Source::Io, off as u32 & 0xff, size)
    }

    pub fn io_write(&mut self, off: u16, size: u8, v: u32, mem: &mut dyn BusMemory) {
        self.write(Source::Io, off as u32 & 0xff, size, v, mem);
    }

    /// The register an access at `off` reaches, or None for VRAM through MM_DATA.
    fn target(&self, off: u32) -> Option<u32> {
        let reg = off & 0xfffc;
        if reg != MM_DATA {
            return Some(reg);
        }
        let index = self.st.mm_index.load(Relaxed);
        (index & 0x8000_0000 == 0).then_some(index & 0xfffc)
    }

    fn fast_read(&self, off: u32) -> Option<u32> {
        let st = &self.st;
        Some(match off {
            CP_RB_RPTR => st.rptr.load(Acquire),
            CP_RB_WPTR => st.wptr.load(Relaxed),
            SCRATCH_REG0..=SCRATCH_REG7 if off & 3 == 0 => st.scratch[(off - SCRATCH_REG0) as usize / 4].load(Acquire),
            RBBM_STATUS => RBBM_FIFOCNT_IDLE | (st.busy() as u32 * RBBM_ACTIVE),
            CP_STAT => st.busy() as u32 * (CP_BUSY | CSF_RING_BUSY),
            _ => return None,
        })
    }

    fn read(&mut self, src: Source, off: u32, size: u8) -> u32 {
        if self.threaded() {
            if src == Source::Mmio
                && size == 4
                && !self.tracing
                && let Some(v) = self.fast_read(off)
            {
                return v;
            }
            if self.target(off).is_some_and(crate::idle_polled_blind) {
                self.drain();
            }
        }
        let mut g = self.lock();
        let v = g.access(src, off, size, None, &mut NoBus);
        self.st.publish(&mut g);
        v
    }

    fn write(&mut self, src: Source, off: u32, size: u8, v: u32, mem: &mut dyn BusMemory) {
        if self.threaded() {
            if src == Source::Mmio && off == CP_RB_WPTR && size == 4 && !self.tracing {
                self.st.wptr.store(v, Release);
                return self.submit();
            }
            if self.target(off).is_some_and(drains_on_write) {
                self.drain();
            }
        }
        let mut g = self.lock();
        g.access(src, off, size, Some(v), mem);
        let kicked = std::mem::take(&mut g.ring_kicked);
        if kicked {
            self.st.wptr.store(g.reg(CP_RB_WPTR), Release);
        }
        self.st.publish(&mut g);
        drop(g);
        if kicked {
            self.submit();
        } else if self.st.gpu_busy.load(Acquire) {
            self.st.wake_worker();
        }
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

impl snapshot::Snap for Card {
    const SCHEMA: u64 = Radeon::SCHEMA;

    fn save(&self, w: &mut snapshot::Writer) {
        self.drain();
        self.lock().save(w);
    }

    fn load(&mut self, r: &mut snapshot::Reader<'_>) -> snapshot::Result<()> {
        self.drain();
        let mut g = self.lock();
        g.gpu_sync();
        g.load(r)?;
        g.drop_ring_run();
        g.fences = Default::default();
        if let Some(m) = g.metal.as_mut() {
            m.zpass_forget();
        }
        g.vga().mark_changed();
        self.st.wptr.store(g.reg(CP_RB_WPTR), Release);
        self.st.publish(&mut g);
        Ok(())
    }
}

/// Writes that wait for the ring work posted before them: the CP's, the
/// engines' and the memory controller's registers.
fn drains_on_write(reg: u32) -> bool {
    match reg {
        CP_RB_WPTR | 0x0f00..=0x0fff => false,
        RBBM_SOFT_RESET | MC_FB_LOCATION | MC_AGP_LOCATION | AGP_BASE => true,
        AIC_CNTL..=AIC_HI_ADDR => true,
        _ => reg >= 0x0400,
    }
}

pub(crate) const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

// The guest waits on this thread, so it belongs on a performance core.
#[cfg(target_os = "macos")]
fn interactive_qos() {
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
    }
    unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
}

#[cfg(not(target_os = "macos"))]
fn interactive_qos() {}

/// Hands the shown picture to the presenter once no Metal work still writes it.
fn present(g: &mut Radeon, st: &Status, picture: &mut Vec<u32>) {
    if g.take_flip_landed() {
        st.present_wanted.store(true, Release);
    }
    if !st.present_wanted.load(Acquire) {
        return;
    }
    let Some(frame) = g.render_unless_drawing(picture, false) else { return };
    st.present_wanted.store(false, Release);
    if let Some((w, h)) = frame
        && let Some(p) = st.presenter.lock().unwrap_or_else(PoisonError::into_inner).as_mut()
    {
        p(picture, w, h);
    }
}

fn lock<'a>(core: &'a Mutex<Radeon>, st: &Status) -> MutexGuard<'a, Radeon> {
    let mut g = core.lock().unwrap_or_else(PoisonError::into_inner);
    g.now_ns = st.now_ns.load(Relaxed);
    g
}

fn worker(core: &Mutex<Radeon>, st: &Status, mut bus: Box<dyn BusMemory + Send>, wake: Box<dyn Fn() + Send + Sync>) {
    let bus = &mut *bus;
    let publish = |g: &mut Radeon| {
        if st.publish(g) {
            st.irq_changed.store(true, Release);
            wake();
        }
    };
    let mut idle_since: Option<Instant> = None;
    let mut fence_since: Option<Instant> = None;
    let mut picture = Vec::new();
    let mut retired = 0;
    while !st.stop.load(Acquire) {
        let target = st.submitted.load(SeqCst);
        if target != st.done.load(Relaxed) {
            idle_since = None;
            let mut g = lock(core, st);
            g.regs[CP_RB_WPTR as usize / 4] = st.wptr.load(Acquire);
            let mut more = g.cp_kick(bus);
            loop {
                if more {
                    let mut n = 0;
                    more = g.cp_step(bus, &mut || {
                        n += 1;
                        n >= SLICE || st.want_lock.load(Relaxed) > 0
                    });
                }
                publish(&mut g);
                if !more {
                    break;
                }
                drop(g);
                while st.want_lock.load(Relaxed) > 0 {
                    thread::yield_now();
                }
                g = lock(core, st);
            }
            drop(g);
            {
                let _l = st.idle_lock.lock().unwrap_or_else(PoisonError::into_inner);
                st.done.store(target, Release);
            }
            st.idle.notify_all();
            continue;
        }
        if st.gpu_busy.load(Acquire) {
            idle_since = None;
            let mut g = lock(core, st);
            g.gpu_poll(bus);
            publish(&mut g);
            present(&mut g, st, &mut picture);
            let fenced = st.fences.load(Acquire);
            if !fenced || g.fences.retired != retired {
                retired = g.fences.retired;
                fence_since = None;
            }
            drop(g);
            if fenced && fence_since.get_or_insert_with(Instant::now).elapsed() < FENCE_SPIN {
                while st.want_lock.load(Relaxed) > 0 {
                    thread::yield_now();
                }
                std::hint::spin_loop();
            } else {
                st.sleeping.store(true, SeqCst);
                if !st.ring_pending() && st.gpu_busy.load(SeqCst) && !st.stop.load(SeqCst) {
                    thread::park_timeout(GPU_POLL);
                }
                st.sleeping.store(false, SeqCst);
            }
            continue;
        }
        if st.present_wanted.load(Acquire) {
            present(&mut lock(core, st), st, &mut picture);
        }
        let since = *idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() < SPIN {
            std::hint::spin_loop();
            continue;
        }
        st.sleeping.store(true, SeqCst);
        if !st.ring_pending() && !st.gpu_busy.load(SeqCst) && !st.stop.load(SeqCst) {
            thread::park();
        }
        st.sleeping.store(false, SeqCst);
        idle_since = None;
    }
}

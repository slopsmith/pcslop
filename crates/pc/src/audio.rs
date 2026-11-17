//! The board's side of the AC'97 controller: the lock it shares with the
//! sound card thread, and what that thread sees of guest RAM and the clock.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// The board clock in a form another thread can read.
pub(crate) struct SharedClock {
    epoch: Instant,
    offset_ns: AtomicI64,
}

impl SharedClock {
    pub(crate) fn new(epoch: Instant) -> Arc<SharedClock> {
        Arc::new(SharedClock { epoch, offset_ns: AtomicI64::new(0) })
    }

    /// Follows the board clock reading `base_ns` at `start`.
    pub(crate) fn rebase(&self, base_ns: u64, start: Instant) {
        let since_epoch = start.duration_since(self.epoch).as_nanos() as i64;
        self.offset_ns.store(base_ns as i64 - since_epoch, Ordering::Release);
    }

    pub(crate) fn now_ns(&self) -> u64 {
        (self.epoch.elapsed().as_nanos() as i64 + self.offset_ns.load(Ordering::Acquire)).max(0) as u64
    }
}

/// Guest RAM for the sound card thread. The pages it writes are queued
/// for the board, which hands them to the JIT's code watch on its next tick.
struct ThreadDma {
    ram: *mut u8,
    len: usize,
    written: Arc<Mutex<Vec<u32>>>,
}

// The board stops the thread before it frees the RAM. Guest code may write
// the same bytes meanwhile, as it could while a real bus master reads them.
unsafe impl Send for ThreadDma {}

impl ThreadDma {
    fn range(&self, addr: u32, len: usize) -> Option<usize> {
        let start = addr as usize;
        (start.checked_add(len)? <= self.len).then_some(start)
    }
}

impl ac97::DmaMemory for ThreadDma {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]) {
        match self.range(addr, buf.len()) {
            Some(at) => unsafe { std::ptr::copy_nonoverlapping(self.ram.add(at), buf.as_mut_ptr(), buf.len()) },
            None => buf.fill(0xff),
        }
    }

    fn write_phys(&mut self, addr: u32, buf: &[u8]) {
        let Some(at) = self.range(addr, buf.len()) else { return };
        let mut pages = self.written.lock().unwrap();
        for page in (addr >> 12)..=((addr as usize + buf.len().max(1) - 1) >> 12) as u32 {
            pages.push(page << 12);
        }
        drop(pages);
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), self.ram.add(at), buf.len()) }
    }
}

pub(crate) struct Audio {
    pub(crate) shared: Arc<ac97::Shared>,
    thread: Option<(ac97::Worker, Arc<Mutex<Vec<u32>>>)>,
}

impl Audio {
    pub(crate) fn new(dev: ac97::Ac97) -> Audio {
        Audio { shared: ac97::Shared::new(dev), thread: None }
    }

    pub(crate) fn threaded(&self) -> bool {
        self.thread.is_some()
    }

    /// Moves the bus masters onto a thread of their own that reaches guest
    /// RAM at `ram` and reads `clock`. `wake` gets the CPU to look at INTA#.
    pub(crate) fn start_thread(&mut self, ram: (*mut u8, usize), clock: Arc<SharedClock>, wake: Box<dyn Fn() + Send>) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mem = ThreadDma { ram: ram.0, len: ram.1, written: written.clone() };
        let worker = ac97::Worker::spawn(self.shared.clone(), Box::new(mem), Box::new(move || clock.now_ns()), wake);
        self.thread = Some((worker, written));
    }

    pub(crate) fn stop_thread(&mut self) {
        self.thread = None;
    }

    /// Guest addresses the thread wrote since the last call.
    pub(crate) fn take_written(&self) -> Vec<u32> {
        self.thread.as_ref().map(|(_, w)| std::mem::take(&mut *w.lock().unwrap())).unwrap_or_default()
    }
}

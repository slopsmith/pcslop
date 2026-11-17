//! The controller behind a lock that the guest's register accesses and a
//! worker thread share, so the bus masters keep pace with host time while
//! the thread that emulates the CPU is busy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::{Ac97, DmaMemory};

/// How often the worker moves samples and checks for interrupts.
const PERIOD: Duration = Duration::from_millis(1);

pub struct Shared {
    dev: Mutex<Ac97>,
    /// INTA# as of the last access, for readers that must not wait for the lock.
    irq: AtomicBool,
    /// Whether the guest lets the controller master the bus.
    bus_master: AtomicBool,
    stop: AtomicBool,
}

impl Shared {
    pub fn new(dev: Ac97) -> Arc<Shared> {
        Arc::new(Shared { dev: Mutex::new(dev), irq: AtomicBool::new(false), bus_master: AtomicBool::new(false), stop: AtomicBool::new(false) })
    }

    /// Runs `f` on the controller and updates the interrupt level after it.
    pub fn with<R>(&self, f: impl FnOnce(&mut Ac97) -> R) -> R {
        let mut dev = self.lock();
        let r = f(&mut dev);
        self.irq.store(dev.irq(), Ordering::Release);
        r
    }

    /// Holds the controller, and so the worker, still. Whoever changes it
    /// through the guard calls [`Shared::with`] afterwards to refresh INTA#.
    pub fn lock(&self) -> MutexGuard<'_, Ac97> {
        self.dev.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn irq(&self) -> bool {
        self.irq.load(Ordering::Acquire)
    }

    pub fn set_bus_master(&self, on: bool) {
        self.bus_master.store(on, Ordering::Relaxed);
    }

    fn bus_master(&self) -> bool {
        self.bus_master.load(Ordering::Relaxed)
    }
}

/// The thread that runs the bus masters. Dropping it stops the thread.
pub struct Worker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Runs the controller's bus masters every millisecond against `mem` at
    /// host time `clock()`, and calls `wake` whenever INTA# goes up.
    pub fn spawn(
        shared: Arc<Shared>,
        mut mem: Box<dyn DmaMemory + Send>,
        clock: Box<dyn Fn() -> u64 + Send>,
        wake: Box<dyn Fn() + Send>,
    ) -> Worker {
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ac97".into())
            .spawn(move || {
                raise_priority();
                while !s.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(PERIOD);
                    if !s.bus_master() {
                        continue;
                    }
                    let was = s.irq();
                    s.with(|dev| dev.run(clock(), &mut *mem));
                    if s.irq() && !was {
                        wake();
                    }
                }
            })
            .expect("cannot start the sound card thread");
        Worker { shared, thread: Some(thread) }
    }
}

#[cfg(target_os = "macos")]
fn raise_priority() {
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
    }
    unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
}

#[cfg(not(target_os = "macos"))]
fn raise_priority() {}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

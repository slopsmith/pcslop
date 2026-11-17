//! Mouse capture: the host cursor is hidden and pinned, and raw host motion
//! goes to the guest's PS/2 mouse, for games that read relative motion.

/// What starts a capture, besides Ctrl+Option+G and the `capture` command.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CaptureMode {
    /// Nothing else.
    #[default]
    Key,
    /// A click in the window.
    Click,
    /// A click in the window while the guest is not driving the USB tablet.
    Auto,
}

pub fn parse_mode(v: &str) -> Result<CaptureMode, String> {
    match v {
        "key" => Ok(CaptureMode::Key),
        "click" => Ok(CaptureMode::Click),
        "auto" => Ok(CaptureMode::Auto),
        _ => Err("expected key, click or auto".into()),
    }
}

/// Turns fractional host motion into whole mouse counts, carrying the
/// remainder so that slow movement still adds up.
#[derive(Default)]
pub struct Accumulator {
    x: f64,
    y: f64,
}

impl Accumulator {
    pub fn add(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        self.x += dx;
        self.y += dy;
        let (wx, wy) = (self.x.trunc(), self.y.trunc());
        self.x -= wx;
        self.y -= wy;
        (wx as i32, wy as i32)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(feature = "window")]
pub use host::{Grab, app_active};

#[cfg(feature = "window")]
mod host {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::{Mutex, Once, mpsc};

    #[repr(C)]
    struct Point {
        x: f64,
        y: f64,
    }

    type EventRef = *mut c_void;
    type TapCallback = extern "C" fn(*mut c_void, u32, EventRef, *mut c_void) -> EventRef;

    const SESSION_TAP: u32 = 1;
    const HEAD_INSERT: u32 = 0;
    const LISTEN_ONLY: u32 = 1;
    const MOUSE_MOVED: u32 = 5;
    const LEFT_DRAGGED: u32 = 6;
    const RIGHT_DRAGGED: u32 = 7;
    const OTHER_DRAGGED: u32 = 27;
    const DELTA_X: u32 = 4;
    const DELTA_Y: u32 = 5;
    const TAP_DISABLED_BY_TIMEOUT: u32 = 0xffff_fffe;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayHideCursor(display: u32) -> i32;
        fn CGDisplayShowCursor(display: u32) -> i32;
        fn CGAssociateMouseAndMouseCursorPosition(connected: u8) -> i32;
        fn CGWarpMouseCursorPosition(p: Point) -> i32;
        fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, callback: TapCallback, info: *mut c_void) -> *mut c_void;
        fn CGEventTapEnable(tap: *mut c_void, enable: bool);
        fn CGEventGetDoubleValueField(event: EventRef, field: u32) -> f64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopCommonModes: *const c_void;
        fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
        fn CFRunLoopGetCurrent() -> *mut c_void;
        fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
        fn CFRunLoopRun();
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    static LISTENING: AtomicBool = AtomicBool::new(false);
    static MOTION: Mutex<(f64, f64)> = Mutex::new((0.0, 0.0));
    static TAP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static START: Once = Once::new();

    extern "C" fn on_event(_proxy: *mut c_void, kind: u32, event: EventRef, _info: *mut c_void) -> EventRef {
        if kind == TAP_DISABLED_BY_TIMEOUT {
            unsafe { CGEventTapEnable(TAP.load(Ordering::Relaxed), true) };
        } else if LISTENING.load(Ordering::Relaxed) {
            let (dx, dy) = unsafe { (CGEventGetDoubleValueField(event, DELTA_X), CGEventGetDoubleValueField(event, DELTA_Y)) };
            let mut m = MOTION.lock().unwrap();
            m.0 += dx;
            m.1 += dy;
        }
        event
    }

    fn start_tap() {
        START.call_once(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mask = [MOUSE_MOVED, LEFT_DRAGGED, RIGHT_DRAGGED, OTHER_DRAGGED].iter().fold(0u64, |m, &t| m | 1 << t);
                let tap = unsafe { CGEventTapCreate(SESSION_TAP, HEAD_INSERT, LISTEN_ONLY, mask, on_event, std::ptr::null_mut()) };
                let _ = tx.send(!tap.is_null());
                if tap.is_null() {
                    return;
                }
                TAP.store(tap, Ordering::Relaxed);
                unsafe {
                    let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
                    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
                    CGEventTapEnable(tap, true);
                    CFRunLoopRun();
                }
            });
            if !rx.recv().unwrap_or(false) {
                eprintln!("mouse capture: macOS refused a mouse event tap, so the guest gets no motion");
            }
        });
    }

    /// Whether this app has the focus. minifb's `is_active` stays true when
    /// another app is brought to the front.
    pub fn app_active() -> bool {
        unsafe {
            let send: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let app = send(objc_getClass(c"NSApplication".as_ptr()), sel_registerName(c"sharedApplication".as_ptr()));
            if app.is_null() {
                return false;
            }
            let is_active: extern "C" fn(*mut c_void, *mut c_void) -> bool = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            is_active(app, sel_registerName(c"isActive".as_ptr()))
        }
    }

    /// The host side of a capture: a hidden cursor detached from the mouse,
    /// so it never stops at a screen edge, and the motion the mouse reports.
    #[derive(Default)]
    pub struct Grab {
        on: bool,
    }

    impl Grab {
        pub fn is_on(&self) -> bool {
            self.on
        }

        /// Hides the cursor and pins it at `at`, in global display points.
        pub fn engage(&mut self, at: (f64, f64)) {
            if self.on {
                return;
            }
            start_tap();
            unsafe {
                CGWarpMouseCursorPosition(Point { x: at.0, y: at.1 });
                CGAssociateMouseAndMouseCursorPosition(0);
                CGDisplayHideCursor(CGMainDisplayID());
            }
            *MOTION.lock().unwrap() = (0.0, 0.0);
            LISTENING.store(true, Ordering::Relaxed);
            self.on = true;
        }

        pub fn release(&mut self) {
            if !self.on {
                return;
            }
            LISTENING.store(false, Ordering::Relaxed);
            unsafe {
                CGAssociateMouseAndMouseCursorPosition(1);
                CGDisplayShowCursor(CGMainDisplayID());
            }
            self.on = false;
        }

        /// Mouse motion since the last call, in points, with positive y down.
        pub fn take_motion(&self) -> (f64, f64) {
            std::mem::take(&mut *MOTION.lock().unwrap())
        }
    }

    impl Drop for Grab {
        fn drop(&mut self) {
            self.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_motion_adds_up_to_whole_counts() {
        let mut a = Accumulator::default();
        let moved = (0..12).map(|_| a.add(0.25, -0.25)).fold((0, 0), |s, d| (s.0 + d.0, s.1 + d.1));
        assert_eq!(moved, (3, -3));
    }

    #[test]
    fn the_fraction_left_over_carries_into_the_next_motion() {
        let mut a = Accumulator::default();
        assert_eq!(a.add(2.75, -1.5), (2, -1));
        assert_eq!(a.add(0.25, -0.5), (1, -1));
        assert_eq!(a.add(0.0, 0.0), (0, 0));
    }

    #[test]
    fn clearing_drops_the_leftover_fraction() {
        let mut a = Accumulator::default();
        a.add(0.9, 0.9);
        a.clear();
        assert_eq!(a.add(0.2, 0.2), (0, 0));
    }

    #[test]
    fn capture_modes_parse() {
        assert_eq!(parse_mode("click"), Ok(CaptureMode::Click));
        assert_eq!(parse_mode("auto"), Ok(CaptureMode::Auto));
        assert_eq!(parse_mode("key"), Ok(CaptureMode::Key));
        assert!(parse_mode("always").is_err());
    }
}

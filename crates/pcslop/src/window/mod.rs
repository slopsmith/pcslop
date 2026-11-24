//! The window: winit for the window and input, Metal for drawing.

mod layout;
mod present;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::macos::EventLoopBuilderExtMacOS;
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::capture::{self, Accumulator, CaptureMode};
use crate::{Event, Shared, keys};
use layout::{Wheel, picture_position, window_size_for};
use present::Presenter;

const RELEASE_HINT: &str = "mouse captured, Ctrl+Option+G releases";
const TITLE_INTERVAL: Duration = Duration::from_millis(500);
const FIRST_SIZE: (u32, u32) = (720, 400);

/// Runs the window on this (the main) thread until it is closed or the
/// emulator stops, then asks the emulator to quit.
pub fn run(tx: Sender<Event>, shared: Arc<Shared>, capture_mode: CaptureMode) {
    let mut builder = EventLoop::builder();
    // No menu bar shortcuts, so Cmd+Q and Cmd+H reach the guest as Windows-key chords.
    builder.with_default_menu(false);
    let event_loop = match builder.build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot open window: {e}");
            let _ = tx.send(Event::Quit);
            return;
        }
    };
    let proxy = event_loop.create_proxy();
    *shared.wake.lock().unwrap() = Some(Box::new(move || {
        let _ = proxy.send_event(());
    }));
    let mut app = App::new(tx.clone(), shared.clone(), capture_mode);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("window: {e}");
    }
    *shared.wake.lock().unwrap() = None;
    let _ = tx.send(Event::Quit);
}

struct App {
    tx: Sender<Event>,
    shared: Arc<Shared>,
    capture_mode: CaptureMode,
    window: Option<Window>,
    presenter: Option<Presenter>,
    seq: u64,
    picture: (u32, u32),
    fitted: (u32, u32),
    focused: bool,
    mods: ModifiersState,
    /// Scancodes the guest has seen pressed and not yet released.
    held: BTreeSet<u16>,
    swallow_g: bool,
    toggle: bool,
    capturing_click: bool,
    swallow_click: bool,
    captured: bool,
    cursor: Option<PhysicalPosition<f64>>,
    last_cursor: Option<PhysicalPosition<f64>>,
    pointer_moved: bool,
    buttons: u8,
    sent_buttons: Option<u8>,
    wheel: Wheel,
    raw: (f64, f64),
    motion: Accumulator,
    rel: Accumulator,
    rate_at: (Instant, u64),
    mips: f64,
}

impl App {
    fn new(tx: Sender<Event>, shared: Arc<Shared>, capture_mode: CaptureMode) -> Self {
        Self {
            tx,
            shared,
            capture_mode,
            window: None,
            presenter: None,
            seq: 0,
            picture: FIRST_SIZE,
            fitted: FIRST_SIZE,
            focused: false,
            mods: ModifiersState::empty(),
            held: BTreeSet::new(),
            swallow_g: false,
            toggle: false,
            capturing_click: false,
            swallow_click: false,
            captured: false,
            cursor: None,
            last_cursor: None,
            pointer_moved: false,
            buttons: 0,
            sent_buttons: None,
            wheel: Wheel::default(),
            raw: (0.0, 0.0),
            motion: Accumulator::default(),
            rel: Accumulator::default(),
            rate_at: (Instant::now(), 0),
            mips: 0.0,
        }
    }

    fn send_key(&mut self, code: u16, down: bool) {
        if down {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
        let _ = self.tx.send(Event::Key(code, down));
    }

    fn release_keys(&mut self) {
        for code in std::mem::take(&mut self.held) {
            let _ = self.tx.send(Event::Key(code, false));
        }
    }

    fn key(&mut self, ev: KeyEvent) {
        let PhysicalKey::Code(code) = ev.physical_key else { return };
        let down = ev.state == ElementState::Pressed;
        if code == KeyCode::KeyG {
            if down && !ev.repeat && self.mods.control_key() && self.mods.alt_key() {
                self.swallow_g = true;
                self.toggle = true;
                return;
            }
            if self.swallow_g {
                self.swallow_g = down;
                return;
            }
        }
        let Some(sc) = keys::from_keycode(code) else { return };
        if code == KeyCode::CapsLock {
            // macOS reports Caps Lock turning on as a press and off as a
            // release; the guest needs a whole keystroke for each toggle.
            self.send_key(sc, true);
            self.send_key(sc, false);
            return;
        }
        if !down && !self.held.contains(&sc) {
            return;
        }
        // OS key repeat goes on as repeated make codes, which is how a PS/2
        // keyboard's typematic repeat looks to the guest.
        self.send_key(sc, down);
    }

    fn mouse_button(&mut self, button: MouseButton, state: ElementState) {
        let bit = match button {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 4,
            _ => return,
        };
        if state == ElementState::Pressed {
            self.buttons |= bit;
            if bit == 1 && !self.captured {
                self.capturing_click = capture::click_captures(self.capture_mode, self.shared.tablet_in_use.load(Ordering::Relaxed));
            }
        } else {
            self.buttons &= !bit;
        }
    }

    /// Picks up a new guest frame, if there is one.
    fn take_frame(&mut self) {
        let Some(p) = self.presenter.as_mut() else { return };
        let f = self.shared.frame.lock().unwrap();
        if f.seq == self.seq || f.width == 0 {
            return;
        }
        self.seq = f.seq;
        p.upload(&f.pixels, f.width, f.height);
        self.picture = (f.width, f.height);
        drop(f);
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Resizes the window to a new guest resolution, unless the user has
    /// made it full screen or maximized it.
    fn fit_window(&mut self) {
        let Some(w) = &self.window else { return };
        let (pw, ph) = self.picture;
        if self.picture == self.fitted || pw < 320 || ph < 200 || w.fullscreen().is_some() || w.is_maximized() {
            return;
        }
        self.fitted = self.picture;
        let screen = w.current_monitor().map(|m| {
            let s = m.size().to_logical::<f64>(m.scale_factor());
            (s.width, s.height)
        });
        let (ww, wh) = window_size_for(self.picture, screen);
        let at = w.outer_position().ok();
        let _ = w.request_inner_size(LogicalSize::new(ww, wh));
        if let Some(at) = at {
            w.set_outer_position(at);
        }
        self.last_cursor = None;
    }

    fn sync_capture(&mut self) {
        let Some(w) = &self.window else { return };
        let (want, swallow) = capture::decide(capture::Input {
            captured: self.shared.mouse_captured.load(Ordering::Relaxed),
            toggle: std::mem::take(&mut self.toggle),
            capturing_click: std::mem::take(&mut self.capturing_click),
            focused: self.focused,
        });
        self.shared.mouse_captured.store(want, Ordering::Relaxed);
        if swallow {
            self.swallow_click = true;
        }
        if want == self.captured {
            return;
        }
        self.captured = want;
        if want {
            lock_cursor(w);
            w.set_cursor_visible(false);
            self.raw = (0.0, 0.0);
            self.motion.clear();
        } else {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            w.set_cursor_visible(true);
            self.swallow_click = false;
            self.last_cursor = None;
        }
        self.sent_buttons = None;
        self.retitle();
    }

    /// Sends what the mouse did since the last call.
    fn flush_pointer(&mut self) {
        let wheel = self.wheel.take();
        if self.captured {
            if self.swallow_click && self.buttons & 1 == 0 {
                self.swallow_click = false;
            }
            let buttons = if self.swallow_click { self.buttons & !1 } else { self.buttons };
            let (dx, dy) = self.motion.add(self.raw.0, self.raw.1);
            self.raw = (0.0, 0.0);
            if dx != 0 || dy != 0 || wheel != 0 || Some(buttons) != self.sent_buttons {
                let _ = self.tx.send(Event::Mouse(dx, -dy, wheel, buttons));
                self.sent_buttons = Some(buttons);
            }
            return;
        }
        let (Some(pos), Some(p), Some(w)) = (self.cursor, &self.presenter, &self.window) else { return };
        if !self.pointer_moved && wheel == 0 && Some(self.buttons) == self.sent_buttons {
            return;
        }
        self.pointer_moved = false;
        let (x, y) = picture_position(pos.x, pos.y, p.picture_rect());
        let scale = w.scale_factor();
        let last = self.last_cursor.replace(pos).unwrap_or(pos);
        let (dx, dy) = self.rel.add((pos.x - last.x) / scale, (pos.y - last.y) / scale);
        let _ = self.tx.send(Event::Pointer { x, y, dx, dy: -dy, buttons: self.buttons, wheel });
        self.sent_buttons = Some(self.buttons);
    }

    fn retitle(&self) {
        if let Some(w) = &self.window {
            let hint = if self.captured { format!(" | {RELEASE_HINT}") } else { String::new() };
            w.set_title(&format!("pcslop | {:.1} MIPS{hint}", self.mips));
        }
    }
}

/// Pins the hidden cursor in the middle of the window, so clicks land in it.
fn lock_cursor(w: &Window) {
    let size = w.inner_size();
    let _ = w.set_cursor_position(PhysicalPosition::new(size.width / 2, size.height / 2));
    if w.set_cursor_grab(CursorGrabMode::Locked).is_err() {
        let _ = w.set_cursor_grab(CursorGrabMode::Confined);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("pcslop")
            .with_inner_size(LogicalSize::new(FIRST_SIZE.0, FIRST_SIZE.1))
            .with_resizable(true);
        let window = match event_loop.create_window(attrs) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("cannot open window: {e}");
                event_loop.exit();
                return;
            }
        };
        match Presenter::new(&window) {
            Ok(p) => self.presenter = Some(p),
            Err(e) => {
                eprintln!("cannot draw into the window: {e}");
                event_loop.exit();
                return;
            }
        }
        self.focused = window.has_focus();
        self.window = Some(window);
        self.take_frame();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let (Some(p), Some(w)) = (&self.presenter, &self.window) {
                    p.resize(size.into(), w.scale_factor());
                    w.request_redraw();
                    if self.captured {
                        lock_cursor(w);
                    }
                }
                self.last_cursor = None;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let (Some(p), Some(w)) = (&self.presenter, &self.window) {
                    p.resize(w.inner_size().into(), scale_factor);
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(p) = &mut self.presenter {
                    p.draw();
                }
            }
            WindowEvent::Occluded(false) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if !focused {
                    self.release_keys();
                    self.mods = ModifiersState::empty();
                    self.swallow_g = false;
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.key(event),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(position);
                self.pointer_moved = true;
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.last_cursor = None;
            }
            WindowEvent::MouseInput { state, button, .. } => self.mouse_button(button, state),
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::LineDelta(_, y) => self.wheel.lines(y as f64),
                MouseScrollDelta::PixelDelta(p) => {
                    let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor());
                    self.wheel.points(p.y / scale);
                }
            },
            _ => {}
        }
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.captured
        {
            self.raw.0 += delta.0;
            self.raw.1 += delta.1;
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, _: ()) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.shared.done.load(Ordering::Relaxed) {
            event_loop.exit();
            return;
        }
        self.take_frame();
        self.fit_window();
        self.sync_capture();
        self.flush_pointer();
        let elapsed = self.rate_at.0.elapsed();
        if elapsed >= TITLE_INTERVAL {
            let n = self.shared.instructions.load(Ordering::Relaxed);
            self.mips = n.saturating_sub(self.rate_at.1) as f64 / elapsed.as_secs_f64() / 1e6;
            self.rate_at = (Instant::now(), n);
            self.retitle();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.rate_at.0 + TITLE_INTERVAL));
    }
}

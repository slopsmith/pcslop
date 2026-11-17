//! Mouse capture: the host cursor is hidden and pinned, and raw host motion
//! goes to the guest's PS/2 mouse, for games that read relative motion.
#![cfg_attr(not(feature = "window"), allow(dead_code))]

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

/// Whether a left click in the window captures the mouse.
pub fn click_captures(mode: CaptureMode, tablet_in_use: bool) -> bool {
    match mode {
        CaptureMode::Key => false,
        CaptureMode::Click => true,
        CaptureMode::Auto => !tablet_in_use,
    }
}

/// What the window saw since it last decided.
#[derive(Clone, Copy, Default, Debug)]
pub struct Input {
    /// The mouse is captured now (the window or the `capture` command set it).
    pub captured: bool,
    /// Ctrl+Option+G was pressed.
    pub toggle: bool,
    /// A left click landed in the window, and clicks capture in this mode.
    pub capturing_click: bool,
    pub focused: bool,
}

/// Whether the mouse is captured next, and whether the guest must not see
/// the left button until it is released, because that click captured it.
pub fn decide(i: Input) -> (bool, bool) {
    if !i.focused {
        return (false, false);
    }
    let want = i.captured != i.toggle;
    if !want && !i.captured && i.capturing_click {
        return (true, true);
    }
    (want, false)
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

    fn focused() -> Input {
        Input { focused: true, ..Default::default() }
    }

    #[test]
    fn the_hotkey_toggles() {
        assert_eq!(decide(Input { toggle: true, ..focused() }), (true, false));
        assert_eq!(decide(Input { toggle: true, captured: true, ..focused() }), (false, false));
        assert_eq!(decide(Input { captured: true, ..focused() }), (true, false));
    }

    #[test]
    fn a_capturing_click_captures_and_is_swallowed() {
        assert_eq!(decide(Input { capturing_click: true, ..focused() }), (true, true));
        assert_eq!(decide(Input { capturing_click: true, captured: true, ..focused() }), (true, false));
    }

    #[test]
    fn a_click_does_not_undo_a_hotkey_release() {
        assert_eq!(decide(Input { capturing_click: true, captured: true, toggle: true, ..focused() }), (false, false));
    }

    #[test]
    fn losing_focus_releases() {
        assert_eq!(decide(Input { captured: true, ..Default::default() }), (false, false));
        assert_eq!(decide(Input { toggle: true, capturing_click: true, ..Default::default() }), (false, false));
    }

    #[test]
    fn which_clicks_capture() {
        assert!(!click_captures(CaptureMode::Key, false));
        assert!(click_captures(CaptureMode::Click, true));
        assert!(click_captures(CaptureMode::Auto, false));
        assert!(!click_captures(CaptureMode::Auto, true));
    }

    #[test]
    fn capture_modes_parse() {
        assert_eq!(parse_mode("click"), Ok(CaptureMode::Click));
        assert_eq!(parse_mode("auto"), Ok(CaptureMode::Auto));
        assert_eq!(parse_mode("key"), Ok(CaptureMode::Key));
        assert!(parse_mode("always").is_err());
    }
}

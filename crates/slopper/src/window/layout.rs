//! Window geometry and pointer arithmetic, kept apart from winit and Metal.

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Where a `picture` is drawn in a `drawable` (both in pixels): centred with
/// its aspect ratio kept, between black bars. The flag is true for a
/// whole-number scale, drawn nearest-neighbour so guest pixels stay sharp;
/// a scale within 1% of one snaps to it. Other scales filter linearly.
pub fn draw_rect(drawable: (u32, u32), picture: (u32, u32)) -> (Rect, bool) {
    let (dw, dh) = (drawable.0 as f64, drawable.1 as f64);
    let (pw, ph) = (picture.0.max(1) as f64, picture.1.max(1) as f64);
    let fit = (dw / pw).min(dh / ph);
    let whole = fit.floor();
    let nearest = whole >= 1.0 && whole >= fit * 0.99;
    let scale = if nearest { whole } else { fit };
    let (w, h) = ((pw * scale).round(), (ph * scale).round());
    let rect = Rect { x: ((dw - w) / 2.0).floor(), y: ((dh - h) / 2.0).floor(), w, h };
    (rect, nearest)
}

/// Maps a point in the window to the guest tablet's 0..=`usb::MAX_COORD`,
/// given where the picture is drawn. Points on the bars clamp to the edge.
pub fn picture_position(x: f64, y: f64, drawn: Rect) -> (u16, u16) {
    let to_tablet = |v: f64, origin: f64, len: f64| {
        let f = ((v - origin) / len.max(1.0)).clamp(0.0, 1.0);
        (f * (usb::MAX_COORD as f64 + 1.0)).min(usb::MAX_COORD as f64) as u16
    };
    (to_tablet(x, drawn.x, drawn.w), to_tablet(y, drawn.y, drawn.h))
}

/// The window's inner size in points for a `picture`: the picture's own
/// size, scaled down to fit a `screen` of that many points if it is larger.
pub fn window_size_for(picture: (u32, u32), screen: Option<(f64, f64)>) -> (f64, f64) {
    let (w, h) = (picture.0.max(1) as f64, picture.1.max(1) as f64);
    let scale = screen.map_or(1.0, |(sw, sh)| (sw * 0.95 / w).min(sh * 0.9 / h).min(1.0));
    ((w * scale).floor(), (h * scale).floor())
}

/// How far a trackpad or a smooth-scrolling mouse scrolls, in points, for
/// one wheel detent.
const POINTS_PER_DETENT: f64 = 20.0;

/// Turns host scrolling into whole wheel detents, positive away from the
/// user, carrying the remainder.
#[derive(Default)]
pub struct Wheel {
    acc: f64,
}

impl Wheel {
    pub fn lines(&mut self, y: f64) {
        self.acc += y;
    }

    pub fn points(&mut self, y: f64) {
        self.acc += y / POINTS_PER_DETENT;
    }

    pub fn take(&mut self) -> i32 {
        let whole = self.acc.trunc();
        self.acc -= whole;
        whole as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_twice_its_size_is_sharp() {
        let (r, nearest) = draw_rect((1600, 1200), (800, 600));
        assert_eq!(r, Rect { x: 0.0, y: 0.0, w: 1600.0, h: 1200.0 });
        assert!(nearest);
    }

    #[test]
    fn a_wide_window_has_bars_left_and_right() {
        let (r, nearest) = draw_rect((1200, 600), (800, 600));
        assert_eq!(r, Rect { x: 200.0, y: 0.0, w: 800.0, h: 600.0 });
        assert!(nearest);
    }

    #[test]
    fn a_fractional_scale_fills_the_space_with_linear_filtering() {
        let (r, nearest) = draw_rect((1920, 1080), (800, 600));
        assert_eq!(r, Rect { x: 240.0, y: 0.0, w: 1440.0, h: 1080.0 });
        assert!(!nearest);
        let (r, nearest) = draw_rect((700, 500), (800, 600));
        assert!(!nearest && r.w <= 700.0 && r.h <= 500.0);
    }

    #[test]
    fn a_nearly_whole_scale_snaps_to_it() {
        assert!(!draw_rect((3456, 2234), (1440, 900)).1);
        let (r, nearest) = draw_rect((1610, 1205), (800, 600));
        assert!(nearest);
        assert_eq!((r.w, r.h), (1600.0, 1200.0));
        assert_eq!((r.x, r.y), (5.0, 2.0));
    }

    #[test]
    fn letterboxed_picture_position() {
        let (r, _) = draw_rect((800, 600), (800, 600));
        assert_eq!(picture_position(0.0, 0.0, r), (0, 0));
        assert_eq!(picture_position(400.0, 300.0, r), (0x4000, 0x4000));
        let (r, _) = draw_rect((1200, 600), (800, 600));
        assert_eq!(picture_position(200.0, 0.0, r), (0, 0));
        assert_eq!(picture_position(10.0, 0.0, r), (0, 0));
        assert_eq!(picture_position(600.0, 300.0, r), (0x4000, 0x4000));
        assert_eq!(picture_position(1199.0, 650.0, r), (usb::MAX_COORD, usb::MAX_COORD));
        let (r, _) = draw_rect((800, 1200), (800, 600));
        assert_eq!(picture_position(400.0, 300.0, r), (0x4000, 0));
        assert_eq!(picture_position(400.0, 600.0, r), (0x4000, 0x4000));
    }

    #[test]
    fn windows_fit_the_picture_or_the_screen() {
        assert_eq!(window_size_for((800, 600), Some((1512.0, 982.0))), (800.0, 600.0));
        assert_eq!(window_size_for((1440, 900), Some((1512.0, 982.0))), (1414.0, 883.0));
        assert_eq!(window_size_for((1920, 1080), None), (1920.0, 1080.0));
    }

    #[test]
    fn scrolling_adds_up_to_detents() {
        let mut w = Wheel::default();
        w.lines(0.4);
        assert_eq!(w.take(), 0);
        w.lines(0.7);
        assert_eq!(w.take(), 1);
        w.points(-25.0);
        assert_eq!(w.take(), -1);
        w.points(-12.0);
        assert_eq!(w.take(), 0);
        w.points(-6.0);
        assert_eq!(w.take(), -1);
        w.lines(3.1);
        assert_eq!(w.take(), 3);
    }
}

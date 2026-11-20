//! Normal no-argument UI Automation actions used by the three Routines controllers.
use std::time::Duration;

pub(crate) trait Native {
    type Handle: Copy + PartialEq;
    fn handle(&mut self) -> Result<Self::Handle, String>;
    fn top_level(&mut self, handle: Self::Handle) -> bool;
    fn iconic(&mut self, handle: Self::Handle) -> bool;
    fn visible(&mut self, handle: Self::Handle) -> bool;
    fn show(&mut self, handle: Self::Handle, restore: bool);
    fn foreground(&mut self, handle: Self::Handle);
    fn topmost(&mut self, handle: Self::Handle);
    fn focus(&mut self);
    fn bounds(&mut self) -> Result<(i32, i32, i32, i32), String>;
    fn cursor(&mut self) -> (i32, i32);
    fn screen(&mut self) -> (i32, i32);
    fn set_cursor(&mut self, x: i32, y: i32);
    fn mouse(&mut self, down: bool, x: u32, y: u32);
    fn sleep(&mut self, time: Duration);
}

pub(crate) fn activate(native: &mut impl Native) -> Result<(), String> {
    let handle = native.handle()?;
    if native.top_level(handle) {
        if native.iconic(handle) {
            native.show(handle, true);
        } else if !native.visible(handle) {
            native.show(handle, false);
        }
        native.foreground(handle);
        native.sleep(Duration::from_millis(500));
    }
    Ok(())
}

pub(crate) fn focus(native: &mut impl Native) {
    native.focus();
}

pub(crate) fn topmost(native: &mut impl Native) -> Result<(), String> {
    let handle = native.handle()?;
    if native.top_level(handle) {
        native.topmost(handle);
        native.sleep(Duration::from_millis(500));
    }
    Ok(())
}

pub(crate) fn click(native: &mut impl Native) -> Result<(), String> {
    let (left, top, right, bottom) = native.bounds()?;
    let (width, height) = (i64::from(right) - i64::from(left), i64::from(bottom) - i64::from(top));
    if width == 0 || height == 0 {
        return Ok(());
    }
    let x = (i64::from(left) + (width as f64 * 0.5) as i64) as i32;
    let y = (i64::from(top) + (height as f64 * 0.5) as i64) as i32;
    let (cur_x, cur_y) = native.cursor();
    let (screen_w, screen_h) = native.screen();
    let max_side = screen_w.max(screen_h) as f64;
    let min_side = screen_w.min(screen_h) as i64;
    let mut max_point =
        (i64::from(x) - i64::from(cur_x)).abs().max((i64::from(y) - i64::from(cur_y)).abs()).min(min_side);
    let mut move_time = 1.0;
    if (max_point as f64) < max_side {
        max_point = 100 + (((max_side - 100.0) / max_side) * (max_point as f64)) as i64;
        move_time = move_time * (max_point as f64) / max_side;
    }
    let step_count = max_point / 20;
    if step_count > 1 {
        let x_step = (f64::from(x) - f64::from(cur_x)) / (step_count as f64);
        let y_step = (f64::from(y) - f64::from(cur_y)) / (step_count as f64);
        // The per-step pause is rounded up to the timer unit.
        let interval =
            openrigcue_model::python_time::sleep_duration(move_time / (step_count as f64)).unwrap_or_default();
        for i in 0..step_count {
            native.set_cursor(
                (i64::from(cur_x) + (x_step * (i as f64)) as i64) as i32,
                (i64::from(cur_y) + (y_step * (i as f64)) as i64) as i32,
            );
            native.sleep(interval);
        }
    }
    native.set_cursor(x, y);
    native.sleep(Duration::ZERO);
    native.set_cursor(x, y);
    let (screen_w, screen_h) = native.screen();
    let absolute_x = (i64::from(x) * 65535).div_euclid(i64::from(screen_w)) as u32;
    let absolute_y = (i64::from(y) * 65535).div_euclid(i64::from(screen_h)) as u32;
    native.mouse(true, absolute_x, absolute_y);
    native.sleep(Duration::from_millis(50));
    native.mouse(false, absolute_x, absolute_y);
    native.sleep(Duration::from_millis(500));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        rect: (i32, i32, i32, i32),
        iconic: bool,
        visible: bool,
        top: bool,
    }
    impl Native for Fake {
        type Handle = usize;
        fn handle(&mut self) -> Result<usize, String> {
            self.calls.push("handle".into());
            Ok(1)
        }
        fn top_level(&mut self, _: usize) -> bool {
            self.calls.push("top-level".into());
            self.top
        }
        fn iconic(&mut self, _: usize) -> bool {
            self.calls.push("iconic".into());
            self.iconic
        }
        fn visible(&mut self, _: usize) -> bool {
            self.calls.push("visible".into());
            self.visible
        }
        fn show(&mut self, _: usize, restore: bool) {
            self.calls.push(format!("show:{restore}"));
        }
        fn foreground(&mut self, _: usize) {
            self.calls.push("foreground".into());
        }
        fn topmost(&mut self, _: usize) {
            self.calls.push("topmost".into());
        }
        fn focus(&mut self) {
            self.calls.push("focus-ignored-result".into());
        }
        fn bounds(&mut self) -> Result<(i32, i32, i32, i32), String> {
            self.calls.push("bounds".into());
            Ok(self.rect)
        }
        fn cursor(&mut self) -> (i32, i32) {
            self.calls.push("cursor".into());
            (0, 0)
        }
        fn screen(&mut self) -> (i32, i32) {
            self.calls.push("screen".into());
            (1000, 800)
        }
        fn set_cursor(&mut self, x: i32, y: i32) {
            self.calls.push(format!("cursor:{x},{y}"));
        }
        fn mouse(&mut self, down: bool, x: u32, y: u32) {
            self.calls.push(format!("mouse:{down},{x},{y}"));
        }
        fn sleep(&mut self, t: Duration) {
            self.calls.push(format!("sleep:{}", t.as_millis()));
        }
    }
    #[test]
    fn activation_restores_before_foreground_and_waits() {
        let mut f = Fake { top: true, iconic: true, ..Default::default() };
        activate(&mut f).unwrap();
        assert_eq!(f.calls, ["handle", "top-level", "iconic", "show:true", "foreground", "sleep:500"]);
        let mut hidden = Fake { top: true, ..Default::default() };
        activate(&mut hidden).unwrap();
        assert_eq!(hidden.calls, ["handle", "top-level", "iconic", "visible", "show:false", "foreground", "sleep:500"]);
    }
    #[test]
    fn focus_ignores_the_native_boolean_failure() {
        let mut f = Fake::default();
        focus(&mut f);
        assert_eq!(f.calls, ["focus-ignored-result"]);
    }
    #[test]
    fn topmost_skips_non_top_level() {
        let mut f = Fake::default();
        topmost(&mut f).unwrap();
        assert_eq!(f.calls, ["handle", "top-level"]);
    }
    #[test]
    fn zero_sized_click_is_noop() {
        let mut f = Fake::default();
        click(&mut f).unwrap();
        assert_eq!(f.calls, ["bounds"]);
    }
    #[test]
    fn click_moves_smoothly_before_absolute_mouse_events() {
        let mut f = Fake { rect: (190, 90, 210, 110), ..Default::default() };
        click(&mut f).unwrap();
        assert_eq!(&f.calls[..6], ["bounds", "cursor", "screen", "cursor:0,0", "sleep:20", "cursor:14,7"]);
        assert_eq!(
            &f.calls[f.calls.len() - 8..],
            [
                "cursor:200,100",
                "sleep:0",
                "cursor:200,100",
                "screen",
                "mouse:true,13107,8191",
                "sleep:50",
                "mouse:false,13107,8191",
                "sleep:500"
            ]
        );
    }
}

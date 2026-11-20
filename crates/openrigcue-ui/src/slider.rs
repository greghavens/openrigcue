//! Slider arithmetic of the slider widgets: how a pointer position becomes a stored value.
//!
//! `tests/vectors.json` holds test cases for the slider formulas.

use serde::Serialize;

/// Rounds a float to the nearest integer, exact halves to the even neighbour.
pub fn py_round(x: f64) -> f64 {
    let floor = x.floor();
    let fraction = x - floor;
    let rounded = if fraction > 0.5 || (fraction == 0.5 && floor % 2.0 != 0.0) { floor + 1.0 } else { floor };
    // The sign of a zero result is kept: -0.4 rounds to -0.0.
    if rounded == 0.0 {
        0.0_f64.copysign(x)
    } else {
        rounded
    }
}

/// Truncates a float toward zero.
pub fn py_int(x: f64) -> f64 {
    x.trunc()
}

/// The scale of a single-pointer slider: its minimum, maximum and step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SliderScale {
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

impl SliderScale {
    pub const fn new(min: f64, max: f64, step: f64) -> SliderScale {
        SliderScale { min, max, step }
    }

    /// The value a slider takes for a pointer fraction: the fraction is mapped onto
    /// the range, rounded to a whole number of steps, then clamped.
    pub fn value_at(&self, fraction: f64) -> f64 {
        let value = py_round((fraction * (self.max - self.min) + self.min) / self.step) * self.step;
        value.max(self.min).min(self.max)
    }

    /// `MyApp::set_slider`: a stored value goes through the pointer fraction and back, which rounds and
    /// clamps it the way a drag would.
    pub fn snap(&self, value: f64) -> f64 {
        self.value_at((value - self.min) / (self.max - self.min))
    }

    /// The pointer moved by a key: an arrow key moves one step (`delta` is ±1).
    pub fn step_from(&self, current: f64, delta: f64) -> f64 {
        self.value_at((current + delta * self.step - self.min) / (self.max - self.min))
    }
}

/// The scale of a two-pointer range slider: its two limits and step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RangeScale {
    pub limit_min: f64,
    pub limit_max: f64,
    pub step: f64,
}

impl RangeScale {
    pub const fn new(limit_min: f64, limit_max: f64, step: f64) -> RangeScale {
        RangeScale { limit_min, limit_max, step }
    }

    /// The values a range slider takes for its two pointer fractions: both are rounded to steps, then
    /// ordered and clamped ([`Self::clamp`]).
    pub fn values_at(&self, low_fraction: f64, high_fraction: f64) -> (f64, f64) {
        let span = self.limit_max - self.limit_min;
        let low = py_round((low_fraction * span + self.limit_min) / self.step) * self.step;
        let high = py_round((high_fraction * span + self.limit_min) / self.step) * self.step;
        self.clamp(low, high)
    }

    /// `MyApp::set_range`: stored bounds go through their pointer fractions and back.
    pub fn snap(&self, low: f64, high: f64) -> (f64, f64) {
        let span = self.limit_max - self.limit_min;
        self.values_at((low - self.limit_min) / span, (high - self.limit_min) / span)
    }

    /// Orders and clamps a range slider's two values. The upper bound is ordered against the requested lower
    /// bound, not the clamped one.
    pub fn clamp(&self, low: f64, high: f64) -> (f64, f64) {
        let clamped_low = low.max(self.limit_min).min(high);
        let clamped_high = low.max(high).min(self.limit_max);
        (clamped_low, clamped_high)
    }

    /// A scale with new limits, and the current range re-clamped into them.
    pub fn with_limits(&self, limit_min: f64, limit_max: f64, low: f64, high: f64) -> (RangeScale, (f64, f64)) {
        let scale = RangeScale { limit_min, limit_max, step: self.step };
        let range = scale.clamp(low, high);
        (scale, range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rounding ties to even: 0.5 → 0, 1.5 → 2, 2.5 → 2, -2.5 → -2.
    #[test]
    fn py_round_ties_to_even() {
        assert_eq!(py_round(0.5), 0.0);
        assert_eq!(py_round(1.5), 2.0);
        assert_eq!(py_round(2.5), 2.0);
        assert_eq!(py_round(-2.5), -2.0);
        assert_eq!(py_round(2.5000001), 3.0);
        assert!(py_round(-0.4) == 0.0 && py_round(-0.4).is_sign_negative());
        assert_eq!(py_int(-7.9), -7.0);
    }

    /// A slider's change: 0..20 step 1 (effect sliders) at fraction 0.375 lands on 7.5 and rounds to 8.
    #[test]
    fn slider_rounds_and_clamps() {
        let effect = SliderScale::new(0.0, 20.0, 1.0);
        assert_eq!(effect.value_at(0.375), 8.0);
        assert_eq!(effect.value_at(0.325), 6.0); // 6.5 → 6
        assert_eq!(effect.value_at(1.2), 20.0);
        assert_eq!(effect.value_at(-0.1), 0.0);
        let boost = SliderScale::new(1.0, 100.0, 1.0);
        assert_eq!(boost.snap(0.0), 1.0, "set_slider of 0 on the Boost slider stores the minimum");
        let reaction = SliderScale::new(70.0, 100.0, 1.0);
        assert_eq!(reaction.snap(100.0), 100.0);
        assert_eq!(reaction.snap(10.0), 70.0);
    }

    /// The pointer moved by a key: one step per arrow press, clamped at the ends.
    #[test]
    fn arrow_keys_move_one_step() {
        let test = SliderScale::new(-100.0, 100.0, 1.0);
        assert_eq!(test.step_from(0.0, 1.0), 1.0);
        assert_eq!(test.step_from(-100.0, -1.0), -100.0);
        let smoothing = SliderScale::new(0.0, 100.0, 5.0);
        assert_eq!(smoothing.step_from(50.0, 1.0), 55.0);
        assert_eq!(smoothing.snap(52.5), 50.0, "10.5 steps round to 10");
    }

    /// Setting a range slider's values: the upper bound compares with the requested lower bound.
    #[test]
    fn range_orders_like_set_range() {
        let wind = RangeScale::new(0.0, 100.0, 1.0);
        assert_eq!(wind.snap(10.0, 50.0), (10.0, 50.0));
        assert_eq!(wind.snap(60.0, 40.0), (40.0, 60.0));
        assert_eq!(wind.clamp(-5.0, 120.0), (0.0, 100.0));
        assert_eq!(wind.clamp(150.0, 120.0), (120.0, 100.0), "a lower bound above the limit is not clamped to it");
        let leds = RangeScale::new(1.0, 150.0, 1.0);
        let (scale, range) = leds.with_limits(1.0, 78.0, 51.0, 120.0);
        assert_eq!(scale.limit_max, 78.0);
        assert_eq!(range, (51.0, 78.0));
    }
}

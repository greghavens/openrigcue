//! Per-axis moving-average smoothing of the stream path of the mixing stage.

use std::collections::VecDeque;

use crate::axis::{Axis, PerAxis};
use crate::error::{mh, CueError};
use crate::mixing::cmp_frame;
use crate::platform::SLOTS;
use crate::pymath::{round_half_even, Num};
use crate::pyvalue::{Cmp, Op, PyValue};
use crate::views::{config_bool, config_float, config_integer, game_item2, Sources};

/// Length of each per-slot, per-axis history (a first-in, first-out list).
pub(crate) const HISTORY_LEN: usize = 25;

/// The smoothed axis values of past frames, per output slot.
#[derive(Debug, Clone)]
pub(crate) struct FormulaHistory {
    slots: [PerAxis<VecDeque<Num>>; SLOTS],
}

impl Default for FormulaHistory {
    fn default() -> Self {
        FormulaHistory { slots: std::array::from_fn(|_| PerAxis::splat(VecDeque::with_capacity(HISTORY_LEN))) }
    }
}

impl FormulaHistory {
    pub fn of(&self, slot: usize, axis: Axis) -> &VecDeque<Num> {
        &self.slots[slot][axis]
    }

    /// Appends every axis value of `slot`, keeping the newest 25.
    pub fn push(&mut self, slot: usize, values: &PerAxis<Num>) {
        for axis in Axis::ALL {
            let fifo = &mut self.slots[slot][axis];
            if fifo.len() >= HISTORY_LEN {
                fifo.pop_front();
            }
            fifo.push_back(values[axis]);
        }
    }
}

/// The moving average of one axis value over its history.
///
/// The window covers `window / 5` (truncated) past values plus the current one, and only applies once the
/// history is strictly longer than that; with exactly that many values the current value passes
/// unsmoothed. A window below 5 (zero steps) passes `(0 + current) / 1`. The sum starts from the
/// integer 0 and adds the newest values first.
pub(crate) fn moving_average(current: Num, history: &VecDeque<Num>, window: &PyValue) -> Result<Num, CueError> {
    let at = |line: u32| move |e: CueError| e.at(mh("calculate_moving_average", line));
    let steps = window.binop(Op::Div, &PyValue::Float(5.0)).and_then(|v| v.to_integer_value()).map_err(at(132))?;
    let len = history.len() as i128;
    if len == 0 || PyValue::Int(len).compare(Cmp::Lt, &steps).map_err(at(134))? {
        return Ok(current);
    }
    let mut total = Num::Int(0);
    for (i, value) in history.iter().rev().enumerate() {
        if !PyValue::Int(i as i128).compare(Cmp::Lt, &steps).map_err(at(143))? {
            let denominator = steps.binop(Op::Add, &PyValue::Int(1)).map_err(at(144))?;
            return match total.add(current) {
                Num::Int(value) => {
                    PyValue::Int(value).binop(Op::Div, &denominator).and_then(|v| v.as_double()).map(Num::Real)
                }
                Num::Real(value) => {
                    PyValue::Float(value).binop(Op::Div, &denominator).and_then(|v| v.as_double()).map(Num::Real)
                }
                complex => complex.div(Num::Real(denominator.as_double().map_err(at(144))?)),
            }
            .map_err(at(144));
        }
        total = total.add(*value);
    }
    Ok(current)
}

/// The speed-scaled smoothing window: a window that falls linearly from `max_window` at
/// standstill to `slider_window` at `max_speed`, limited to that range.
#[cfg(test)]
pub(crate) fn speed_scaled_window(
    speed: f64,
    max_speed: f64,
    slider_window: &PyValue,
    max_window: i128,
) -> Result<PyValue, CueError> {
    speed_scaled_window_native(&PyValue::Float(speed), max_speed, slider_window, &PyValue::Int(max_window))
}

fn speed_scaled_window_native(
    speed: &PyValue,
    max_speed: f64,
    slider_window: &PyValue,
    max: &PyValue,
) -> Result<PyValue, CueError> {
    let at = |line: u32| move |e: CueError| e.at(mh("calc_auto_smoothing", line));
    let raw = slider_window
        .binop(Op::Sub, max)
        .and_then(|v| v.binop(Op::Div, &PyValue::Float(max_speed)))
        .and_then(|v| v.binop(Op::Mul, speed))
        .and_then(|v| max.binop(Op::Add, &v))
        .map_err(at(166))?;
    let rounded = PyValue::Float(round_half_even(raw.as_double().map_err(at(166))?).map_err(at(166))?)
        .to_integer_value()
        .map_err(at(166))?;
    let window = rounded.binop(Op::Add, &PyValue::Int(1)).map_err(at(166))?;
    if window.compare(Cmp::Lt, slider_window).map_err(at(168))? {
        return Ok(slider_window.clone());
    }
    if window.compare(Cmp::Gt, max).map_err(at(171))? {
        return Ok(max.clone());
    }
    Ok(window)
}

/// The order in which the smoothing loop visits the effects.
pub(crate) const SMOOTHING_ORDER: [Axis; Axis::COUNT] = [
    Axis::Roll,
    Axis::Pitch,
    Axis::Heave,
    Axis::Sway,
    Axis::Surge,
    Axis::TractionLoss,
    Axis::TractionLossFront,
    Axis::Yaw,
];

/// What decides the smoothing windows of one effect.
pub(crate) struct WindowContext<'a> {
    pub sources: Sources<'a>,
    /// Whether soft start/stop is engaged when the window is chosen.
    pub soft_start_engaged: bool,
    /// The gear smoothing value while gear smoothing is active.
    pub gear_smoothing: Option<PyValue>,
    pub speed: PyValue,
}

/// The smoothing-window choice of the mixing stage for one effect.
#[cfg(test)]
pub(crate) fn smoothing_window(axis: Axis, ctx: &WindowContext<'_>) -> Result<PyValue, CueError> {
    smoothing_window_with_workers(axis, ctx, None)
}

pub(crate) fn smoothing_window_with_workers(
    axis: Axis,
    ctx: &WindowContext<'_>,
    workers: Option<&crate::worker_smoothing::SmoothingWorkers>,
) -> Result<PyValue, CueError> {
    let s = &ctx.sources;
    let at = |line: u32| move |e: CueError| e.at(cmp_frame(line));
    if workers.map_or(ctx.soft_start_engaged, |workers| workers.soft_start_engaged()) {
        return config_integer(s.config, s.manufacturer, "soft_start_stop_factor").map_err(at(422));
    }
    let mut window = game_item2(s.game, "motion_effect_smoothing_factor", axis.name()).map_err(at(424))?;
    let gear = if axis == Axis::Surge {
        workers.map_or_else(|| ctx.gear_smoothing.clone(), |workers| workers.gear_smoothing_value())
    } else {
        None
    };
    if let Some(gear) = gear {
        if window.equals_checked(&PyValue::Int(0)).map_err(at(429))? {
            let inflation = gear
                .binop(Op::Div, &PyValue::Int(50))
                .and_then(|v| PyValue::Int(1).binop(Op::Add, &v))
                .map_err(at(430))?;
            window = PyValue::Int(5).binop(Op::Mul, &inflation).map_err(at(430))?;
        } else if window.compare(Cmp::Gt, &PyValue::Int(0)).map_err(at(431))? {
            let inflation = gear
                .binop(Op::Div, &PyValue::Int(50))
                .and_then(|v| PyValue::Int(1).binop(Op::Add, &v))
                .map_err(at(432))?;
            window = window.binop(Op::Mul, &inflation).map_err(at(432))?;
        }
    }
    let mode = config_integer(s.config, "CUSTOM_FEATURES", "dams_front_tl_mode").map_err(at(434))?;
    if (mode.equals(&PyValue::Int(1)) || mode.equals(&PyValue::Int(2)))
        && matches!(axis, Axis::TractionLoss | Axis::TractionLossFront)
        && !matches!(ctx.speed.to_native().resolved(), openrigcue_model::native_value::LegacyValue::None)
    {
        let max_speed =
            config_float(s.config, "CUSTOM_FEATURES", "dams_tl_auto_smoothing_max_speed_kmh").map_err(at(437))?;
        let max_window = config_integer(s.config, "CUSTOM_FEATURES", "dams_tl_max_auto_smoothing").map_err(at(438))?;
        window = speed_scaled_window_native(&ctx.speed, max_speed / 3.6, &window, &max_window).map_err(at(438))?;
    }
    if matches!(ctx.speed.to_native().resolved(), openrigcue_model::native_value::LegacyValue::None) {
        return Ok(window);
    }
    let low_speed = config_bool(s.config, "MOTIONBOX", "auto_smoothing_low_speed").map_err(at(441))?;
    if low_speed
        && ctx
            .speed
            .compare(
                Cmp::Lt,
                &PyValue::Float(config_float(s.config, "MOTIONBOX", "auto_smoothing_speed").map_err(at(442))?),
            )
            .map_err(at(442))?
    {
        window = config_integer(s.config, "MOTIONBOX", "auto_smoothing_slider").map_err(at(443))?;
    }
    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::MotionSettings;
    use crate::tune::MotionTune;
    use crate::views::testing::{raw_game, set_game};
    use crate::views::{ConfigView, GameConfigView};

    fn hist(values: &[f64]) -> VecDeque<Num> {
        values.iter().map(|&v| Num::Real(v)).collect()
    }

    fn w(v: f64) -> PyValue {
        PyValue::Float(v)
    }

    #[test]
    fn calculate_moving_average_needs_more_history_than_steps() {
        // window 20 -> 4 steps.
        assert_eq!(moving_average(Num::Real(10.0), &hist(&[]), &w(20.0)), Ok(Num::Real(10.0)));
        assert_eq!(moving_average(Num::Real(10.0), &hist(&[1.0, 2.0, 3.0]), &w(20.0)), Ok(Num::Real(10.0)));
        // Exactly 4 values: the loop runs out, current value unsmoothed.
        assert_eq!(moving_average(Num::Real(10.0), &hist(&[1.0, 2.0, 3.0, 4.0]), &w(20.0)), Ok(Num::Real(10.0)));
        // 5 values: newest 4 (5, 4, 3, 2) plus current over 5.
        assert_eq!(
            moving_average(Num::Real(10.0), &hist(&[1.0, 2.0, 3.0, 4.0, 5.0]), &PyValue::Int(20)),
            Ok(Num::Real(24.0 / 5.0))
        );
    }

    #[test]
    fn calculate_moving_average_small_and_negative_windows() {
        let h = hist(&[1.0, 2.0]);
        // window 4.9 -> 0 steps: (0 + current) / 1.
        assert_eq!(moving_average(Num::Real(7.0), &h, &w(4.9)), Ok(Num::Real(7.0)));
        // window -10 -> -2 steps: (0 + current) / -1.
        assert_eq!(moving_average(Num::Real(7.0), &h, &w(-10.0)), Ok(Num::Real(-7.0)));
        // window -5 -> -1 steps: division by zero, with the operand types' text.
        let e = moving_average(Num::Real(7.0), &h, &w(-5.0)).unwrap_err();
        assert_eq!((e.to_string(), e.frames().last().map(|f| f.line)), ("float division by zero".into(), Some(144)));
        assert_eq!(moving_average(Num::Int(0), &hist(&[]), &w(-5.0)), Ok(Num::Int(0)));
        let ints: VecDeque<Num> = [Num::Int(0)].into();
        assert_eq!(moving_average(Num::Int(0), &ints, &w(-5.0)).unwrap_err().to_string(), "division by zero");
        // Empty history wins over everything.
        assert_eq!(moving_average(Num::Real(7.0), &hist(&[]), &w(-5.0)), Ok(Num::Real(7.0)));
        // A window of 125 or more never averages with the 25-entry FIFO.
        let full = hist(&[1.0; HISTORY_LEN]);
        assert_eq!(moving_average(Num::Real(7.0), &full, &w(125.0)), Ok(Num::Real(7.0)));
        let e = moving_average(Num::Real(7.0), &full, &w(f64::NAN)).unwrap_err();
        assert_eq!(
            (e.to_string(), e.frames().last().map(|f| f.line)),
            ("cannot convert float NaN to integer".into(), Some(132))
        );
        // A window that is not a number fails at the division by 5.
        let e = moving_average(Num::Real(7.0), &full, &PyValue::from("20")).unwrap_err();
        assert_eq!(e.to_string(), "unsupported operand type(s) for /: 'str' and 'float'");
    }

    #[test]
    fn add_hist_calc_keeps_the_newest_25() {
        let mut h = FormulaHistory::default();
        for i in 0..30 {
            h.push(3, &PerAxis::splat(Num::Real(i as f64)));
        }
        let fifo = h.of(3, Axis::Yaw);
        assert_eq!(fifo.len(), 25);
        assert_eq!(fifo.front(), Some(&Num::Real(5.0)));
        assert_eq!(fifo.back(), Some(&Num::Real(29.0)));
        assert!(h.of(2, Axis::Yaw).is_empty());
    }

    #[test]
    fn calc_auto_smoothing_limits_and_rounding() {
        // 80 + (20 - 80) / 22.22 * 0 = 80 -> round 80 + 1 = 81 > 80 -> 80.
        let max_speed = 80.0 / 3.6;
        assert_eq!(speed_scaled_window(0.0, max_speed, &w(20.0), 80), Ok(PyValue::Int(80)));
        // At max speed: 20 + 1 = 21.
        assert_eq!(speed_scaled_window(max_speed, max_speed, &w(20.0), 80), Ok(PyValue::Int(21)));
        // Beyond: below the slider -> slider.
        assert_eq!(speed_scaled_window(100.0, max_speed, &w(20.0), 80), Ok(w(20.0)));
        // Half to even: 80 + (-60 / 10) * 0.75 = 75.5 -> 76, + 1 = 77.
        assert_eq!(speed_scaled_window(0.75, 10.0, &w(20.0), 80), Ok(PyValue::Int(77)));
        // 80 - 6 * 1.25 = 72.5 -> 72, + 1 = 73.
        assert_eq!(speed_scaled_window(1.25, 10.0, &w(20.0), 80), Ok(PyValue::Int(73)));
        let e = speed_scaled_window(1.0, 0.0, &w(20.0), 80).unwrap_err();
        assert_eq!((e.to_string(), e.frames().last().map(|f| f.line)), ("float division by zero".into(), Some(166)));
    }

    fn ctx<'a>(settings: &'a dyn ConfigView, tune: &'a dyn GameConfigView) -> WindowContext<'a> {
        WindowContext {
            sources: Sources { config: settings, game: tune, manufacturer: "DOF REALITY", game_running: "dirtrally2" },
            soft_start_engaged: false,
            gear_smoothing: None,
            speed: PyValue::Float(30.0),
        }
    }

    #[test]
    fn smoothing_window_soft_start_overrides_everything() {
        let mut settings = MotionSettings::default();
        let tune = MotionTune::default();
        let mut c = ctx(&settings, &tune);
        c.soft_start_engaged = true;
        c.gear_smoothing = Some(PyValue::Int(95));
        assert_eq!(smoothing_window(Axis::Surge, &c), Ok(PyValue::Int(30)));
        settings.manufacturer.soft_start_stop_factor = None;
        let mut c = ctx(&settings, &tune);
        c.soft_start_engaged = true;
        let e = smoothing_window(Axis::Roll, &c).unwrap_err();
        assert_eq!((e.to_string(), e.frames().last().map(|f| f.line)), ("'soft_start_stop_factor'".into(), Some(422)));
    }

    #[test]
    fn smoothing_window_gear_inflates_surge_only() {
        let settings = MotionSettings::default();
        let mut tune = MotionTune::default();
        let mut c = ctx(&settings, &tune);
        c.gear_smoothing = Some(PyValue::Int(95));
        assert_eq!(smoothing_window(Axis::Surge, &c), Ok(w(20.0 * (1.0 + 95.0 / 50.0))));
        assert_eq!(smoothing_window(Axis::Sway, &c), Ok(w(20.0)));
        tune.effect_smoothing[Axis::Surge] = 0.0;
        let mut c = ctx(&settings, &tune);
        c.gear_smoothing = Some(PyValue::Int(50));
        assert_eq!(smoothing_window(Axis::Surge, &c), Ok(w(10.0)));
        tune.effect_smoothing[Axis::Surge] = -3.0;
        let mut c = ctx(&settings, &tune);
        c.gear_smoothing = Some(PyValue::Int(50));
        assert_eq!(smoothing_window(Axis::Surge, &c), Ok(w(-3.0)));
        // A window that is not a number fails the `> 0` test.
        let mut raw = raw_game(&MotionTune::default());
        set_game(&mut raw, &["motion_effect_smoothing_factor", "surge"], Some(PyValue::from("20")));
        let mut c = ctx(&settings, &raw);
        c.gear_smoothing = Some(PyValue::Int(50));
        let e = smoothing_window(Axis::Surge, &c).unwrap_err();
        assert_eq!(e.to_string(), "'>' not supported between instances of 'str' and 'int'");
        assert_eq!(e.frames().last().map(|f| f.line), Some(431));
    }

    #[test]
    fn gear_inflation_rounds_integer_division_once_before_multiplication() {
        let settings = MotionSettings::default();
        let mut tune = MotionTune::default();
        tune.effect_smoothing[Axis::Surge] = 0.0;
        let mut context = ctx(&settings, &tune);
        context.gear_smoothing = Some(PyValue::Int(9_007_199_254_740_993));
        let PyValue::Float(actual) = smoothing_window(Axis::Surge, &context).unwrap() else {
            panic!("float smoothing window");
        };
        // Exact integer rational rounding: n/50 => 0x42e47ae147ae147c,
        // then the window is 5 * (1 + quotient).
        assert_eq!(actual.to_bits(), 0x4309_9999_9999_99c3);
    }

    #[test]
    fn smoothing_window_speed_rules() {
        let mut settings = MotionSettings::default();
        settings.traction_loss_auto_smoothing.mode = 2;
        settings.low_speed_smoothing.enabled = true;
        let tune = MotionTune::default();
        let mut c = ctx(&settings, &tune);
        c.speed = PyValue::Float(0.0);
        // Low speed wins after the traction-loss scaling.
        assert_eq!(smoothing_window(Axis::TractionLoss, &c), Ok(PyValue::Int(60)));
        c.speed = PyValue::Float(1.66); // not below: 80 + (-60 / 22.2) · 1.66 = 75.52 -> 76 + 1
        assert_eq!(smoothing_window(Axis::TractionLoss, &c), Ok(PyValue::Int(77)));
        assert_eq!(smoothing_window(Axis::Pitch, &c), Ok(w(20.0)));
    }

    #[test]
    fn smoothing_window_reads_the_configuration_for_every_effect() {
        // The dams_front_tl_mode integer is the first operand: it is read for every effect.
        let mut config = crate::views::testing::raw_config(&MotionSettings::default(), "DOF REALITY");
        config.set("CUSTOM_FEATURES", "dams_front_tl_mode", "off");
        let tune = MotionTune::default();
        let e = smoothing_window(Axis::Pitch, &ctx(&config, &tune)).unwrap_err();
        assert_eq!(
            (e.to_string(), e.frames().last().map(|f| f.line)),
            ("invalid literal for int() with base 10: 'off'".into(), Some(434))
        );
        let mut config = crate::views::testing::raw_config(&MotionSettings::default(), "DOF REALITY");
        config.set("MOTIONBOX", "auto_smoothing_low_speed", "sometimes");
        let e = smoothing_window(Axis::Pitch, &ctx(&config, &tune)).unwrap_err();
        let frames: Vec<_> = e.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("calculate_motor_power", 441), ("str2bool", 261)]);
    }
}

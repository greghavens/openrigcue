//! Travel scaling and frame-to-frame rate shaping of the stream path.
//!
//! Every value is read on every call, at the point of use. The rate shaping functions fall back
//! to defaults where a value cannot be read: a missing
//! `motion_smoothing_factor` / `motion_boost_factor` is logged with its message and
//! trace on every call; the three threshold fallbacks log nothing.

use crate::error::{mh, CueError, Frame};
use crate::mixing::map_arg;
use crate::platform::Platform;
use crate::pymath::rescale;
use crate::pyvalue::{str2bool, Cmp, Op, PyValue};
use crate::views::{config_float, config_integer, config_str, game_item, Sources};
#[cfg(test)]
use num_traits::ToPrimitive;
use openrigcue_model::native_value::LegacyValue;

/// Travel scaling: scale the excursion from the centre by `motion_max_power`
/// (main actuators) or `motion_max_horizontal` (the others), percent: `(pwm - center) *
/// percent / 100 + center`.
pub(crate) fn scale_travel(
    position: f64,
    slot: usize,
    platform: &Platform,
    sources: &Sources<'_>,
) -> Result<f64, CueError> {
    let at = |e: CueError| e.at(mh("apply_max_power", 477));
    let main = match sources.model_field("main_actuators") {
        Ok(Some(PyValue::List(values))) => values.iter().any(|v| v.equals(&PyValue::Int(slot as i128))),
        Ok(None) => platform.scales_with_max_power(slot),
        _ => true,
    };
    let key = if main { "motion_max_power" } else { "motion_max_horizontal" };
    let maximum = sources
        .model_field("max_motor_abs")
        .map_err(|e| e.at(mh("apply_max_power", 476)))?
        .unwrap_or(PyValue::Int(platform.max_motor_abs.into()));
    let centre = maximum
        .binop(Op::Add, &PyValue::Int(1))
        .and_then(|v| v.binop(Op::Div, &PyValue::Int(2)))
        .and_then(|v| v.to_integer_value())
        .map_err(|e| e.at(mh("apply_max_power", 476)))?;
    let excursion = PyValue::Int(position as i128).binop(Op::Sub, &centre).map_err(at)?;
    let percent = game_item(sources.game, key).map_err(at)?;
    let scaled = excursion
        .binop(Op::Mul, &percent)
        .and_then(|v| v.binop(Op::Div, &PyValue::Int(100)))
        .and_then(|v| v.binop(Op::Add, &centre))
        .map_err(at)?;
    scaled.as_double().map_err(at)
}

/// Logs a handled error: the message, then the trace
/// of the error (one frame: the line that failed).
fn log_exception(message: &str, error: &CueError) {
    tracing::error!("{message}\n{}", error.traceback().trim_end_matches('\n'));
}

/// A tune value with the factor fallback of the rate shaping functions (a failed read is logged as
/// `<game> <what> error: <text>` and the fallback is used).
fn factor(sources: &Sources<'_>, key: &str, what: &str, fallback: i128, frame: Frame) -> PyValue {
    match game_item(sources.game, key) {
        Ok(v) => v.into_owned(),
        Err(e) => {
            let e = e.at(frame);
            log_exception(&format!("{}{what} error: {}", sources.game_running, e), &e);
            PyValue::Int(fallback)
        }
    }
}

/// A manufacturer threshold as a float (optionally as a percentage of `max_motor_abs`), or the fallback
/// (nothing is logged: see the module documentation).
fn threshold(sources: &Sources<'_>, key: &str, scale: Option<f64>, fallback: f64) -> f64 {
    match config_float(sources.config, sources.manufacturer, key) {
        Ok(v) => scale.map_or(v, |full| v / 100.0 * full),
        Err(_) => fallback,
    }
}

/// `value > limit`, then the limit where it is exceeded.
fn limited_value(value: PyValue, limit: i128, frame: Frame) -> Result<PyValue, CueError> {
    let limit = PyValue::Int(limit);
    Ok(if value.compare(Cmp::Gt, &limit).map_err(|e| e.at(frame))? { limit } else { value })
}

fn rotary(line: u32) -> Frame {
    mh("apply_ajustment_factor_non_actuators", line)
}

fn actuator(line: u32) -> Frame {
    mh("apply_ajustment_factor_actuators", line)
}

/// Rotary rate shaping (DOF REALITY and Yaw VR).
///
/// Against the previously sent value: a step above the soft-crash threshold keeps `100 − soft_crash_factor`
/// percent of itself, else a step above the smoothing threshold keeps `100 − smoothing_factor` percent;
/// then, judged on the unshaped step, a step below the boost threshold is amplified and clamped to the
/// guard band. The first frame (nothing sent yet) passes through.
#[cfg(test)]
pub(crate) fn shape_rotary(
    position: f64,
    previous: Option<i64>,
    platform: &Platform,
    sources: &Sources<'_>,
) -> Result<i64, CueError> {
    shape_rotary_native(position, previous.as_ref().map(|v| LegacyValue::Int(*v)).as_ref(), platform, sources)?
        .integer()
        .and_then(|v| v.to_i64())
        .ok_or_else(|| CueError::overflow("Python int too large to convert to C long").at(rotary(654)))
}

pub(crate) fn shape_rotary_native(
    position: f64,
    previous: Option<&LegacyValue>,
    platform: &Platform,
    sources: &Sources<'_>,
) -> Result<LegacyValue, CueError> {
    let current_sources = sources.with_current_manufacturer();
    let sources = &current_sources;
    let full = sources
        .model_field("max_motor_abs")
        .map_err(|e| e.at(rotary(588)))?
        .map_or(Ok(platform.max_motor_abs()), |v| v.as_double())?;
    let smoothing_factor = factor(sources, "motion_smoothing_factor", " Smoothing Factor", 50, rotary(591));
    let smoothing_factor = limited_value(smoothing_factor, 99, rotary(598))?;
    let smoothing_threshold = threshold(sources, "smoothing_threshold", Some(full), 0.15 * full);
    let soft_crash_factor =
        config_integer(sources.config, "MOTIONBOX", "soft_crash_factor").map_err(|e| e.at(rotary(607)))?;
    let soft_crash_threshold = threshold(sources, "soft_crash_threshold", Some(full), 0.5 * full);
    let soft_crash_enabled =
        config_str(sources.config, "MOTIONBOX", "soft_crash_enabled").map_err(|e| e.at(rotary(614)))?;
    let boost_factor = factor(sources, "motion_boost_factor", " Boost Factor", 35, rotary(617));
    let boost_factor = limited_value(boost_factor, 35, rotary(622))?;
    let boost_threshold = threshold(sources, "boost_threshold", Some(full), 0.15 * full);
    let percent = sources
        .model_field("motor_max_perc")
        .map_err(|e| e.at(rotary(634)))?
        .map_or(Ok(platform.motor_max_perc), |v| v.as_double())?;
    let guard = (full * (100.0 - percent) / 100.0).trunc();

    let Some(previous) = previous else {
        return to_position(position, rotary(654));
    };
    let previous = previous.py_as_double().map_err(crate::pyvalue::native_error).map_err(|e| e.at(rotary(638)))?;
    let delta = position - previous;
    let step = delta.abs();
    let mut shaped = position;
    if step > soft_crash_threshold && soft_crash_enabled.equals_str("yes") {
        shaped = delta
            * PyValue::Int(100)
                .binop(Op::Sub, &soft_crash_factor)
                .and_then(|v| v.as_double())
                .map_err(|e| e.at(rotary(630)))?
            / 100.0
            + previous;
    } else if step > smoothing_threshold {
        let complement = PyValue::Int(100)
            .binop(Op::Sub, &smoothing_factor)
            .and_then(|v| v.as_double())
            .map_err(|e| e.at(rotary(635)))?;
        shaped = delta * complement / 100.0 + previous;
    }
    if step < boost_threshold {
        let multiplier =
            config_float(sources.config, "MOTIONBOX", "boost_multiplier").map_err(|e| e.at(rotary(647)))?;
        let boost = boost_factor
            .binop(Op::Mul, &PyValue::Float(multiplier))
            .and_then(|v| PyValue::Int(100).binop(Op::Add, &v))
            .and_then(|v| v.as_double())
            .map_err(|e| e.at(rotary(647)))?;
        let value = delta * boost / 100.0 + previous;
        shaped = if value < guard {
            guard
        } else if value > full - guard {
            full - guard
        } else {
            value
        };
    }
    to_position(shaped, rotary(654))
}

/// Linear-actuator rate shaping (every manufacturer except DOF REALITY and Yaw
/// VR).
///
/// A boosted value is also computed here but discarded: the result is the overall
/// smoothing of the unboosted position. The first frame passes through.
#[cfg(test)]
pub(crate) fn shape_actuator(
    position: f64,
    previous: Option<i64>,
    platform: &Platform,
    sources: &Sources<'_>,
) -> Result<i64, CueError> {
    shape_actuator_native(position, previous.as_ref().map(|v| LegacyValue::Int(*v)).as_ref(), platform, sources)?
        .integer()
        .and_then(|v| v.to_i64())
        .ok_or_else(|| CueError::overflow("Python int too large to convert to C long").at(actuator(575)))
}

pub(crate) fn shape_actuator_native(
    position: f64,
    previous: Option<&LegacyValue>,
    platform: &Platform,
    sources: &Sources<'_>,
) -> Result<LegacyValue, CueError> {
    let current_sources = sources.with_current_manufacturer();
    let sources = &current_sources;
    let full = sources
        .model_field("max_motor_abs")
        .map_err(|e| e.at(actuator(489)))?
        .map_or(Ok(platform.max_motor_abs()), |v| v.as_double())?;
    let smoothing_factor = factor(sources, "motion_smoothing_factor", " Smoothing Factor", 50, actuator(492));
    let smoothing_threshold = threshold(sources, "smoothing_threshold", None, 0.01);
    config_integer(sources.config, "MOTIONBOX", "soft_crash_factor").map_err(|e| e.at(actuator(505)))?;
    threshold(sources, "soft_crash_threshold", Some(full), 0.5 * full);
    let soft_crash_enabled =
        config_str(sources.config, "MOTIONBOX", "soft_crash_enabled").map_err(|e| e.at(actuator(512)))?;
    str2bool(&soft_crash_enabled).map_err(|e| e.at(actuator(512)))?;
    let boost_factor = factor(sources, "motion_boost_factor", " Boost Factor", 35, actuator(515));
    let boost_factor = limited_value(boost_factor, 35, actuator(520))?;
    let boost_threshold = threshold(sources, "boost_threshold", Some(full), 0.15 * full);

    let percent = sources
        .model_field("motor_max_perc")
        .map_err(|e| e.at(actuator(532)))?
        .map_or(Ok(platform.motor_max_perc), |v| v.as_double())?;
    let _guard = (full * (100.0 - percent) / 100.0).trunc();
    let Some(previous) = previous else {
        return to_position(position, actuator(575));
    };
    let previous = previous.py_as_double().map_err(crate::pyvalue::native_error).map_err(|e| e.at(actuator(536)))?;
    let delta = position - previous;
    let line = if delta.abs() < boost_threshold {
        // The boosted value is computed (its reads can fail) and then discarded.
        let multiplier =
            config_float(sources.config, "MOTIONBOX", "boost_multiplier").map_err(|e| e.at(actuator(549)))?;
        let boost = boost_factor
            .binop(Op::Mul, &PyValue::Float(multiplier))
            .and_then(|v| PyValue::Int(100).binop(Op::Add, &v))
            .and_then(|v| v.as_double())
            .map_err(|e| e.at(actuator(549)))?;
        let _ = delta * boost / 100.0;
        557
    } else {
        559
    };
    let shaped = overall_smoothing(position, previous, &smoothing_factor, smoothing_threshold, sources)
        .map_err(|e| e.at(actuator(line)))?;
    to_position(shaped, actuator(575))
}

/// Overall smoothing of a position against the previous one.
///
/// Outside the band `previous · (1 ± threshold)` (judged on the ratio `|position / previous|`) only
/// `(100 − m) / 100` of the step is taken, where `m` is the smoothing factor mapped onto
/// `[overall_smoothing_map_min_slider, overall_smoothing_map_max_slider]`.
pub(crate) fn overall_smoothing(
    position: f64,
    previous: f64,
    smoothing_factor: &PyValue,
    threshold: f64,
    sources: &Sources<'_>,
) -> Result<f64, CueError> {
    if previous == 0.0 {
        return Ok(position);
    }
    let at = |e: CueError| e.at(mh("overall_smoothing", 664));
    let min = config_integer(sources.config, "MOTIONBOX", "overall_smoothing_map_min_slider").map_err(at)?;
    let max = config_integer(sources.config, "MOTIONBOX", "overall_smoothing_map_max_slider").map_err(at)?;
    let mapped = rescale(
        map_arg(smoothing_factor).map_err(at)?,
        0.0,
        100.0,
        map_arg(&min).map_err(at)?,
        map_arg(&max).map_err(at)?,
    );
    let kept = (100.0 - mapped) / 100.0;
    let ratio = (position / previous).abs();
    if position > previous {
        if ratio > 1.0 + threshold {
            return Ok(previous + kept * (position - previous));
        }
    } else if ratio < 1.0 - threshold {
        return Ok(previous - kept * (position - previous).abs());
    }
    Ok(position)
}

/// A shaped position truncated to an integer.
fn to_position(value: f64, frame: Frame) -> Result<LegacyValue, CueError> {
    LegacyValue::Float(value)
        .py_integer()
        .map(LegacyValue::from_integer)
        .map_err(crate::pyvalue::native_error)
        .map_err(|e| e.at(frame))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::builtin;
    use crate::settings::MotionSettings;
    use crate::tune::MotionTune;
    use crate::views::testing::{raw_config, raw_game, set_game};
    use crate::views::{ConfigView, GameConfigView};

    fn p3() -> Platform {
        builtin("dof-reality-p3").unwrap().unwrap()
    }

    fn src<'a>(config: &'a dyn ConfigView, game: &'a dyn GameConfigView, manufacturer: &'static str) -> Sources<'a> {
        Sources { config, game, manufacturer, game_running: "dirtrally2" }
    }

    #[test]
    fn apply_max_power_scales_about_the_centre() {
        let p = p3();
        let s = MotionSettings::default();
        let tune = MotionTune { max_power: 50.0, max_horizontal: 10.0, ..MotionTune::default() };
        let sources = src(&s, &tune, "DOF REALITY");
        // centre 4096 / 2 = 2048
        assert_eq!(scale_travel(3048.0, 0, &p, &sources), Ok(2548.0));
        // DOF has no main_actuators: every slot uses max_power.
        assert_eq!(scale_travel(3048.0, 9, &p, &sources), Ok(2548.0));
        let pr = builtin("prosimu-t1000-3m-prs200").unwrap().unwrap();
        assert_eq!(scale_travel(42768.0, 2, &pr, &sources), Ok(32768.0 + 1000.0));
        // An int percentage multiplies the int excursion; a string repeats and fails the division.
        let mut raw = raw_game(&tune);
        set_game(&mut raw, &["motion_max_power"], Some(PyValue::from("50")));
        let e = scale_travel(3048.0, 0, &p, &src(&s, &raw, "DOF REALITY")).unwrap_err();
        assert_eq!(e.to_string(), "unsupported operand type(s) for /: 'str' and 'int'");
    }

    #[test]
    fn apply_ajustment_factor_non_actuators_branches() {
        let p = p3();
        let s = MotionSettings::default();
        let tune = MotionTune { smoothing_factor: 50.0, ..MotionTune::default() };
        let sources = src(&s, &tune, "DOF REALITY");
        // Thresholds on 4095: smoothing 15 % = 614.25, soft crash 50 % = 2047.5, boost 15 % = 614.25.
        assert_eq!(shape_rotary(3000.0, None, &p, &sources), Ok(3000));
        // Large step: soft crash keeps 10 %.
        assert_eq!(shape_rotary(4000.0, Some(1000), &p, &sources), Ok(1300));
        // Medium step above smoothing threshold: keeps 50 %.
        assert_eq!(shape_rotary(2000.0, Some(1000), &p, &sources), Ok(1500));
        // Small step: boost (100 + 5 · 2) % and clamp to [204, 3891].
        assert_eq!(shape_rotary(1100.0, Some(1000), &p, &sources), Ok(1110));
        assert_eq!(shape_rotary(3880.0, Some(3800), &p, &sources), Ok(3888));
        assert_eq!(shape_rotary(3891.0, Some(3800), &p, &sources), Ok(3891));
        assert_eq!(shape_rotary(210.0, Some(205), &p, &sources), Ok(210));
        assert_eq!(shape_rotary(100.0, Some(205), &p, &sources), Ok(204));
        // Exactly at the boost threshold: not boosted, not smoothed (not strictly greater).
        assert_eq!(shape_rotary(1614.25, Some(1000), &p, &sources), Ok(1614));
    }

    #[test]
    fn rotary_integer_soft_crash_complement_precedes_float_conversion() {
        let mut config = raw_config(&MotionSettings::default(), "DOF REALITY");
        config.set("MOTIONBOX", "soft_crash_factor", "9007199254740993");
        config.set("DOF REALITY", "soft_crash_threshold", "0");
        config.set("DOF REALITY", "boost_threshold", "0");
        let tune = MotionTune::default();
        let result = shape_rotary(1064.0, Some(1000), &p3(), &src(&config, &tune, "DOF REALITY"));
        assert_eq!(result, Ok(-5_764_607_523_033_172));
    }

    #[test]
    fn rotary_integer_smoothing_complement_precedes_float_conversion() {
        let mut config = raw_config(&MotionSettings::default(), "DOF REALITY");
        config.set("DOF REALITY", "smoothing_threshold", "0");
        config.set("DOF REALITY", "boost_threshold", "0");
        let mut tune = raw_game(&MotionTune::default());
        set_game(&mut tune, &["motion_smoothing_factor"], Some(PyValue::Int(-18_014_398_509_481_986)));
        let result = shape_rotary(1064.0, Some(1000), &p3(), &src(&config, &tune, "DOF REALITY"));
        assert_eq!(result, Ok(11_529_215_046_069_536));
    }

    #[test]
    fn apply_ajustment_factor_non_actuators_limits_and_flags() {
        let p = p3();
        let mut s = MotionSettings { soft_crash_enabled: false, ..MotionSettings::default() };
        // smoothing factor limited to 99, boost factor to 35
        let tune = MotionTune { smoothing_factor: 150.0, boost_factor: 100.0, ..MotionTune::default() };
        assert_eq!(shape_rotary(4000.0, Some(1000), &p, &src(&s, &tune, "DOF REALITY")), Ok(1030));
        assert_eq!(shape_rotary(1100.0, Some(1000), &p, &src(&s, &tune, "DOF REALITY")), Ok(1170));
        // Missing thresholds fall back to 15 % / 50 % / 15 %.
        s.manufacturer.smoothing_threshold = None;
        s.manufacturer.boost_threshold = None;
        assert_eq!(shape_rotary(1100.0, Some(1000), &p, &src(&s, &tune, "DOF REALITY")), Ok(1170));
        // soft_crash_enabled is compared with 'yes' as written: 'Yes' does not enable it.
        let mut config = raw_config(&MotionSettings::default(), "DOF REALITY");
        config.set("MOTIONBOX", "soft_crash_enabled", "Yes");
        assert_eq!(shape_rotary(4000.0, Some(1000), &p, &src(&config, &tune, "DOF REALITY")), Ok(1030));
        // A factor that cannot be compared with 99 is an error.
        let mut raw = raw_game(&tune);
        set_game(&mut raw, &["motion_smoothing_factor"], Some(PyValue::from("50")));
        let e = shape_rotary(4000.0, None, &p, &src(&s, &raw, "DOF REALITY")).unwrap_err();
        assert_eq!(
            (e.to_string(), e.frames().last().map(|f| f.line)),
            ("'>' not supported between instances of 'str' and 'int'".into(), Some(598))
        );
        // The reads come before the first-frame pass-through: a malformed soft_crash_factor fails even then.
        let mut config = raw_config(&MotionSettings::default(), "DOF REALITY");
        config.set("MOTIONBOX", "soft_crash_factor", "90%");
        let e = shape_rotary(4000.0, None, &p, &src(&config, &tune, "DOF REALITY")).unwrap_err();
        assert_eq!(e.frames().last().map(|f| f.line), Some(607));
    }

    #[test]
    fn missing_rate_factors_fall_back_like_the_original() {
        // Rate shaping: a missing motion_smoothing_factor gives 50, motion_boost_factor 35.
        let p = p3();
        let s = MotionSettings::default();
        let mut raw = raw_game(&MotionTune::default());
        set_game(&mut raw, &["motion_smoothing_factor"], None);
        set_game(&mut raw, &["motion_boost_factor"], None);
        let fallback = MotionTune { smoothing_factor: 50.0, boost_factor: 35.0, ..MotionTune::default() };
        for (position, previous) in [(4000.0, Some(1000)), (2000.0, Some(1000)), (1100.0, Some(1000))] {
            assert_eq!(
                shape_rotary(position, previous, &p, &src(&s, &raw, "DOF REALITY")),
                shape_rotary(position, previous, &p, &src(&s, &fallback, "DOF REALITY"))
            );
        }
    }

    #[test]
    fn apply_ajustment_factor_actuators_uses_overall_smoothing_only() {
        let pr = builtin("prosimu-t1000-5m-prs200").unwrap().unwrap();
        let s = MotionSettings::for_manufacturer(crate::Manufacturer::Prosimu, crate::HostOs::Windows);
        let tune = MotionTune { smoothing_factor: 50.0, ..MotionTune::default() };
        let sources = src(&s, &tune, "PROSIMU");
        assert_eq!(shape_actuator(40000.0, None, &pr, &sources), Ok(40000));
        // 50 mapped from 0..100 onto 90..99 = 94.5 -> keep 5.5 % of the step outside the 1 % band.
        assert_eq!(shape_actuator(40000.0, Some(30000), &pr, &sources), Ok(30550));
        assert_eq!(shape_actuator(20000.0, Some(30000), &pr, &sources), Ok(29450));
        // Inside the band: unchanged, no boost.
        assert_eq!(shape_actuator(30200.0, Some(30000), &pr, &sources), Ok(30200));
        // PT-ACTUATOR threshold 100000: the band never ends.
        let pt = MotionSettings::for_manufacturer(crate::Manufacturer::PtActuator, crate::HostOs::Windows);
        assert_eq!(shape_actuator(40000.0, Some(30000), &pr, &src(&pt, &tune, "PT-ACTUATOR")), Ok(40000));
        // soft_crash_enabled is parsed as a boolean on every call.
        let mut config = raw_config(&s, "PROSIMU");
        config.set("MOTIONBOX", "soft_crash_enabled", "enabled");
        let e = shape_actuator(40000.0, None, &pr, &src(&config, &tune, "PROSIMU")).unwrap_err();
        let frames: Vec<_> = e.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("apply_ajustment_factor_actuators", 512), ("str2bool", 261)]);
        // The discarded boost still reads boost_multiplier as a float for a step below the boost threshold.
        let mut config = raw_config(&s, "PROSIMU");
        config.set("MOTIONBOX", "boost_multiplier", "x2");
        assert!(shape_actuator(40000.0, Some(30000), &pr, &src(&config, &tune, "PROSIMU")).is_ok());
        let e = shape_actuator(30001.0, Some(30000), &pr, &src(&config, &tune, "PROSIMU")).unwrap_err();
        assert_eq!(e.frames().last().map(|f| f.line), Some(549));
        // The smoothing factor reaches the range map, which wants a real number.
        let mut raw = raw_game(&tune);
        set_game(&mut raw, &["motion_smoothing_factor"], Some(PyValue::from("50")));
        let e = shape_actuator(40000.0, Some(30000), &pr, &src(&s, &raw, "PROSIMU")).unwrap_err();
        let frames: Vec<_> = e.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("apply_ajustment_factor_actuators", 559), ("overall_smoothing", 664), ("map", 204)]);
    }

    #[test]
    fn overall_smoothing_edges() {
        let s = MotionSettings::default();
        let tune = MotionTune::default();
        let sources = src(&s, &tune, "DOF REALITY");
        let f = |v: f64| PyValue::Float(v);
        assert_eq!(overall_smoothing(500.0, 0.0, &f(50.0), 0.01, &sources), Ok(500.0));
        // Equal positions: the else branch, ratio 1 not below 0.99.
        assert_eq!(overall_smoothing(500.0, 500.0, &f(50.0), 0.01, &sources), Ok(500.0));
        // Negative previous: ratio uses the absolute value.
        assert_eq!(overall_smoothing(-100.0, -200.0, &f(0.0), 0.01, &sources), Ok(-100.0));
        // Factor 0 maps to 90: 10 % of the step is kept.
        assert_eq!(overall_smoothing(2000.0, 1000.0, &f(0.0), 0.01, &sources), Ok(1100.0));
    }
}

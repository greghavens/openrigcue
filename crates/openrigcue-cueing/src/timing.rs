//! Motion loop timing and telemetry preparation: the stream loop period, the test loop period and
//! the per-frame telemetry checks.

use std::time::Duration;

use openrigcue_model::Telemetry;

use crate::error::{mh, CueError, DivisionKind, Frame};
use crate::pyvalue::{native_error, Cmp, Op, PyValue};
use crate::views::{config_integer, ConfigView};

/// Operating system family, for the refresh-rate caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Windows,
    Linux,
    Other,
}

impl HostOs {
    pub fn current() -> Self {
        if cfg!(windows) {
            HostOs::Windows
        } else if cfg!(target_os = "linux") {
            HostOs::Linux
        } else {
            HostOs::Other
        }
    }
}

/// Highest refresh rate outside Windows (stream) or on Linux (test mode).
const CAPPED_RATE: i128 = 30;
/// Test-mode rate when test mode is not at the motion refresh rate.
const SLOW_TEST_RATE: i128 = 7;

/// Test-loop period while no platform model is selected.
pub const IDLE_TEST_PERIOD: Duration = Duration::from_secs(1);

/// The sleep of one period, `1 / rate` seconds: a division by zero at the division (`division`), a value error
/// of the sleep for a negative duration (`sleep`).
fn period(rate: i128, division: &[Frame], sleep: Frame) -> Result<Duration, CueError> {
    if rate == 0 {
        return Err(division.iter().fold(CueError::zero_division(DivisionKind::Float), |e, f| e.at(*f)));
    }
    let seconds = 1.0 / rate as f64;
    if seconds < 0.0 {
        return Err(CueError::value_error("sleep length must be non-negative").at(sleep));
    }
    openrigcue_model::python_time::sleep_duration(seconds).map_err(|message| CueError::overflow(message).at(sleep))
}

/// The stream loop period: the motion loop sleeps `1 / refresh_rate` seconds after each frame, with the
/// rate capped at 30 Hz on every platform except Windows. The sleep is not compensated for the frame's
/// processing time. An error ends the motion loop thread.
pub fn loop_period(refresh_rate: i64, os: HostOs) -> Result<Duration, CueError> {
    let rate =
        if os != HostOs::Windows && refresh_rate > CAPPED_RATE as i64 { CAPPED_RATE as i64 } else { refresh_rate };
    let caller = controllers("refresh_motionbox", 451);
    compatibility_period(i128::from(rate), &[controllers("get_refresh_rate", 392), caller], caller)
}

fn compatibility_period(rate: i128, division: &[Frame], sleep: Frame) -> Result<Duration, CueError> {
    period(rate, division, sleep)?;
    Ok(Duration::from_secs_f64(1.0 / rate as f64))
}

fn native_period(rate: &PyValue, division: &[Frame], sleep: Frame) -> Result<Duration, CueError> {
    let seconds = PyValue::Float(1.0)
        .binop(Op::Div, rate)
        .and_then(|v| v.as_double())
        .map_err(|error| division.iter().fold(error, |e, f| e.at(*f)))?;
    if seconds < 0.0 {
        return Err(CueError::value_error("sleep length must be non-negative").at(sleep));
    }
    openrigcue_model::python_time::sleep_duration(seconds).map_err(|message| CueError::overflow(message).at(sleep))
}

fn controllers(function: &'static str, line: u32) -> Frame {
    Frame { file: "backend/controllers/controllers.py", prefix: "backend.controllers.controllers.", function, line }
}

/// The per-pass integer read of `MOTIONBOX.refresh_rate`, including its conversion errors.
pub fn loop_period_from_config(config: &dyn ConfigView, os: HostOs) -> Result<Duration, CueError> {
    let rate = config_integer(config, "MOTIONBOX", "refresh_rate")
        .map_err(|error| error.at(controllers("get_refresh_rate", 387)).at(controllers("refresh_motionbox", 451)))?;
    let rate = if os != HostOs::Windows && rate.compare(Cmp::Gt, &PyValue::Int(CAPPED_RATE))? {
        PyValue::Int(CAPPED_RATE)
    } else {
        rate
    };
    let caller = controllers("refresh_motionbox", 451);
    native_period(&rate, &[controllers("get_refresh_rate", 392), caller], caller)
}

/// The test loop period: the motion refresh rate when test mode runs at full
/// rate (`motion_test_mode` is 1), else 7 Hz; capped at 30 Hz on Linux only. The test loop sleeps this
/// long between computing a frame and delivering it. An error ends the test thread.
pub fn test_period(full_rate: bool, refresh_rate: i64, os: HostOs) -> Result<Duration, CueError> {
    let rate = if full_rate { i128::from(refresh_rate) } else { SLOW_TEST_RATE };
    let rate = if rate > CAPPED_RATE && os == HostOs::Linux { CAPPED_RATE } else { rate };
    compatibility_period(rate, &[mh("send_test", 1141)], mh("send_test", 1141))
}

/// The configured refresh rate is read only in full-rate test mode.
pub fn test_period_from_config(full_rate: bool, config: &dyn ConfigView, os: HostOs) -> Result<Duration, CueError> {
    let rate = if full_rate {
        config_integer(config, "MOTIONBOX", "refresh_rate").map_err(|error| error.at(mh("send_test", 1134)))?
    } else {
        PyValue::Int(SLOW_TEST_RATE)
    };
    let rate = if os == HostOs::Linux && rate.compare(Cmp::Gt, &PyValue::Int(CAPPED_RATE))? {
        PyValue::Int(CAPPED_RATE)
    } else {
        rate
    };
    native_period(&rate, &[mh("send_test", 1141)], mh("send_test", 1141))
}

/// The per-frame NaN scan: every NaN channel becomes 0 and is logged.
/// Returns the channels that were NaN.
pub fn clear_telemetry_nan(telemetry: &mut Telemetry) -> Vec<&'static str> {
    let fields = telemetry.clear_nan();
    for field in &fields {
        tracing::error!("found a nan in the telemetry, field: {field}");
    }
    fields
}

/// The NaN test fails for a text, empty or container value. The scan stops there, retaining
/// earlier zero assignments and leaving later values untouched.
pub fn clear_telemetry_nan_checked(telemetry: &mut Telemetry) -> Result<Vec<&'static str>, CueError> {
    let mut cleared = Vec::new();
    for name in telemetry.channel_names().to_vec() {
        if let Some(value) = telemetry.native_value(name) {
            value.py_as_double().map_err(|error| native_error(error).at(controllers("refresh_motionbox", 406)))?;
        }
        if telemetry.get(name).is_some_and(f64::is_nan) {
            if telemetry.native_value(name).is_some() {
                telemetry.set_native(name, openrigcue_model::native_value::LegacyValue::Int(0));
            } else {
                *telemetry.get_mut(name).expect("registry channel") = 0.0;
            }
            tracing::error!("found a nan in the telemetry, field: {name}");
            cleared.push(name);
        }
    }
    Ok(cleared)
}

/// The frame's argument lookups are independent dictionary reads.
/// The helper retains every raw value, including unused landing_gear_nose, before the callee gate.
pub fn capture_motion_arguments(
    mut read: impl FnMut(&'static str) -> Option<openrigcue_model::native_value::LegacyValue>,
) -> Result<Telemetry, CueError> {
    let keys = [
        "roll",
        "pitch",
        "yaw",
        "lateral_acceleration",
        "longitudinal_acceleration",
        "vertical_acceleration",
        "lateral_velocity",
        "gear",
        "wheel_slip_lf",
        "wheel_slip_rf",
        "wheel_slip_lr",
        "wheel_slip_rr",
        "rumble_pitch_lf",
        "rumble_pitch_rf",
        "rumble_pitch_lr",
        "rumble_pitch_rr",
        "canopy",
        "afterburner",
        "landing_gear_left",
        "landing_gear_nose",
        "landing_gear_right",
        "cannon_firing",
        "missile_firing",
        "rpm",
        "max_rpm",
        "abs_lock",
        "speed",
        "front_tl",
    ];
    let mut result = Telemetry::unfilled();
    for (index, key) in keys.into_iter().enumerate() {
        let value =
            read(key).ok_or_else(|| CueError::key(key).at(controllers("refresh_motionbox", 418 + index as u32)))?;
        result.set_native(key, value);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_nan_scan_stops_at_text_and_preserves_the_published_dictionary() {
        use openrigcue_model::native_value::LegacyValue;
        let mut telemetry = Telemetry::unfilled();
        telemetry.set_native("roll", LegacyValue::Float(f64::NAN));
        telemetry.set_native("pitch", LegacyValue::Str("3".into()));
        telemetry.set_native("yaw", LegacyValue::Float(f64::NAN));
        let error = clear_telemetry_nan_checked(&mut telemetry).unwrap_err();
        assert_eq!(error.to_string(), "must be real number, not str");
        assert!(error.traceback().contains("line 406"));
        assert_eq!(telemetry.native_value("roll"), Some(&LegacyValue::Int(0)));
        assert_eq!(telemetry.native_value("pitch"), Some(&LegacyValue::Str("3".into())));
        assert!(telemetry.yaw.is_nan(), "a later numeric NaN was never reached");
        telemetry.set_native("pitch", LegacyValue::Float(3.0));
        assert_eq!(clear_telemetry_nan_checked(&mut telemetry).unwrap(), ["yaw"]);
        assert_eq!(telemetry.native_value("yaw"), Some(&LegacyValue::Int(0)));
    }

    #[test]
    fn get_refresh_rate_caps_outside_windows() {
        assert_eq!(loop_period(30, HostOs::Linux), Ok(Duration::from_secs_f64(1.0 / 30.0)));
        assert_eq!(loop_period(500, HostOs::Linux), Ok(Duration::from_secs_f64(1.0 / 30.0)));
        assert_eq!(loop_period(500, HostOs::Other), Ok(Duration::from_secs_f64(1.0 / 30.0)));
        assert_eq!(loop_period(500, HostOs::Windows), Ok(Duration::from_secs_f64(1.0 / 500.0)));
        assert_eq!(loop_period(20, HostOs::Linux), Ok(Duration::from_secs_f64(1.0 / 20.0)));
        let zero = loop_period(0, HostOs::Linux).unwrap_err();
        assert_eq!(zero.to_string(), "float division by zero");
        assert_eq!(
            zero.traceback(),
            "Traceback (most recent call last):\n  File \"backend/controllers/controllers.py\", line 451, in \
             backend.controllers.controllers.refresh_motionbox\n  File \"backend/controllers/controllers.py\", line \
             392, in backend.controllers.controllers.get_refresh_rate\nZeroDivisionError: float division by zero\n"
        );
        assert_eq!(loop_period(-5, HostOs::Windows).unwrap_err().to_string(), "sleep length must be non-negative");
    }

    #[test]
    fn native_motion_and_test_sleep_round_up_fractional_nanoseconds() {
        use crate::{NativeConfig, PyValue};
        for (rate, nanoseconds) in [(30, 33_333_334_u128), (7, 142_857_143), (90, 11_111_112), (500, 2_000_000)] {
            let config = NativeConfig(PyValue::Dict(vec![(
                "MOTIONBOX".into(),
                PyValue::Dict(vec![("refresh_rate".into(), PyValue::Int(rate))]),
            )]));
            let expected = if cfg!(windows) { nanoseconds.div_ceil(100) * 100 } else { nanoseconds };
            assert_eq!(loop_period_from_config(&config, HostOs::Windows).unwrap().as_nanos(), expected);
            assert_eq!(test_period_from_config(true, &config, HostOs::Windows).unwrap().as_nanos(), expected);
        }
    }

    #[test]
    fn send_test_pacing() {
        assert_eq!(test_period(false, 500, HostOs::Linux), Ok(Duration::from_secs_f64(1.0 / 7.0)));
        assert_eq!(test_period(true, 500, HostOs::Linux), Ok(Duration::from_secs_f64(1.0 / 30.0)));
        // Only Linux is capped in test mode.
        assert_eq!(test_period(true, 500, HostOs::Other), Ok(Duration::from_secs_f64(1.0 / 500.0)));
        assert_eq!(test_period(true, 0, HostOs::Linux).unwrap_err().to_string(), "float division by zero");
    }

    #[test]
    fn native_refresh_rate_conversion_happens_at_the_original_statement() {
        use crate::{NativeConfig, PyValue};
        let config = |value| {
            NativeConfig(PyValue::Dict(vec![("MOTIONBOX".into(), PyValue::Dict(vec![("refresh_rate".into(), value)]))]))
        };
        let invalid = config(PyValue::Str("1.5".into()));
        let error = loop_period_from_config(&invalid, HostOs::Windows).unwrap_err();
        assert_eq!(error.to_string(), "invalid literal for int() with base 10: '1.5'");
        assert_eq!(
            error.frames().map(|frame| (frame.function, frame.line)).collect::<Vec<_>>(),
            [("refresh_motionbox", 451), ("get_refresh_rate", 387)]
        );
        assert_eq!(
            test_period_from_config(false, &invalid, HostOs::Windows),
            openrigcue_model::python_time::sleep_duration(1.0 / 7.0).map_err(CueError::overflow)
        );
        assert_eq!(
            test_period_from_config(true, &invalid, HostOs::Windows).unwrap_err().frames().next().unwrap().line,
            1134
        );
        assert_eq!(
            loop_period_from_config(&config(PyValue::Float(2.9)), HostOs::Windows),
            Ok(Duration::from_millis(500))
        );
        assert_eq!(
            loop_period_from_config(&config(PyValue::Str("100000000000000000000".into())), HostOs::Linux),
            openrigcue_model::python_time::sleep_duration(1.0 / 30.0).map_err(CueError::overflow)
        );
    }

    #[test]
    fn refresh_motionbox_nan_scan() {
        let mut t = Telemetry::off();
        t.roll = f64::NAN;
        t.abs_lock = f64::NAN;
        t.pitch = f64::INFINITY;
        assert_eq!(clear_telemetry_nan(&mut t), vec!["roll", "abs_lock"]);
        assert_eq!(t.roll, 0.0);
        assert_eq!(t.pitch, f64::INFINITY);
    }
}

//! Wind simulator: fan PWM from vehicle speed, with steering-dependent left/right balance ("wind
//! curving") and the ejection-seat blast, for the Intellibox (10-bit PWM) and SIM3D (8-bit PWM) fans.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::numeric::{map_clamped, round_to_hundredths, truncate, DivisionByZero};

/// PWM full scale of the Intellibox fan outputs.
pub const INTELLIBOX_PWM_RANGE: f64 = 1023.0;
/// PWM full scale of SIM3D fan ports.
pub const SIM3D_PWM_RANGE: f64 = 255.0;
/// Fan count the dispatcher passes for SIM3D.
pub const SIM3D_FAN_COUNT: i64 = 3;

/// The three-state wind switch of the sidebar (one for the side fans,
/// one for the centre fan): 0 off, 1 follow telemetry, 2 always on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanMode {
    Off,
    Auto,
    AlwaysOn,
}

impl FanMode {
    /// Only 2 and 0 are special, so any other value follows telemetry.
    pub fn from_status(status: i64) -> Self {
        match status {
            2 => FanMode::AlwaysOn,
            0 => FanMode::Off,
            _ => FanMode::Auto,
        }
    }

    fn ratio(self, telemetry_ratio: f64) -> f64 {
        match self {
            FanMode::AlwaysOn => 1.0,
            FanMode::Off => 0.0,
            FanMode::Auto => telemetry_ratio,
        }
    }
}

/// Per-game wind tuning (the tune keys of the Wind page).
#[derive(Debug, Clone, PartialEq)]
pub struct WindTuning {
    /// `power_wind_min_speed_perc` / `power_wind_max_speed_perc` (0..100) of the left/right fans.
    pub min_percent: f64,
    pub max_percent: f64,
    /// `power_center_wind_min_speed_perc` / `power_center_wind_max_speed_perc` of the centre fan.
    pub center_min_percent: f64,
    pub center_max_percent: f64,
    /// `speed_factor` (1..10): exponent of the speed curve.
    pub speed_factor: f64,
    /// `wind_curving == 1`.
    pub curving: bool,
    /// `curving_effect` (1..10): exponent of the steering reduction.
    pub curving_effect: f64,
    /// `enable_ejection_seat` as a boolean.
    pub ejection_seat: bool,
    /// The raw speed-factor value. A failed required read/conversion is
    /// handled where the speed exponent is actually reached.
    pub native_speed_factor: Option<NativeSpeedFactor>,
    /// Raw required switch read and curving value, consumed only by
    /// the multi-fan curving branch after the speed calculation.
    pub native_curving: Option<NativeCurving>,
    /// Required profile read and boolean conversion, reached only for an
    /// ejection request and before the previous blast timestamp is changed.
    pub native_ejection: Option<Result<bool, NativeWindError>>,
    /// The caller reads the four bound values before the fan calculation.
    /// Retain that required-read error without coercing present values here.
    pub native_arguments: Option<Result<(), NativeWindError>>,
    /// Present raw bound arguments; their arithmetic is reached separately
    /// from the caller's required reads.
    pub native_bounds: Option<NativeWindBounds>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeWindBounds {
    pub minimum: LegacyValue,
    pub maximum: LegacyValue,
    pub center_minimum: LegacyValue,
    pub center_maximum: LegacyValue,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NativeSpeedFactor {
    Number(f64),
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeCurving {
    pub enabled: Result<bool, NativeWindError>,
    pub factor: NativeSpeedFactor,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct NativeWindError {
    pub class: &'static str,
    pub message: String,
}

/// What the fan calculation reads from telemetry and runtime parameters on each call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindInputs {
    pub speed_kmh: f64,
    pub max_speed_kmh: f64,
    pub steering_angle: f64,
    pub steering_max_angle: f64,
    /// Percentage of the wheel's maximum angle at
    /// which the outer fan reaches zero.
    pub dual_max_steering_percent: f64,
    pub left_right_mode: FanMode,
    pub center_mode: FanMode,
    /// The ejection seat was requested (set by the ejection-seat hotkey).
    pub ejection_requested: bool,
}

/// PWM levels for the three fan outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanLevels {
    pub left: i64,
    pub right: i64,
    pub center: i64,
}

/// Why a frame could not be computed; the dispatcher skips the frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WindError {
    #[error(transparent)]
    Native(#[from] NativeWindError),
    #[error("unsupported operand type(s) for /: '{0}' and '{1}'")]
    UnsupportedDivision(&'static str, &'static str),
    #[error("unsupported operand type(s) for *: '{0}' and 'float'")]
    UnsupportedSteeringScale(&'static str),
    #[error("bad operand type for abs(): '{0}'")]
    UnsupportedSteeringAbs(&'static str),
    #[error("max speed is zero")]
    ZeroMaxSpeed,
    /// A negative speed ratio raised to a fractional exponent has no real value.
    #[error("negative speed ratio with a fractional exponent")]
    ComplexPower,
    #[error("fan level is not a finite number")]
    NotFinite,
    #[error("{message}")]
    DiagnosticOutput { kind: std::io::ErrorKind, message: String },
}

impl From<std::io::Error> for WindError {
    fn from(error: std::io::Error) -> Self {
        Self::DiagnosticOutput { kind: error.kind(), message: error.to_string() }
    }
}

impl From<DivisionByZero> for WindError {
    fn from(_: DivisionByZero) -> Self {
        WindError::ZeroMaxSpeed
    }
}

/// Seconds of full blast after the ejection seat fires, then a linear fade to zero over the same time.
const EJECTION_FULL_SECONDS: f64 = 20.0;
const EJECTION_END_SECONDS: f64 = 40.0;

/// `base` raised to `exponent`, rejecting the cases without a real result (a negative base with a
/// fractional exponent) and zero raised to a negative exponent.
fn power(base: f64, exponent: f64) -> Result<f64, WindError> {
    if base < 0.0 && exponent.fract() != 0.0 {
        return Err(WindError::ComplexPower);
    }
    if base == 0.0 && exponent < 0.0 {
        return Err(WindError::ZeroMaxSpeed);
    }
    let result = base.powf(exponent);
    if base.is_finite() && exponent.is_finite() && result.is_infinite() {
        return Err(native_operation_error("OverflowError: (34, 'Result too large')".into()));
    }
    Ok(result)
}

/// The local wall-clock time as the wind timer reads it.
pub type WallClock = std::sync::Arc<dyn Fn() -> chrono::NaiveDateTime + Send + Sync>;

/// Speed-to-wind state that survives between frames: when the ejection seat last fired
/// (a local wall-clock time).
#[derive(Clone)]
pub struct WindSimulator {
    ejection_started: Option<chrono::NaiveDateTime>,
    wall: WallClock,
}

impl Default for WindSimulator {
    fn default() -> Self {
        WindSimulator { ejection_started: None, wall: std::sync::Arc::new(super::link::local_now) }
    }
}

impl std::fmt::Debug for WindSimulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindSimulator").field("ejection_started", &self.ejection_started).finish_non_exhaustive()
    }
}

/// Fan percentages (0..100 nominal) before the PWM mapping.
struct FanPercent {
    left: FanPercentValue,
    right: FanPercentValue,
    center: FanPercentValue,
}

#[derive(Clone)]
enum FanPercentValue {
    Number(f64),
    Native(LegacyValue),
}

impl FanPercentValue {
    fn multiply_float(&mut self, factor: f64) -> Result<(), WindError> {
        match self {
            Self::Number(value) => *value *= factor,
            Self::Native(value) => {
                *value = value.py_mul(&LegacyValue::Float(factor)).map_err(native_operation_error)?
            }
        }
        Ok(())
    }
}

fn native_operation_error(error: String) -> WindError {
    let (class, message) = error.split_once(": ").unwrap_or((&error, ""));
    let class = match class {
        "TypeError" => "TypeError",
        "OverflowError" => "OverflowError",
        "ZeroDivisionError" => "ZeroDivisionError",
        "MemoryError" => "MemoryError",
        _ => "ValueError",
    };
    NativeWindError { class, message: message.to_owned() }.into()
}

fn multiply_maximum(
    power: f64,
    integer_power: bool,
    native: Option<&LegacyValue>,
    projected: f64,
) -> Result<FanPercentValue, WindError> {
    let Some(value) = native else { return Ok(FanPercentValue::Number(power * projected)) };
    let power = if integer_power { LegacyValue::Int(power as i64) } else { LegacyValue::Float(power) };
    power.py_mul(value).map(FanPercentValue::Native).map_err(native_operation_error)
}

impl WindSimulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// A simulator whose wall-clock reads come from `wall` (tests and a feed's own clock).
    pub fn with_wall_clock(wall: WallClock) -> Self {
        WindSimulator { ejection_started: None, wall }
    }

    /// Fan PWM levels for one frame.
    ///
    /// While the ejection effect
    /// runs both maxima are overridden and the speed ratio is 1; otherwise the ratio is `speed / max_speed`
    /// shaped by the sidebar modes; then the ratio raised to the speed factor (1..10 mapped to 1..5) times max%, the steering
    /// reduction of the outer fan, and the min%-floored PWM mapping.
    pub fn fan_levels(
        &mut self,
        fan_count: i64,
        pwm_range: f64,
        inputs: &WindInputs,
        tuning: &WindTuning,
    ) -> Result<FanLevels, WindError> {
        self.fan_levels_impl(fan_count, pwm_range, inputs, tuning, None)
    }

    pub(crate) fn fan_levels_for_telemetry(
        &mut self,
        fan_count: i64,
        pwm_range: f64,
        inputs: &WindInputs,
        tuning: &WindTuning,
        telemetry: &Telemetry,
    ) -> Result<FanLevels, WindError> {
        self.fan_levels_impl(fan_count, pwm_range, inputs, tuning, Some(telemetry))
    }

    fn fan_levels_impl(
        &mut self,
        fan_count: i64,
        pwm_range: f64,
        inputs: &WindInputs,
        tuning: &WindTuning,
        telemetry: Option<&Telemetry>,
    ) -> Result<FanLevels, WindError> {
        self.fan_levels_with_output(fan_count, pwm_range, inputs, tuning, telemetry, &mut |text| {
            use std::io::Write;
            std::io::stdout().lock().write_all(text.as_bytes())
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn fan_levels_with_output(
        &mut self,
        fan_count: i64,
        pwm_range: f64,
        inputs: &WindInputs,
        tuning: &WindTuning,
        telemetry: Option<&Telemetry>,
        output: &mut dyn FnMut(&str) -> std::io::Result<()>,
    ) -> Result<FanLevels, WindError> {
        if let Some(arguments) = &tuning.native_arguments {
            arguments.clone()?;
        }
        let mut percent = self.speed_percent(inputs, tuning, telemetry, output)?;
        let curving = if fan_count == 1 {
            false
        } else if let Some(native) = &tuning.native_curving {
            native.enabled.clone()?
        } else {
            tuning.curving
        };
        if curving {
            apply_curving(&mut percent, inputs, tuning, telemetry, output)?;
        }
        Ok(FanLevels {
            left: to_pwm_value(
                percent.left,
                tuning.min_percent,
                tuning.native_bounds.as_ref().map(|b| &b.minimum),
                pwm_range,
            )?,
            right: to_pwm_value(
                percent.right,
                tuning.min_percent,
                tuning.native_bounds.as_ref().map(|b| &b.minimum),
                pwm_range,
            )?,
            center: to_pwm_value(
                percent.center,
                tuning.center_min_percent,
                tuning.native_bounds.as_ref().map(|b| &b.center_minimum),
                pwm_range,
            )?,
        })
    }

    fn speed_percent(
        &mut self,
        inputs: &WindInputs,
        tuning: &WindTuning,
        telemetry: Option<&Telemetry>,
        output: &mut dyn FnMut(&str) -> std::io::Result<()>,
    ) -> Result<FanPercent, WindError> {
        if inputs.ejection_requested {
            let enabled = match &tuning.native_ejection {
                Some(enabled) => enabled.clone()?,
                None => tuning.ejection_seat,
            };
            if enabled {
                self.ejection_started = Some((self.wall)());
            }
        }
        let blast = self.ejection_blast();
        let ratio = if blast.is_some() {
            1.0
        } else {
            if let Some(telemetry) = telemetry {
                checked_speed_division(telemetry, inputs)?
            } else {
                if inputs.max_speed_kmh == 0.0 {
                    return Err(WindError::ZeroMaxSpeed);
                }
                inputs.speed_kmh / inputs.max_speed_kmh
            }
        };
        let exponent = match tuning.native_speed_factor {
            Some(NativeSpeedFactor::Failed) => {
                output("failed to calculate the xpo_speed / speed_Factor\n")?;
                1.0
            }
            Some(NativeSpeedFactor::Number(value)) => map_clamped(value, 1.0, 10.0, 1.0, 5.0),
            None => map_clamped(tuning.speed_factor, 1.0, 10.0, 1.0, 5.0),
        };
        if let Some(blast) = blast {
            return Ok(FanPercent {
                left: FanPercentValue::Number(blast),
                right: FanPercentValue::Number(blast),
                center: FanPercentValue::Number(blast),
            });
        }
        let integer_exponent = tuning.native_speed_factor == Some(NativeSpeedFactor::Failed);
        let side = multiply_maximum(
            power(inputs.left_right_mode.ratio(ratio), exponent)?,
            integer_exponent && inputs.left_right_mode != FanMode::Auto,
            tuning.native_bounds.as_ref().map(|b| &b.maximum),
            tuning.max_percent,
        )?;
        let center = multiply_maximum(
            power(inputs.center_mode.ratio(ratio), exponent)?,
            integer_exponent && inputs.center_mode != FanMode::Auto,
            tuning.native_bounds.as_ref().map(|b| &b.center_maximum),
            tuning.center_max_percent,
        )?;
        Ok(FanPercent { left: side.clone(), right: side, center })
    }

    /// The ejection-seat override percentage (100 for 20 s, then a linear fade to 0 at 40 s); clears the
    /// effect once it has run out. Ratio 1 to any power is 1, so this is the fan percentage directly.
    /// The elapsed time comes from a second wall-clock read,
    /// negative after the clock is set back.
    fn ejection_blast(&mut self) -> Option<f64> {
        let started = self.ejection_started?;
        let elapsed = super::link::total_seconds((self.wall)(), started);
        if elapsed >= EJECTION_END_SECONDS {
            self.ejection_started = None;
            return None;
        }
        if elapsed > EJECTION_FULL_SECONDS {
            let fade = (elapsed - EJECTION_FULL_SECONDS) * 100.0 / (EJECTION_END_SECONDS - EJECTION_FULL_SECONDS);
            Some(100.0 - fade)
        } else {
            Some(100.0)
        }
    }
}

/// The division comes before either sidebar mode is applied. Nonnumeric
/// raw values fail there even when Off/Always On ignores the ratio.
/// The ejection branch skips this operation, so the check follows that branch.
fn checked_speed_division(telemetry: &Telemetry, inputs: &WindInputs) -> Result<f64, WindError> {
    let speed = telemetry.native_value("speed_kmh").cloned().unwrap_or(LegacyValue::Float(inputs.speed_kmh));
    let maximum = telemetry.native_value("max_speed_kmh").cloned().unwrap_or(LegacyValue::Float(inputs.max_speed_kmh));
    speed.py_truediv(&maximum).and_then(|value| value.py_as_double()).map_err(|error| {
        if error.starts_with("TypeError: unsupported operand type(s) for /:") {
            WindError::UnsupportedDivision(native_kind(&speed), native_kind(&maximum))
        } else if error.starts_with("ZeroDivisionError:") {
            WindError::ZeroMaxSpeed
        } else {
            native_operation_error(error)
        }
    })
}

fn native_kind(value: &LegacyValue) -> &'static str {
    if matches!(value, LegacyValue::DateTime(_)) {
        "datetime.datetime"
    } else {
        value.type_name()
    }
}

/// "Wind Curving": reduce the outer fan while steering, to zero at `dual%` of the wheel's maximum angle.
/// A zero threshold (no steering range reported) skips the effect.
fn apply_curving(
    percent: &mut FanPercent,
    inputs: &WindInputs,
    tuning: &WindTuning,
    telemetry: Option<&Telemetry>,
    output: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), WindError> {
    let raw_angle = telemetry
        .and_then(|t| t.native_value("steering_wheel_angle"))
        .cloned()
        .unwrap_or(LegacyValue::Float(inputs.steering_angle));
    let raw_maximum = telemetry
        .and_then(|t| t.native_value("steering_wheel_max_angle"))
        .cloned()
        .unwrap_or(LegacyValue::Float(inputs.steering_max_angle));
    let threshold =
        raw_maximum.py_mul(&LegacyValue::Float(inputs.dual_max_steering_percent * 0.01)).map_err(|error| {
            if error.starts_with("TypeError:") {
                WindError::UnsupportedSteeringScale(native_kind(&raw_maximum))
            } else {
                native_operation_error(error)
            }
        })?;
    let mut angle = raw_angle.py_abs().map_err(|error| {
        if error.starts_with("TypeError:") {
            WindError::UnsupportedSteeringAbs(native_kind(&raw_angle))
        } else {
            native_operation_error(error)
        }
    })?;
    if angle.py_gt(&threshold).map_err(native_operation_error)? {
        angle = threshold.clone();
    }
    let curve = match tuning.native_curving.as_ref().map(|native| native.factor) {
        Some(NativeSpeedFactor::Failed) => {
            output("Failed to calculate the xpo_angle/curving effect\n")?;
            1.5
        }
        Some(NativeSpeedFactor::Number(value)) => map_clamped(value, 1.0, 10.0, 1.5, 3.0),
        None => map_clamped(tuning.curving_effect, 1.0, 10.0, 1.5, 3.0),
    };
    // Only the division is skipped: the curving value lookup above has
    // already run when an unknown steering range makes this threshold zero.
    let ratio = match angle.py_truediv(&threshold) {
        Ok(value) => value.py_as_double().map_err(native_operation_error)?,
        Err(error) if error.starts_with("ZeroDivisionError:") => return Ok(()),
        Err(error) => return Err(native_operation_error(error)),
    };
    let base = 1.0 - ratio;
    if base == 0.0 && curve < 0.0 {
        // Zero raised to a negative exponent is a division by zero, which also skips the effect.
        return Ok(());
    }
    let mut factor = base.powf(curve);
    // Written as a comparison (not `min`) so a NaN factor stays NaN.
    if factor > 100.0 {
        factor = 100.0;
    }
    if !openrigcue_model::python_compare::native_equal(&raw_angle, &LegacyValue::Int(0)) {
        if raw_angle.py_gt(&LegacyValue::Int(0)).map_err(native_operation_error)? {
            percent.left.multiply_float(factor)?;
        } else {
            percent.right.multiply_float(factor)?;
        }
    }
    Ok(())
}

/// The percentage mapped from 0..100 to `min% * range / 100 .. range`, truncated: the minimum percentage is a floor of the PWM
/// range, so a fan that is "off" still outputs `min% * range`.
#[cfg(test)]
fn to_pwm(percent: f64, min_percent: f64, range: f64) -> Result<i64, WindError> {
    let floor = min_percent * range / 100.0;
    let level = map_clamped(percent, 0.0, 100.0, floor, range);
    truncate(level).ok_or(WindError::NotFinite)
}

fn to_pwm_value(
    percent: FanPercentValue,
    minimum: f64,
    native_minimum: Option<&LegacyValue>,
    range: f64,
) -> Result<i64, WindError> {
    // Both production callers pass integer ranges (1023 and255). Preserve
    // sequence multiplication's kind without allocating a repeated sequence.
    let floor = if let Some(value) = native_minimum {
        value
            .py_mul(&LegacyValue::Int(range as i64))
            .and_then(|value| value.py_truediv(&LegacyValue::Int(100)))
            .and_then(|value| value.py_as_double())
            .map_err(native_operation_error)?
    } else {
        minimum * range / 100.0
    };
    // The minimum's product/division is evaluated before the percentage is converted.
    let percent = match percent {
        FanPercentValue::Number(percent) => percent,
        FanPercentValue::Native(value) => value.py_as_double().map_err(native_operation_error)?,
    };
    let level = map_clamped(percent, 0.0, 100.0, floor, range);
    truncate(level).ok_or(WindError::NotFinite)
}

/// Fan levels in the "LLA" service bypass: both side fans follow speed in m/s
/// mapped 0..100 -> 0..range and the centre fan follows the steering angle.
pub fn lla_levels(speed_kmh: f64, steering_angle: f64, pwm_range: f64) -> Result<FanLevels, WindError> {
    let speed_ms = truncate(speed_kmh / 3.6).ok_or(WindError::NotFinite)? as f64;
    let side = truncate(map_clamped(speed_ms, 0.0, 100.0, 0.0, pwm_range)).ok_or(WindError::NotFinite)?;
    let center = map_clamped(round_to_hundredths(steering_angle), 0.0, 100.0, 0.0, pwm_range);
    Ok(FanLevels { left: side, right: side, center: truncate(center).ok_or(WindError::NotFinite)? })
}

pub(crate) fn lla_levels_for_telemetry(
    telemetry: &Telemetry,
    speed_kmh: f64,
    steering_angle: f64,
    range: f64,
) -> Result<FanLevels, WindError> {
    let speed = super::values::channel(telemetry, "speed_kmh", speed_kmh);
    let speed = speed
        .py_truediv(&LegacyValue::Float(3.6))
        .and_then(|v| v.py_integer())
        .map(LegacyValue::from_integer)
        .and_then(|v| v.py_as_double())
        .map_err(native_operation_error)?;
    let angle = super::values::channel(telemetry, "steering_wheel_angle", steering_angle).resolved();
    let angle = if angle.integer().is_some() {
        angle.py_as_double().map_err(native_operation_error)?
    } else if let LegacyValue::Float(value) = angle {
        round_to_hundredths(value)
    } else {
        return Err(native_operation_error(format!(
            "TypeError: type {} doesn't define __round__ method",
            angle.type_name()
        )));
    };
    let side = truncate(map_clamped(speed, 0.0, 100.0, 0.0, range)).ok_or(WindError::NotFinite)?;
    let center = truncate(map_clamped(angle, 0.0, 100.0, 0.0, range)).ok_or(WindError::NotFinite)?;
    Ok(FanLevels { left: side, right: side, center })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wall_at(offset: Duration) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 3, 1).unwrap().and_hms_opt(8, 0, 0).unwrap()
            + chrono::TimeDelta::from_std(offset).unwrap()
    }

    impl WindSimulator {
        /// The wall clock read as `wall_at(offset)` from now on.
        fn at(&mut self, offset: Duration) -> &mut Self {
            let time = wall_at(offset);
            self.wall = std::sync::Arc::new(move || time);
            self
        }
    }

    fn tuning() -> WindTuning {
        WindTuning {
            min_percent: 0.0,
            max_percent: 100.0,
            center_min_percent: 0.0,
            center_max_percent: 100.0,
            speed_factor: 1.0,
            curving: true,
            curving_effect: 10.0,
            ejection_seat: false,
            native_speed_factor: None,
            native_curving: None,
            native_ejection: None,
            native_arguments: None,
            native_bounds: None,
        }
    }

    fn inputs(speed: f64) -> WindInputs {
        WindInputs {
            speed_kmh: speed,
            max_speed_kmh: 150.0,
            steering_angle: 0.0,
            steering_max_angle: 450.0,
            dual_max_steering_percent: 50.0,
            left_right_mode: FanMode::Auto,
            center_mode: FanMode::Auto,
            ejection_requested: false,
        }
    }

    #[test]
    fn native_bounds_preserve_integer_fallback_and_late_conversion_order() {
        let now = Duration::ZERO;
        let mut tune = tuning();
        tune.curving = false;
        tune.native_bounds = Some(NativeWindBounds {
            minimum: 17.into(),
            maximum: "100".into(),
            center_minimum: 0.into(),
            center_maximum: 100.into(),
        });
        let mut input = inputs(75.0);
        let mut wind = WindSimulator::new();
        assert_eq!(
            wind.at(now).fan_levels(2, 1023.0, &input, &tune).unwrap_err().to_string(),
            "can't multiply sequence by non-int of type 'float'"
        );
        tune.native_speed_factor = Some(NativeSpeedFactor::Failed);
        input.left_right_mode = FanMode::Off;
        let mut output = Vec::new();
        let error = wind
            .at(now)
            .fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "must be real number, not str");
        assert_eq!(output, ["failed to calculate the xpo_speed / speed_Factor\n"]);
        // The sequence survived integer0 multiplication; its later conversion
        // must wait until the minimum's multiplication/division has completed.
        tune.native_bounds.as_mut().unwrap().minimum = "17".into();
        let error = wind.at(now).fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |_| Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "unsupported operand type(s) for /: 'str' and 'int'");
        tune.native_bounds.as_mut().unwrap().minimum = 17.into();
        tune.native_curving = Some(NativeCurving { enabled: Ok(true), factor: NativeSpeedFactor::Failed });
        input.steering_angle = 112.5;
        output.clear();
        let error = wind
            .at(now)
            .fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "can't multiply sequence by non-int of type 'float'");
        assert_eq!(
            output,
            [
                "failed to calculate the xpo_speed / speed_Factor\n",
                "Failed to calculate the xpo_angle/curving effect\n"
            ]
        );
        tune.native_bounds.as_mut().unwrap().center_maximum = LegacyValue::None;
        let error = wind.at(now).fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |_| Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "unsupported operand type(s) for *: 'float' and 'NoneType'");
    }

    #[test]
    fn native_bounds_ejection_overrides_maxima_but_not_minima() {
        let now = Duration::ZERO;
        let mut tune = tuning();
        tune.curving = false;
        tune.ejection_seat = true;
        tune.native_bounds = Some(NativeWindBounds {
            minimum: 17.into(),
            maximum: LegacyValue::None,
            center_minimum: 13.into(),
            center_maximum: LegacyValue::Dict(Default::default()),
        });
        let input = WindInputs { ejection_requested: true, ..inputs(75.0) };
        let mut wind = WindSimulator::new();
        assert_eq!(
            wind.at(now).fan_levels(2, 1023.0, &input, &tune).unwrap(),
            FanLevels { left: 1023, right: 1023, center: 1023 }
        );
        tune.native_bounds.as_mut().unwrap().minimum = LegacyValue::List(vec![]);
        let error = wind.at(now).fan_levels(2, 1023.0, &input, &tune).unwrap_err();
        assert_eq!(error.to_string(), "unsupported operand type(s) for /: 'list' and 'int'");
        assert_eq!(wind.ejection_started, Some(wall_at(now)));
        tune.native_bounds.as_mut().unwrap().minimum = 17.into();
        tune.native_bounds.as_mut().unwrap().center_minimum = LegacyValue::None;
        let error = wind.at(now).fan_levels(2, 1023.0, &input, &tune).unwrap_err();
        assert_eq!(error.to_string(), "unsupported operand type(s) for *: 'NoneType' and 'int'");
    }

    #[test]
    fn native_arguments_fail_before_timer_expiration_or_factor_output() {
        let now = Duration::ZERO;
        let failure = NativeWindError { class: "KeyError", message: "'power_wind_min_speed_perc'".into() };
        let mut tune = tuning();
        tune.ejection_seat = true;
        tune.curving = false;
        let request = WindInputs { ejection_requested: true, ..inputs(75.0) };
        let mut wind = WindSimulator::new();
        tune.native_arguments = Some(Err(failure.clone()));
        tune.native_ejection = Some(Err(NativeWindError { class: "Exception", message: "later enable error".into() }));
        tune.native_speed_factor = Some(NativeSpeedFactor::Failed);
        assert_eq!(
            wind.at(now).fan_levels_with_output(1, 1023.0, &request, &tune, None, &mut |_| {
                panic!("missing caller argument precedes all fan computations")
            }),
            Err(WindError::Native(failure.clone()))
        );
        assert_eq!(wind.ejection_started, None);
        tune.native_arguments = Some(Ok(()));
        tune.native_ejection = Some(Ok(true));
        tune.native_speed_factor = Some(NativeSpeedFactor::Number(1.0));
        wind.at(now).fan_levels(1, 1023.0, &request, &tune).unwrap();
        assert_eq!(wind.ejection_started, Some(wall_at(now)));
        tune.native_arguments = Some(Err(failure.clone()));
        let expired = WindInputs { ejection_requested: false, max_speed_kmh: 0.0, ..request };
        assert_eq!(
            wind.at(now + Duration::from_secs(41)).fan_levels(1, 1023.0, &expired, &tune),
            Err(WindError::Native(failure))
        );
        assert_eq!(wind.ejection_started, Some(wall_at(now)), "argument failure precedes clearing an expired blast");
        tune.native_arguments = Some(Ok(()));
        assert_eq!(
            wind.at(now + Duration::from_secs(41)).fan_levels(1, 1023.0, &expired, &tune),
            Err(WindError::ZeroMaxSpeed)
        );
        assert_eq!(wind.ejection_started, None);
    }

    #[test]
    fn native_ejection_failure_is_reached_only_on_request_before_timer_and_output() {
        let now = Duration::ZERO;
        let failure = NativeWindError { class: "KeyError", message: "'enable_ejection_seat'".into() };
        let mut tune = tuning();
        tune.ejection_seat = true;
        tune.curving = false;
        tune.native_ejection = Some(Err(failure.clone()));
        tune.native_speed_factor = Some(NativeSpeedFactor::Failed);
        let mut wind = WindSimulator::new();
        let no_request = inputs(75.0);
        let mut output = Vec::new();
        let normal = wind
            .at(now)
            .fan_levels_with_output(2, 1023.0, &no_request, &tune, None, &mut |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(normal.left, 511);
        assert_eq!(output, ["failed to calculate the xpo_speed / speed_Factor\n"]);
        let request = WindInputs { ejection_requested: true, max_speed_kmh: 0.0, ..no_request };
        assert_eq!(
            wind.at(now).fan_levels_with_output(2, 1023.0, &request, &tune, None, &mut |_| {
                panic!("ejection failure precedes speed division and both factor catches")
            }),
            Err(WindError::Native(failure.clone()))
        );
        assert_eq!(wind.ejection_started, None);
        tune.native_ejection = Some(Ok(true));
        wind.at(now).fan_levels_with_output(2, 1023.0, &request, &tune, None, &mut |_| Ok(())).unwrap();
        assert_eq!(wind.ejection_started, Some(wall_at(now)));
        tune.native_ejection = Some(Err(failure.clone()));
        assert_eq!(
            wind.at(now + Duration::from_secs(41)).fan_levels_with_output(
                2,
                1023.0,
                &request,
                &tune,
                None,
                &mut |_| panic!("failed request also precedes expiration")
            ),
            Err(WindError::Native(failure))
        );
        assert_eq!(wind.ejection_started, Some(wall_at(now)), "old timestamp remains until a frame reaches expiration");
        wind.at(now + Duration::from_secs(41))
            .fan_levels_with_output(2, 1023.0, &no_request, &tune, None, &mut |_| Ok(()))
            .unwrap();
        assert_eq!(wind.ejection_started, None);
    }

    #[test]
    fn native_ejection_false_preserves_existing_blast_without_rearming() {
        let now = Duration::ZERO;
        let mut tune = tuning();
        tune.ejection_seat = true;
        tune.curving = false;
        tune.native_ejection = Some(Ok(true));
        let request = WindInputs { ejection_requested: true, ..inputs(75.0) };
        let mut wind = WindSimulator::new();
        wind.at(now).fan_levels(2, 1023.0, &request, &tune).unwrap();
        tune.native_ejection = Some(Ok(false));
        let held = wind.at(now + Duration::from_secs(10)).fan_levels(2, 1023.0, &request, &tune).unwrap();
        assert_eq!(held.left, 1023);
        assert_eq!(wind.ejection_started, Some(wall_at(now)));
        let fading = wind.at(now + Duration::from_secs(30)).fan_levels(2, 1023.0, &request, &tune).unwrap();
        assert_eq!(fading.left, 511);
        assert_eq!(wind.ejection_started, Some(wall_at(now)));
        let normal = wind.at(now + Duration::from_secs(40)).fan_levels(2, 1023.0, &request, &tune).unwrap();
        assert_eq!(normal.left, 511);
        assert_eq!(wind.ejection_started, None);
    }

    #[test]
    fn native_curving_required_gate_follows_speed_and_preserves_timer() {
        let now = Duration::ZERO;
        let failure = NativeWindError { class: "KeyError", message: "'wind_curving'".into() };
        let mut tune = tuning();
        tune.ejection_seat = true;
        tune.native_speed_factor = Some(NativeSpeedFactor::Failed);
        tune.native_curving = Some(NativeCurving { enabled: Err(failure.clone()), factor: NativeSpeedFactor::Failed });
        let mut wind = WindSimulator::new();
        let mut output = Vec::new();
        let input = WindInputs { ejection_requested: true, ..inputs(75.0) };
        assert_eq!(
            wind.at(now).fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |text| {
                output.push(text.to_owned());
                Ok(())
            }),
            Err(WindError::Native(failure))
        );
        assert_eq!(output, ["failed to calculate the xpo_speed / speed_Factor\n"]);
        assert_eq!(wind.ejection_started, Some(wall_at(now)));
        let single = wind.at(now).fan_levels_with_output(1, 1023.0, &input, &tune, None, &mut |_| Ok(())).unwrap();
        assert_eq!(single, FanLevels { left: 1023, right: 1023, center: 1023 });
        let input = WindInputs { max_speed_kmh: 0.0, ..inputs(0.0) };
        assert_eq!(
            WindSimulator::new().at(now).fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |_| {
                panic!("division precedes the speed catch and curving gate")
            }),
            Err(WindError::ZeroMaxSpeed)
        );
    }

    #[test]
    fn native_curving_factor_catch_precedes_zero_division_and_preserves_prefix() {
        let now = Duration::ZERO;
        let mut tune = tuning();
        tune.min_percent = 17.0;
        tune.center_min_percent = 17.0;
        tune.ejection_seat = true;
        tune.native_curving = Some(NativeCurving { enabled: Ok(true), factor: NativeSpeedFactor::Failed });
        let mut input = WindInputs { steering_angle: 112.5, ..inputs(75.0) };
        let mut output = Vec::new();
        let mut wind = WindSimulator::new();
        let levels = wind
            .at(now)
            .fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(levels, FanLevels { left: 324, right: 598, center: 598 });
        assert_eq!(output, ["Failed to calculate the xpo_angle/curving effect\n"]);
        input.steering_max_angle = 0.0;
        output.clear();
        let levels = wind
            .at(now)
            .fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(levels, FanLevels { left: 598, right: 598, center: 598 });
        assert_eq!(output, ["Failed to calculate the xpo_angle/curving effect\n"]);
        let mut telemetry = Telemetry::off();
        telemetry.set_native("steering_wheel_angle", LegacyValue::None);
        assert_eq!(
            wind.at(now).fan_levels_with_output(2, 1023.0, &input, &tune, Some(&telemetry), &mut |_| {
                panic!("abs failure precedes the local curving factor catch")
            }),
            Err(WindError::UnsupportedSteeringAbs("NoneType"))
        );
        tune.native_curving.as_mut().unwrap().enabled = Ok(false);
        wind.at(now)
            .fan_levels_with_output(2, 1023.0, &input, &tune, Some(&telemetry), &mut |_| {
                panic!("false gate skips both the steering operands and factor")
            })
            .unwrap();
        tune.native_curving.as_mut().unwrap().enabled = Ok(true);
        input.ejection_requested = true;
        let error = wind
            .at(now)
            .fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |_| Err(std::io::ErrorKind::BrokenPipe.into()))
            .unwrap_err();
        assert!(matches!(error, WindError::DiagnosticOutput { kind: std::io::ErrorKind::BrokenPipe, .. }));
        assert_eq!(wind.ejection_started, Some(wall_at(now)), "print failure retains the earlier timer update");
        input.ejection_requested = false;
        input.steering_max_angle = 450.0;
        tune.native_curving.as_mut().unwrap().factor = NativeSpeedFactor::Number(10.0);
        let recovered = wind
            .at(now + Duration::from_secs(40))
            .fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |_| {
                panic!("numeric factor recovers without output")
            })
            .unwrap();
        assert_eq!(recovered, FanLevels { left: 226, right: 598, center: 598 });
    }

    #[test]
    fn native_factor_failure_uses_linear_exponent_and_exact_output() {
        let now = Duration::ZERO;
        let mut tune = tuning();
        tune.curving = false;
        tune.speed_factor = 3.0;
        tune.native_speed_factor = Some(NativeSpeedFactor::Failed);
        let mut output = Vec::new();
        let result = WindSimulator::new()
            .at(now)
            .fan_levels_with_output(2, 1023.0, &inputs(75.0), &tune, None, &mut |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(result, FanLevels { left: 511, right: 511, center: 511 });
        assert_eq!(output, ["failed to calculate the xpo_speed / speed_Factor\n"]);
        tune.native_speed_factor = Some(NativeSpeedFactor::Number(3.0));
        let recovered = WindSimulator::new()
            .at(now)
            .fan_levels_with_output(2, 1023.0, &inputs(75.0), &tune, None, &mut |_| {
                panic!("numeric factor does not print")
            })
            .unwrap();
        assert_eq!(recovered.left, 276);
        tune.native_speed_factor = Some(NativeSpeedFactor::Number(f64::NAN));
        let result = WindSimulator::new()
            .at(now)
            .fan_levels_with_output(2, 1023.0, &inputs(75.0), &tune, None, &mut |_| {
                panic!("NaN maps to the upper bound")
            })
            .unwrap();
        assert_eq!(result.left, 31);
    }

    #[test]
    fn native_factor_failure_follows_division_and_preserves_ejection_state() {
        let now = Duration::ZERO;
        let mut tune = tuning();
        tune.curving = false;
        tune.ejection_seat = true;
        tune.native_speed_factor = Some(NativeSpeedFactor::Failed);
        let mut wind = WindSimulator::new();
        assert_eq!(
            wind.at(now).fan_levels_with_output(
                2,
                1023.0,
                &inputs(75.0),
                &tune,
                Some(&{
                    let mut t = Telemetry::off();
                    t.set_native("speed_kmh", LegacyValue::None);
                    t
                }),
                &mut |_| panic!("division failure precedes local factor catch")
            ),
            Err(WindError::UnsupportedDivision("NoneType", "float"))
        );
        let mut output = Vec::new();
        let blast = WindInputs { ejection_requested: true, ..inputs(75.0) };
        wind.at(now)
            .fan_levels_with_output(2, 1023.0, &blast, &tune, None, &mut |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(wind.ejection_started, Some(wall_at(now)));
        assert_eq!(output.len(), 1, "active blast still evaluates the factor");
        let input = WindInputs { max_speed_kmh: 0.0, ..inputs(0.0) };
        let error = wind
            .at(now + Duration::from_secs(20))
            .fan_levels_with_output(2, 1023.0, &input, &tune, None, &mut |_| Err(std::io::ErrorKind::BrokenPipe.into()))
            .unwrap_err();
        assert!(matches!(error, WindError::DiagnosticOutput { kind: std::io::ErrorKind::BrokenPipe, .. }));
        assert_eq!(wind.ejection_started, Some(wall_at(now)), "printing does not roll back earlier timer state");
        assert_eq!(
            wind.at(now + Duration::from_secs(40)).fan_levels_with_output(
                2,
                1023.0,
                &input,
                &tune,
                None,
                &mut |_| panic!("expired blast then failed division skips factor")
            ),
            Err(WindError::ZeroMaxSpeed)
        );
        assert_eq!(wind.ejection_started, None);
    }

    #[test]
    fn native_division_distinguishes_nonnumeric_values_from_nan_and_skips_the_blast() {
        let now = Duration::ZERO;
        let mut tuning = tuning();
        tuning.curving = false;
        let mut input = WindInputs { left_right_mode: FanMode::Off, center_mode: FanMode::Off, ..inputs(f64::NAN) };
        let mut telemetry = Telemetry::off();
        telemetry.set_native("speed_kmh", LegacyValue::None);
        telemetry.set_native("max_speed_kmh", LegacyValue::Int(150));
        let mut wind = WindSimulator::new();
        assert_eq!(
            wind.at(now).fan_levels_for_telemetry(2, 1023.0, &input, &tuning, &telemetry),
            Err(WindError::UnsupportedDivision("NoneType", "int"))
        );
        telemetry.set_native("speed_kmh", LegacyValue::Float(f64::NAN));
        assert_eq!(
            wind.at(now).fan_levels_for_telemetry(2, 1023.0, &input, &tuning, &telemetry).unwrap(),
            FanLevels { left: 0, right: 0, center: 0 }
        );
        telemetry.set_native("speed_kmh", LegacyValue::None);
        telemetry.set_native("max_speed_kmh", LegacyValue::None);
        input.ejection_requested = true;
        input.max_speed_kmh = f64::NAN;
        tuning.ejection_seat = true;
        assert_eq!(
            wind.at(now).fan_levels_for_telemetry(2, 1023.0, &input, &tuning, &telemetry).unwrap(),
            FanLevels { left: 1023, right: 1023, center: 1023 }
        );
        input.ejection_requested = false;
        assert_eq!(
            wind.at(now + Duration::from_secs(41)).fan_levels_for_telemetry(2, 1023.0, &input, &tuning, &telemetry),
            Err(WindError::UnsupportedDivision("NoneType", "NoneType"))
        );
    }

    #[test]
    fn native_curving_checks_scale_and_abs_before_zero_division_but_only_when_used() {
        let now = Duration::ZERO;
        let mut input = inputs(75.0);
        input.steering_angle = f64::NAN;
        input.steering_max_angle = f64::NAN;
        let mut telemetry = Telemetry::off();
        telemetry.set_native("speed_kmh", LegacyValue::Float(75.0));
        telemetry.set_native("max_speed_kmh", LegacyValue::Float(150.0));
        telemetry.set_native("steering_wheel_angle", LegacyValue::None);
        telemetry.set_native("steering_wheel_max_angle", LegacyValue::None);
        let mut wind = WindSimulator::new();
        let mut tuning = tuning();
        assert_eq!(
            wind.at(now).fan_levels_for_telemetry(2, 1023.0, &input, &tuning, &telemetry),
            Err(WindError::UnsupportedSteeringScale("NoneType"))
        );
        telemetry.set_native("steering_wheel_max_angle", LegacyValue::Int(0));
        input.steering_max_angle = 0.0;
        assert_eq!(
            wind.at(now).fan_levels_for_telemetry(2, 1023.0, &input, &tuning, &telemetry),
            Err(WindError::UnsupportedSteeringAbs("NoneType"))
        );
        let unchanged = FanLevels { left: 511, right: 511, center: 511 };
        assert_eq!(wind.at(now).fan_levels_for_telemetry(1, 1023.0, &input, &tuning, &telemetry).unwrap(), unchanged);
        tuning.curving = false;
        assert_eq!(wind.at(now).fan_levels_for_telemetry(2, 1023.0, &input, &tuning, &telemetry).unwrap(), unchanged);
    }

    /// Linear speed curve (speed_factor 1 -> exponent 1), 10-bit PWM.
    #[test]
    fn clamped_wind_maps_limit_pwm_and_lla_outputs() {
        assert_eq!(to_pwm(200.0, 0.0, 255.0).unwrap(), 255);
        assert_eq!(to_pwm(-100.0, 20.0, 255.0).unwrap(), 51);
        assert_eq!(lla_levels(-36.0, -100.0, 255.0).unwrap(), FanLevels { left: 0, right: 0, center: 0 });
        assert_eq!(lla_levels(720.0, 200.0, 255.0).unwrap(), FanLevels { left: 255, right: 255, center: 255 });
    }

    #[test]
    fn clamped_wind_maps_limit_speed_and_curving_exponents() {
        let mut wind = WindSimulator::new();
        let mut tuning = tuning();
        tuning.curving = false;
        tuning.speed_factor = 0.0;
        assert_eq!(
            wind.at(Duration::ZERO).fan_levels(2, 255.0, &inputs(-75.0), &tuning).unwrap(),
            FanLevels { left: 0, right: 0, center: 0 }
        );
        tuning.speed_factor = 1.0;
        tuning.curving = true;
        tuning.curving_effect = -9.0;
        let mut input = inputs(150.0);
        input.steering_angle = 225.0;
        assert_eq!(
            wind.at(Duration::ZERO).fan_levels(2, 255.0, &input, &tuning).unwrap(),
            FanLevels { left: 0, right: 255, center: 255 }
        );
    }

    #[test]
    fn clamped_wind_maps_nan_pwm_selects_the_upper_bound() {
        assert_eq!(to_pwm(f64::NAN, 0.0, 255.0).unwrap(), 255);
        let mut input = inputs(150.0);
        input.steering_angle = f64::NAN;
        assert_eq!(
            WindSimulator::new().at(Duration::ZERO).fan_levels(2, 255.0, &input, &tuning()).unwrap(),
            FanLevels { left: 255, right: 255, center: 255 }
        );
    }

    #[test]
    fn linear_speed_curve() {
        let mut wind = WindSimulator::new();
        let now = Duration::ZERO;
        let out = wind.at(now).fan_levels(2, INTELLIBOX_PWM_RANGE, &inputs(75.0), &tuning()).unwrap();
        assert_eq!(out, FanLevels { left: 511, right: 511, center: 511 });
        let mut t = tuning();
        t.speed_factor = 10.0; // exponent 5: 0.5^5 * 100 = 3.125 % -> 31.96 -> 31
        let out = wind.at(now).fan_levels(2, INTELLIBOX_PWM_RANGE, &inputs(75.0), &t).unwrap();
        assert_eq!(out.left, 31);
    }

    /// min% is a PWM floor even for a switched-off channel; ratio > 1 is not clamped.
    #[test]
    fn modes_floor_and_overspeed() {
        let mut wind = WindSimulator::new();
        let mut t = tuning();
        t.min_percent = 20.0;
        let mut i = inputs(300.0);
        i.left_right_mode = FanMode::Off;
        i.center_mode = FanMode::AlwaysOn;
        let out = wind.at(Duration::ZERO).fan_levels(2, SIM3D_PWM_RANGE, &i, &t).unwrap();
        assert_eq!(out.left, 51); // 0 % maps to the floor of 51..255
        assert_eq!(out.center, 255);
        i.center_mode = FanMode::Auto;
        let out = wind.at(Duration::ZERO).fan_levels(2, SIM3D_PWM_RANGE, &i, &t).unwrap();
        assert_eq!(out.center, 255); // final PWM map clamps the ratio's output
        assert_eq!(FanMode::from_status(7), FanMode::Auto);
    }

    /// Curving: positive steering reduces the left fan, negative the right, zero at threshold.
    #[test]
    fn curving_reduces_outer_fan() {
        let mut wind = WindSimulator::new();
        let mut i = inputs(150.0);
        i.steering_angle = 112.5; // half of threshold 225 -> (0.5)^3 = 0.125
        let out = wind.at(Duration::ZERO).fan_levels(2, INTELLIBOX_PWM_RANGE, &i, &tuning()).unwrap();
        assert_eq!(out, FanLevels { left: 127, right: 1023, center: 1023 });
        i.steering_angle = -900.0;
        let out = wind.at(Duration::ZERO).fan_levels(2, INTELLIBOX_PWM_RANGE, &i, &tuning()).unwrap();
        assert_eq!(out, FanLevels { left: 1023, right: 0, center: 1023 });
        // One fan (Intellibox model 1): no curving.
        let out = wind.at(Duration::ZERO).fan_levels(1, INTELLIBOX_PWM_RANGE, &i, &tuning()).unwrap();
        assert_eq!(out.right, 1023);
        // No steering range: the division by zero is ignored and curving skipped.
        i.steering_max_angle = 0.0;
        let out = wind.at(Duration::ZERO).fan_levels(2, INTELLIBOX_PWM_RANGE, &i, &tuning()).unwrap();
        assert_eq!(out.right, 1023);
    }

    /// Curving: the cap at 100 leaves a NaN factor (NaN steering angle) as NaN,
    /// so the PWM conversion fails the frame instead of driving the fan at 100x.
    #[test]
    fn curving_nan_factor_reaches_the_clamped_pwm_map() {
        let mut wind = WindSimulator::new();
        let mut i = inputs(150.0);
        i.steering_angle = f64::NAN;
        assert_eq!(
            wind.at(Duration::ZERO).fan_levels(2, INTELLIBOX_PWM_RANGE, &i, &tuning()).unwrap(),
            FanLevels { left: 1023, right: 1023, center: 1023 }
        );
    }

    /// The curving map clamps low slider values to a positive exponent before the full-lock power.
    #[test]
    fn curving_clamps_low_exponent_before_full_lock() {
        let mut wind = WindSimulator::new();
        let mut i = inputs(150.0);
        i.steering_angle = 900.0;
        let steep = WindTuning { curving_effect: -20.0, ..tuning() };
        let out = wind.at(Duration::ZERO).fan_levels(2, INTELLIBOX_PWM_RANGE, &i, &steep).unwrap();
        assert_eq!(out.left, 0);
    }

    /// Ejection seat: full for 20 s, linear fade to 40 s, then telemetry again; no speed division.
    #[test]
    fn ejection_seat_blast() {
        let mut wind = WindSimulator::new();
        let mut t = tuning();
        t.ejection_seat = true;
        let start = Duration::ZERO;
        let mut i = inputs(0.0);
        i.max_speed_kmh = 0.0; // would be a division by zero outside the blast
        i.ejection_requested = true;
        assert_eq!(wind.at(start).fan_levels(2, 255.0, &i, &t).unwrap().left, 255);
        i.ejection_requested = false;
        assert_eq!(wind.at(start + Duration::from_secs(20)).fan_levels(2, 255.0, &i, &t).unwrap().center, 255);
        assert_eq!(wind.at(start + Duration::from_secs(30)).fan_levels(2, 255.0, &i, &t).unwrap().left, 127);
        assert_eq!(wind.at(start + Duration::from_secs(40)).fan_levels(2, 255.0, &i, &t), Err(WindError::ZeroMaxSpeed));
        // Switch disabled: the request is ignored.
        let mut w2 = WindSimulator::new();
        t.ejection_seat = false;
        i.ejection_requested = true;
        assert_eq!(w2.at(start).fan_levels(2, 255.0, &i, &t), Err(WindError::ZeroMaxSpeed));
    }

    /// The elapsed time follows the wall clock: a clock set back
    /// before the press keeps the blast (negative elapsed), microseconds count, and 40 s exactly ends it.
    #[test]
    fn ejection_seat_timer_reads_the_wall_clock_with_microsecond_total_seconds() {
        let mut wind = WindSimulator::new();
        let mut t = tuning();
        t.ejection_seat = true;
        let mut i = inputs(0.0);
        i.max_speed_kmh = 0.0;
        i.ejection_requested = true;
        let press = Duration::from_secs(3600);
        assert_eq!(wind.at(press).fan_levels(2, 255.0, &i, &t).unwrap().left, 255);
        i.ejection_requested = false;
        assert_eq!(wind.at(Duration::ZERO).fan_levels(2, 255.0, &i, &t).unwrap().left, 255, "set back an hour");
        assert_eq!(wind.ejection_started, Some(wall_at(press)));
        let almost = press + Duration::from_secs(40) - Duration::from_micros(1);
        assert!(wind.at(almost).fan_levels(2, 255.0, &i, &t).unwrap().left < 255, "fading, still no division");
        assert!(wind.ejection_started.is_some());
        assert_eq!(wind.at(press + Duration::from_secs(40)).fan_levels(2, 255.0, &i, &t), Err(WindError::ZeroMaxSpeed));
        assert_eq!(wind.ejection_started, None);
    }

    /// A negative ratio with a fractional exponent (no real power) fails the frame.
    #[test]
    fn negative_speed_with_fractional_exponent_fails() {
        let mut wind = WindSimulator::new();
        let mut t = tuning();
        t.speed_factor = 5.0;
        assert_eq!(wind.at(Duration::ZERO).fan_levels(2, 255.0, &inputs(-10.0), &t), Err(WindError::ComplexPower));
    }

    /// The LLA bypass for both devices.
    #[test]
    fn lla_bypass() {
        let out = lla_levels(180.0, 50.0, INTELLIBOX_PWM_RANGE).unwrap();
        assert_eq!(out, FanLevels { left: 511, right: 511, center: 511 });
        let out = lla_levels(36.0, 12.345, SIM3D_PWM_RANGE).unwrap();
        assert_eq!(out, FanLevels { left: 25, right: 25, center: 31 });
    }
}

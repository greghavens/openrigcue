//! Motion compensation: the rig's attitude and displacement, measured by the IMU or estimated from the
//! actuator positions, exported as the pose VR software subtracts from the head tracking.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDateTime;
use openrigcue_model::{
    motion_sensor::MotionSensorValues,
    native_value::{BigInt, LegacyValue},
};
use tracing::{debug, error};

use super::imu::{Imu, InstallationReader, NativeInstallationReader};
use super::link::Clock;
use super::numeric::map_clamped;
use super::pose_log::{PoseLog, PoseLogRow};
use super::rig_geometry::{
    center_height_mm, corner_tilt, estimate_pitch, estimate_roll, estimate_yaw, table_tilt, Corners, MotorCounts, Tilt,
};

/// `MOTION_COMPENSATION.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompensationKind {
    /// `'W'`: pitch and roll come from the WitMotion IMU.
    Sensor,
    /// `'V'`: pitch and roll are estimated from the actuator positions; no sensor is needed.
    Virtual,
    /// Any other value.
    Off,
}

/// The six compensated axes, in the order a pass computes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Pitch,
    Roll,
    Sway,
    Surge,
    Yaw,
    Heave,
}

impl Axis {
    pub const ORDER: [Axis; 6] = [Axis::Pitch, Axis::Roll, Axis::Sway, Axis::Surge, Axis::Yaw, Axis::Heave];

    fn name(self) -> &'static str {
        match self {
            Self::Pitch => "pitch",
            Self::Roll => "roll",
            Self::Yaw => "yaw",
            Self::Heave => "heave",
            Self::Sway => "sway",
            Self::Surge => "surge",
        }
    }

    /// The other tilt axis (`swap_roll_pitch`); other axes are their own.
    fn swapped(self) -> Axis {
        match self {
            Axis::Pitch => Axis::Roll,
            Axis::Roll => Axis::Pitch,
            other => other,
        }
    }
}

/// Per-axis settings (the `MOTION_COMPENSATION.<axis>` subsection).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisSettings {
    /// `enabled` as a boolean.
    pub enabled: bool,
    /// `reverse` as a boolean: the axis' value is negated (factor -1).
    pub reverse: bool,
    /// `offset` as a float, added to the sensor reading before the reversal.
    pub offset: f64,
}

impl AxisSettings {
    /// The sign factor: -1 when reversed, else 1.
    fn factor(&self) -> f64 {
        if self.reverse {
            -1.0
        } else {
            1.0
        }
    }
}

/// The horizontal surge actuator settings (`surge_horizontal_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HorizontalSurgeSettings {
    /// `surge_horizontal_stroke` as an integer: actuator travel in millimetres.
    pub stroke_mm: i64,
    /// `surge_horizontal_reverse` as a boolean.
    pub reverse: bool,
    /// `surge_horizontal_factor` as an integer, in percent.
    pub factor_percent: i64,
}

/// The `MOTION_COMPENSATION` values read every pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompensationSettings {
    pub kind: CompensationKind,
    pub pitch: AxisSettings,
    pub roll: AxisSettings,
    pub sway: AxisSettings,
    pub surge: AxisSettings,
    pub yaw: AxisSettings,
    pub heave: AxisSettings,
    /// `swap_roll_pitch` as a boolean.
    pub swap_roll_pitch: bool,
    /// The pitch, roll, yaw, surge and sway factors as integers
    /// (percent).
    pub pitch_factor: i64,
    pub roll_factor: i64,
    pub yaw_factor: i64,
    /// Failed required reads/integer conversions for pitch, roll, yaw, respectively.
    /// Only the six-actuator estimators, where such a failure gives 0, consume these flags.
    pub estimator_factor_failures: [bool; 3],
    /// The pitch, roll and yaw factors as the floats the estimators multiply by
    /// (`None` with a failure flag above, including a value beyond a double); `None` here keeps the
    /// typed factors.
    pub native_estimator_factors: Option<[f64; 3]>,
    pub surge_factor: i64,
    pub sway_factor: i64,
    /// `height` as a float: the head height above the rotation centre, which turns pitch and roll into surge
    /// and sway.
    pub height: f64,
    /// `mm_distance_left_right` as an integer: actuator spacing of four-actuator rigs (used for both directions).
    pub actuator_spacing_mm: i64,
    pub horizontal_surge: HorizontalSurgeSettings,
    /// `refresh_rate` as an integer, in Hz.
    pub refresh_rate: i64,
}

impl CompensationSettings {
    fn axis(&self, axis: Axis) -> &AxisSettings {
        match axis {
            Axis::Pitch => &self.pitch,
            Axis::Roll => &self.roll,
            Axis::Sway => &self.sway,
            Axis::Surge => &self.surge,
            Axis::Yaw => &self.yaw,
            Axis::Heave => &self.heave,
        }
    }
}

/// `MOTIONBOX.model`, as far as compensation tells models apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RigModel {
    P6,
    H6,
    H4,
    P4,
    Other,
}

impl RigModel {
    pub fn from_model(model: &str) -> Self {
        match model {
            "P6" => RigModel::P6,
            "H6" => RigModel::H6,
            "H4" => RigModel::H4,
            "P4" => RigModel::P4,
            _ => RigModel::Other,
        }
    }

    fn six_actuator(self) -> bool {
        matches!(self, RigModel::P6 | RigModel::H6)
    }

    fn four_actuator_seat(self) -> bool {
        matches!(self, RigModel::H4 | RigModel::P4)
    }
}

/// A horizontal surge actuator of the rig (the model's port mapping has a `Surge` entry).
#[derive(Debug, Clone, PartialEq)]
pub struct HorizontalSurgeActuator {
    /// The model's full scale of the motor stream.
    pub max_motor_abs: i64,
    /// The motion stream's value for the `Surge` port; `None` when the stream has no such
    /// entry. The cell is the published raw value (any width).
    pub position: Option<LegacyValue>,
}

/// What compensation reads about the motion platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Rig {
    pub model: RigModel,
    /// Whether the model's manufacturer is DOF Reality; `None` when `MOTIONBOX.model` is not a
    /// known model (the lookup is then an error).
    pub dof_reality: Option<bool>,
    pub horizontal_surge: Option<HorizontalSurgeActuator>,
}

/// The actuator positions the motion driver publishes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ActuatorPositions {
    /// `front_left` ... `rear_right` (0 by default).
    pub corners: Corners,
    /// `m0` ... `m5` on the 10-bit scale, when the motion driver publishes them.
    pub motors: Option<[i64; 6]>,
}

impl ActuatorPositions {
    fn motor_counts(&self) -> Option<MotorCounts> {
        self.motors.map(|m| m.map(|v| v as f64))
    }
}

/// What one compensation pass reads besides the settings.
#[derive(Clone, Copy)]
pub struct CompensationInputs<'a> {
    pub settings: &'a CompensationSettings,
    /// Required raw type comparison, consumed at the reached session/tilt boundary.
    pub native_kind: Option<&'a Result<CompensationKind, NativeCompensationError>>,
    /// Raw displacement reads, deferred until the active sway/surge branch.
    pub native_displacement: Option<&'a NativeDisplacementSettings>,
    /// The required refresh rate as an integer, consumed after the completed capture and export.
    pub native_refresh_rate: Option<&'a Result<BigInt, NativeCompensationError>>,
    /// Raw horizontal actuator reads, consumed in the order their branches are reached.
    pub native_horizontal_surge: Option<&'a NativeHorizontalSurgeSettings>,
    /// Required `enabled` boolean reads in Axis::ORDER, after each calculated row.
    pub native_axis_enabled: Option<&'a [Result<bool, NativeCompensationError>; 6]>,
    /// Reversal, tilt swap, sensor offsets and generic spacing, consumed where their branches are reached.
    pub native_axis_controls: Option<&'a NativeAxisControls>,
    /// The motion-compensation switch is on.
    pub switch_on: bool,
    pub rig: &'a Rig,
    pub actuators: &'a ActuatorPositions,
    /// The heave mode is 0: heave is derived from the actuators.
    pub heave_from_actuators: bool,
    /// The running game, for the log.
    pub game_running: Option<&'a str>,
    /// `MOTION_COMPENSATION.installation`, read where the pass compares it and by the sensor configuration.
    pub installation: &'a InstallationReader,
    pub native_installation: Option<&'a NativeInstallationReader>,
}

/// The raw height (float) and factor (integer) results before the typed defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeDisplacementSettings {
    pub height: Result<f64, NativeCompensationError>,
    pub sway_factor: Result<BigInt, NativeCompensationError>,
    pub surge_factor: Result<BigInt, NativeCompensationError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeHorizontalSurgeSettings {
    pub stroke_mm: Result<BigInt, NativeCompensationError>,
    pub reverse: Result<bool, NativeCompensationError>,
    pub factor_percent: Result<BigInt, NativeCompensationError>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeAxisControls {
    pub reverse: [Result<bool, NativeCompensationError>; 6],
    pub swap_roll_pitch: Result<bool, NativeCompensationError>,
    /// Pitch, then roll; virtual actuator estimates do not read these offsets.
    pub offsets: [Result<f64, NativeCompensationError>; 2],
    /// Both spacings of the generic actuator tilt come from this one setting.
    pub actuator_spacing_mm: Result<BigInt, NativeCompensationError>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct NativeCompensationError {
    pub class: &'static str,
    pub message: String,
}

/// One axis of the published motion-sensor readouts.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AxisReading {
    /// The live value: the calculated value while the axis is enabled and the switch is on, else 0.
    pub value: f64,
    /// The calculated value (`calculated_<axis>`).
    pub calculated: f64,
    /// The raw value (`raw_<axis>`; pitch and roll: the reading before offset and reversal).
    pub raw: f64,
}

/// The published motion-sensor readouts of all axes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MotionSensor {
    pub pitch: AxisReading,
    pub roll: AxisReading,
    pub sway: AxisReading,
    pub surge: AxisReading,
    pub yaw: AxisReading,
    pub heave: AxisReading,
}

impl MotionSensor {
    fn axis_mut(&mut self, axis: Axis) -> &mut AxisReading {
        match axis {
            Axis::Pitch => &mut self.pitch,
            Axis::Roll => &mut self.roll,
            Axis::Sway => &mut self.sway,
            Axis::Surge => &mut self.surge,
            Axis::Yaw => &mut self.yaw,
            Axis::Heave => &mut self.heave,
        }
    }

    fn pose(&self) -> RigPose {
        RigPose {
            sway: self.sway.value,
            surge: self.surge.value,
            heave: self.heave.value,
            yaw: self.yaw.value,
            roll: self.roll.value,
            pitch: self.pitch.value,
        }
    }

    /// The per-axis loop: with the switch on compute the axis, then the live value is
    /// `calculated_<axis>` only while the axis is enabled and the switch is on.
    fn pass(
        &mut self,
        source: TiltSource,
        inputs: &CompensationInputs<'_>,
        values: Option<&MotionSensorValues>,
    ) -> Result<(), CompensationError> {
        self.pass_with_source(source, None, inputs, values)
    }

    fn pass_with_source(
        &mut self,
        source: TiltSource,
        sensor_angles: Option<&dyn Fn() -> [f64; 3]>,
        inputs: &CompensationInputs<'_>,
        values: Option<&MotionSensorValues>,
    ) -> Result<(), CompensationError> {
        let settings = inputs.settings;
        for (index, axis) in Axis::ORDER.into_iter().enumerate() {
            if inputs.switch_on {
                self.compute(axis, source, sensor_angles, inputs, values)?;
            } else {
                // The live row is cleared before `enabled` is read, even if that read fails.
                self.axis_mut(axis).value = 0.0;
                if let Some(values) = values {
                    values.set(axis.name(), LegacyValue::Int(0));
                }
            }
            let enabled = match inputs.native_axis_enabled {
                Some(enabled) => enabled[index].clone()?,
                None => settings.axis(axis).enabled,
            };
            let live = enabled && inputs.switch_on;
            let reading = self.axis_mut(axis);
            reading.value = if live { reading.calculated } else { 0.0 };
            if let Some(values) = values {
                let value = if live {
                    values.get(&format!("calculated_{}", axis.name())).unwrap_or(LegacyValue::Int(0))
                } else {
                    LegacyValue::Int(0)
                };
                if let Some(number) = value.number() {
                    reading.value = number;
                }
                values.set(axis.name(), value);
            }
        }
        Ok(())
    }

    /// `calculated_<axis>` (and `raw_` for pitch and roll) of one axis.
    fn compute(
        &mut self,
        axis: Axis,
        source: TiltSource,
        sensor_angles: Option<&dyn Fn() -> [f64; 3]>,
        inputs: &CompensationInputs<'_>,
        values: Option<&MotionSensorValues>,
    ) -> Result<(), CompensationError> {
        let settings = inputs.settings;
        let factor = native_axis_factor(inputs, axis)?;
        let calculated = match axis {
            Axis::Pitch | Axis::Roll => return self.compute_tilt(axis, source, sensor_angles, inputs, values),
            Axis::Yaw => actuator_yaw(
                inputs.rig,
                inputs.actuators,
                estimator_factor(settings, 2),
                settings.estimator_factor_failures[2],
            ),
            Axis::Heave => actuator_heave(inputs, factor),
            Axis::Sway => self.compute_displacement(axis, self.roll.calculated, inputs, factor, values)?,
            Axis::Surge => {
                let from_pitch = self.compute_displacement(axis, self.pitch.calculated, inputs, factor, values)?;
                self.surge.calculated = from_pitch;
                if let Some(values) = values {
                    values.set("calculated_surge", LegacyValue::Float(from_pitch));
                }
                match &inputs.rig.horizontal_surge {
                    Some(actuator) => {
                        from_pitch
                            + match inputs.native_horizontal_surge {
                                Some(native) => {
                                    horizontal_surge_native(actuator, &settings.horizontal_surge, Some(native))?
                                }
                                None => horizontal_surge(actuator, &settings.horizontal_surge)?,
                            }
                    }
                    None => from_pitch,
                }
            }
        };
        self.axis_mut(axis).calculated = calculated;
        if let Some(values) = values {
            // The tests are `>`, then `<`, then else: NaN also reaches the literal integer zero.
            #[allow(clippy::double_comparisons)]
            let seat_zero = axis == Axis::Heave
                && inputs.heave_from_actuators
                && inputs.rig.model.four_actuator_seat()
                && !(inputs.actuators.corners.rear_right > CENTRE_COUNT as f64
                    || inputs.actuators.corners.rear_right < CENTRE_COUNT as f64);
            let value = if seat_zero
                || axis == Axis::Yaw
                    && (!(inputs.rig.model.six_actuator() && inputs.actuators.motor_counts().is_some())
                        || settings.estimator_factor_failures[2])
            {
                LegacyValue::Int(0)
            } else {
                LegacyValue::Float(calculated)
            };
            values.set(&format!("calculated_{}", axis.name()), value);
        }
        Ok(())
    }

    fn compute_displacement(
        &mut self,
        axis: Axis,
        angle: f64,
        inputs: &CompensationInputs<'_>,
        reversal: f64,
        values: Option<&MotionSensorValues>,
    ) -> Result<f64, CompensationError> {
        let settings = inputs.settings;
        let Some(native) = inputs.native_displacement else {
            let factor = if axis == Axis::Sway { settings.sway_factor } else { settings.surge_factor };
            return Ok(tilt_displacement(angle, inputs, reversal, factor));
        };
        let height = native.height.clone()?;
        let unscaled = angle.to_radians().tan() * (height * reversal);
        self.axis_mut(axis).calculated = unscaled;
        if let Some(values) = values {
            values.set(&format!("calculated_{}", axis.name()), LegacyValue::Float(unscaled));
        }
        let factor = if axis == Axis::Sway { &native.sway_factor } else { &native.surge_factor }.clone()?;
        let divisor = if inputs.rig.model.six_actuator() { SIX_ACTUATOR_DIVISOR } else { DEFAULT_DIVISOR };
        Ok(native_float(LegacyValue::Float(unscaled).py_mul(&LegacyValue::from_integer(factor)))? / divisor)
    }

    /// Pitch or roll. With `swap_roll_pitch` the values land on the other axis: the reading of `axis`
    /// (its IMU angle and offset, or its actuator estimate) is stored as `calculated_`/`raw_` of the swapped
    /// axis, reversed by that axis' setting. The value is `(reading + offset) * factor`, the offset being
    /// 0 for estimates.
    fn compute_tilt(
        &mut self,
        axis: Axis,
        source: TiltSource,
        sensor_angles: Option<&dyn Fn() -> [f64; 3]>,
        inputs: &CompensationInputs<'_>,
        values: Option<&MotionSensorValues>,
    ) -> Result<(), CompensationError> {
        let settings = inputs.settings;
        let swap = match inputs.native_axis_controls {
            Some(native) => native.swap_roll_pitch.clone()?,
            None => settings.swap_roll_pitch,
        };
        let target = if swap { axis.swapped() } else { axis };
        let factor = native_axis_factor(inputs, target)?;
        let source = if inputs.native_kind.is_some() {
            if native_compensation_kind(inputs)? == CompensationKind::Sensor {
                sensor_angles.map_or(source, |read| TiltSource::Sensor(read()))
            } else {
                TiltSource::Actuators
            }
        } else {
            source
        };
        let (reading, offset) = match source {
            TiltSource::Sensor(xyz) => {
                let offset = match inputs.native_axis_controls {
                    Some(native) => native.offsets[usize::from(axis == Axis::Roll)].clone()?,
                    None => settings.axis(axis).offset,
                };
                (sensor_angle(axis, xyz), offset)
            }
            TiltSource::Actuators => (actuator_tilt(axis, inputs)?, 0.0),
        };
        let stored = self.axis_mut(target);
        stored.calculated = (reading + offset) * factor;
        if let Some(values) = values {
            values.set(&format!("calculated_{}", target.name()), LegacyValue::Float(stored.calculated));
        }
        stored.raw = reading;
        if let Some(values) = values {
            let value = if matches!(source, TiltSource::Actuators)
                && inputs.rig.dof_reality == Some(true)
                && inputs.rig.model.six_actuator()
                && (inputs.actuators.motor_counts().is_none()
                    || settings.estimator_factor_failures[usize::from(axis == Axis::Roll)])
            {
                LegacyValue::Int(0)
            } else {
                LegacyValue::Float(reading)
            };
            values.set(&format!("raw_{}", target.name()), value);
        }
        Ok(())
    }
}

fn native_axis_factor(inputs: &CompensationInputs<'_>, axis: Axis) -> Result<f64, CompensationError> {
    let Some(native) = inputs.native_axis_controls else {
        return Ok(inputs.settings.axis(axis).factor());
    };
    let index = Axis::ORDER.iter().position(|candidate| *candidate == axis).expect("axis is in ORDER");
    Ok(if native.reverse[index].clone()? { -1.0 } else { 1.0 })
}

fn native_compensation_kind(inputs: &CompensationInputs<'_>) -> Result<CompensationKind, CompensationError> {
    match inputs.native_kind {
        Some(kind) => kind.clone().map_err(Into::into),
        None => Ok(inputs.settings.kind),
    }
}

/// The pose exported to VR software (`SimRacingStudioMotionRigPoseFile`: six packed doubles).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RigPose {
    pub sway: f64,
    pub surge: f64,
    pub heave: f64,
    pub yaw: f64,
    pub roll: f64,
    pub pitch: f64,
}

impl RigPose {
    /// The fields in structure order: sway, surge, heave, yaw, roll, pitch.
    pub fn fields(&self) -> [f64; 6] {
        [self.sway, self.surge, self.heave, self.yaw, self.roll, self.pitch]
    }

    /// The 48-byte little-endian layout of the structure.
    pub fn to_bytes(&self) -> [u8; 48] {
        let mut out = [0u8; 48];
        for (i, v) in self.fields().iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// Why a compensation pass failed; each of these ends the compensation thread.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompensationError {
    #[error(transparent)]
    NativeScalar(#[from] NativeCompensationError),
    #[error("refresh rate {0} Hz gives no valid sleep (1 / int(refresh_rate))")]
    BadRefreshRate(i64),
    /// The four-corner tilt divides by `mm_distance_left_right / 1000`, which is zero.
    #[error("mm_distance_left_right is 0")]
    ZeroActuatorSpacing,
    /// `'V'` mode needs the model's manufacturer, and the model is unknown.
    #[error("the motion model is not in motion_models")]
    UnknownModel,
}

/// Pause of a pass that has nothing to do.
const COMPENSATION_IDLE: Duration = Duration::from_secs(1);
/// The motor position that means "centred" (the default for a missing motor and the log's idle test).
const CENTRE_COUNT: i64 = 511;
/// Surge and sway scaling divisor: 20 on six-actuator rigs, 100 otherwise.
const SIX_ACTUATOR_DIVISOR: f64 = 20.0;
const DEFAULT_DIVISOR: f64 = 100.0;

/// The pause between passes, `1 / refresh_rate` seconds; a zero rate is a division error and a negative
/// rate a value error.
pub fn refresh_pause(rate_hz: i64) -> Result<Duration, CompensationError> {
    if rate_hz > 0 {
        Ok(Duration::from_secs_f64(1.0 / rate_hz as f64))
    } else {
        Err(CompensationError::BadRefreshRate(rate_hz))
    }
}

/// The result of a raw-value operation converted to a double (rescale arguments and float results).
fn native_float(value: Result<LegacyValue, String>) -> Result<f64, NativeCompensationError> {
    value.and_then(|value| value.py_as_double()).map_err(|error| {
        let error = super::values::operation(error);
        NativeCompensationError { class: error.class, message: error.message }
    })
}

/// The pause for an arbitrary-width refresh rate: `1.0 / rate` seconds as a nanosecond duration,
/// rounded up to 100 ns.
fn native_refresh_pause(rate_hz: &Result<BigInt, NativeCompensationError>) -> Result<Duration, CompensationError> {
    let rate = rate_hz.as_ref().map_err(Clone::clone)?;
    let zero = BigInt::from(0);
    // `1.0 / rate`: the integer is converted to a float first (an overflow error beyond a double).
    let seconds = native_float(LegacyValue::Float(1.0).py_truediv(&LegacyValue::from_integer(rate.clone())));
    let error = if *rate == zero { Some(("ZeroDivisionError", "float division by zero")) } else { None };
    if let Some((class, message)) = error {
        return Err(NativeCompensationError { class, message: message.into() }.into());
    }
    let seconds = seconds?;
    if seconds < 0.0 {
        return Err(NativeCompensationError {
            class: "ValueError",
            message: "sleep length must be non-negative".into(),
        }
        .into());
    }
    let nanos = (seconds * 1_000_000_000.0).ceil() as u64;
    let nanos = if cfg!(target_os = "windows") { nanos.div_ceil(100) * 100 } else { nanos };
    Ok(Duration::from_nanos(nanos))
}

/// Where pitch and roll come from in a pass.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TiltSource {
    /// Type `'W'`: the IMU's X/Y/Z angles.
    Sensor([f64; 3]),
    /// Any other type: the actuator positions.
    Actuators,
}

/// The IMU angle a tilt axis reads: pitch X (index 0), roll Y (index 1).
fn sensor_angle(axis: Axis, xyz: [f64; 3]) -> f64 {
    if axis == Axis::Pitch {
        xyz[0]
    } else {
        xyz[1]
    }
}

/// The pitch, roll or yaw factor as the float the estimator multiplies by.
fn estimator_factor(settings: &CompensationSettings, index: usize) -> f64 {
    match settings.native_estimator_factors {
        Some(factors) => factors[index],
        None => [settings.pitch_factor, settings.roll_factor, settings.yaw_factor][index] as f64,
    }
}

/// The calculated yaw, taken from the actuators in both modes: on P6 and H6 rigs
/// the six-actuator yaw estimate, negated, times `yaw_factor / 100` (0 when the positions are missing),
/// on other rigs 0.
fn actuator_yaw(rig: &Rig, actuators: &ActuatorPositions, yaw_factor: f64, factor_failed: bool) -> f64 {
    match actuators.motor_counts() {
        Some(m) if rig.model.six_actuator() && !factor_failed => -estimate_yaw(m) * yaw_factor / 100.0,
        _ => 0.0,
    }
}

/// Pitch or roll estimated from the actuators (any type other than `'W'`): on DOF Reality P6/H6 rigs the
/// six-actuator fit times `pitch_factor / 100` or `roll_factor / 100` (0 without positions), on
/// other DOF Reality rigs the seat-mover tilt (H4/P4 tables for those models), on every other rig
/// the four-corner tilt with `mm_distance_left_right` for both spacings.
fn actuator_tilt(axis: Axis, inputs: &CompensationInputs<'_>) -> Result<f64, CompensationError> {
    let tilt = if axis == Axis::Pitch { Tilt::Pitch } else { Tilt::Roll };
    let (settings, rig, actuators) = (inputs.settings, inputs.rig, inputs.actuators);
    let corners = &actuators.corners;
    if !rig.dof_reality.ok_or(CompensationError::UnknownModel)? {
        let spacing = match inputs.native_axis_controls {
            Some(native) => native_float(Ok(LegacyValue::from_integer(native.actuator_spacing_mm.clone()?)))?,
            None => settings.actuator_spacing_mm as f64,
        };
        return corner_tilt(tilt, corners, spacing, spacing).map_err(|_| {
            if inputs.native_axis_controls.is_some() {
                NativeCompensationError { class: "ZeroDivisionError", message: "float division by zero".into() }.into()
            } else {
                CompensationError::ZeroActuatorSpacing
            }
        });
    }
    if !rig.model.six_actuator() {
        return Ok(table_tilt(
            tilt,
            corners.front_left,
            corners.front_right,
            corners.rear_right,
            rig.model.four_actuator_seat(),
        ));
    }
    if settings.estimator_factor_failures[usize::from(axis == Axis::Roll)] {
        return Ok(0.0);
    }
    Ok(actuators.motor_counts().map_or(0.0, |m| match tilt {
        Tilt::Pitch => estimate_pitch(m) * estimator_factor(settings, 0) / 100.0,
        Tilt::Roll => estimate_roll(m) * estimator_factor(settings, 1) / 100.0,
    }))
}

/// Heave of an H4/P4 seat mover from its rear (heave) motor: above centre
/// 512..1023 maps to 0..-73, below centre 0..510 maps to 55.5..0, and 511 is 0.
fn seat_heave(rear_right: f64) -> f64 {
    let centre = CENTRE_COUNT as f64;
    if rear_right > centre {
        -((rear_right - 512.0) * 73.0 / 511.0)
    } else if rear_right < centre {
        55.5 - rear_right * 55.5 / 510.0
    } else {
        0.0
    }
}

/// The calculated heave: in heave mode 0, P6/H6 rigs use the centre height of the actuator plane
/// (times the sign factor, 0 when it fails) and H4/P4 seats their rear motor;
/// otherwise 0.
fn actuator_heave(inputs: &CompensationInputs<'_>, factor: f64) -> f64 {
    let (rig, actuators) = (inputs.rig, inputs.actuators);
    if !inputs.heave_from_actuators {
        return 0.0;
    }
    if rig.model.six_actuator() {
        return actuators.motors.and_then(|m| center_height_mm(m).ok()).map_or(0.0, |h| h * factor);
    }
    if rig.model.four_actuator_seat() {
        return seat_heave(actuators.corners.rear_right);
    }
    0.0
}

/// Head displacement caused by a tilt: `tan(angle) * (height * factor)`, then
/// `* axis_factor / divisor` (20 on P6/H6 rigs, 100 otherwise).
fn tilt_displacement(angle_deg: f64, inputs: &CompensationInputs<'_>, factor: f64, axis_factor: i64) -> f64 {
    let divisor = if inputs.rig.model.six_actuator() { SIX_ACTUATOR_DIVISOR } else { DEFAULT_DIVISOR };
    let displacement = angle_deg.to_radians().tan() * (inputs.settings.height * factor);
    displacement * axis_factor as f64 / divisor
}

/// The surge of a horizontal surge actuator: its stream position mapped from `0..max_motor_abs` to
/// `-stroke/2..stroke/2`, reversed when configured, times `surge_horizontal_factor / 100`; 0 when the
/// stream has no position for it.
fn horizontal_surge(
    actuator: &HorizontalSurgeActuator,
    settings: &HorizontalSurgeSettings,
) -> Result<f64, CompensationError> {
    horizontal_surge_native(actuator, settings, None)
}

fn horizontal_surge_native(
    actuator: &HorizontalSurgeActuator,
    settings: &HorizontalSurgeSettings,
    native: Option<&NativeHorizontalSurgeSettings>,
) -> Result<f64, CompensationError> {
    let stroke = match native {
        Some(native) => native.stroke_mm.clone()?,
        None => settings.stroke_mm.into(),
    };
    let Some(position) = &actuator.position else {
        return Ok(0.0);
    };
    let reverse = match native {
        Some(native) => native.reverse.clone()?,
        None => settings.reverse,
    };
    // Integer by integer: one correctly rounded float division of the arbitrary-width stroke.
    let half = native_float(LegacyValue::from_integer(stroke).py_truediv(&LegacyValue::Int(2)))?;
    let reverse = if reverse { -1.0 } else { 1.0 };
    // The rescale converts the published stream cell to a double.
    let position = native_float(Ok(position.clone()))?;
    let mm = map_clamped(position, 0.0, actuator.max_motor_abs as f64, -half, half) * reverse;
    let factor = match native {
        Some(native) => native.factor_percent.clone()?,
        None => settings.factor_percent.into(),
    };
    Ok(native_float(LegacyValue::Float(mm).py_mul(&LegacyValue::from_integer(factor)))? / 100.0)
}

/// A running sensor session.
#[derive(Debug, Default)]
struct Session {
    /// The installation mode of the previous pass (None at the start of each session).
    previous_installation: Option<String>,
    previous_native_installation: Option<LegacyValue>,
}

/// The motion-compensation producer: turns IMU angles or actuator positions into
/// the published motion-sensor readouts and the pose exported to VR software,
/// one pass per [`cycle`] call.
///
/// [`cycle`]: Compensation::cycle
#[derive(Debug, Default)]
pub struct Compensation {
    session: Option<Session>,
    sensor: MotionSensor,
    sensor_values: MotionSensorValues,
    exported: RigPose,
    log: Option<PoseLog>,
}

impl Compensation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep the per-pass CSV log in `log` (see [`start_log`]).
    pub fn attach_log(&mut self, log: PoseLog) {
        self.log = Some(log);
    }

    /// The published motion-sensor readouts.
    pub fn sensor(&self) -> &MotionSensor {
        &self.sensor
    }

    pub fn sensor_values(&self) -> MotionSensorValues {
        self.sensor_values.clone()
    }

    /// The pose last written to the export (the `SimRacingStudioMotionRigPose` section).
    pub fn pose(&self) -> RigPose {
        self.exported
    }

    /// One pass; returns the pause before the next one. `imu` is the connected sensor,
    /// `None` otherwise.
    ///
    /// A session runs while the type is `'W'` with a connected sensor, or the type is `'V'`; outside one,
    /// the readouts are reset and the pause is 1 s, plus 1 s in `'W'` mode.
    ///
    /// In a session, a pass first does what follows the previous pass' pause: without a connected sensor
    /// the session ends (1 s pause in `'W'` mode, otherwise straight back to the start, which
    /// starts a new session in `'V'` mode); a changed installation re-sends the sensor configuration on a
    /// thread. It then computes every axis, exports the pose, writes the log row and pauses
    /// `1 / refresh_rate` seconds. The pose is always exported and logged.
    pub fn cycle(
        &mut self,
        imu: Option<&Imu>,
        inputs: &CompensationInputs<'_>,
        clock: &Arc<dyn Clock>,
    ) -> Result<Duration, CompensationError> {
        self.cycle_with_sensor(
            imu.is_some(),
            || imu.expect("connected sensor").angles(),
            |session| {
                let imu = imu.expect("connected sensor");
                if let Some(read) = inputs.native_installation {
                    Self::follow_native_installation(session, read, || {
                        imu.send_native_installation(Arc::clone(read), Arc::clone(clock)).map(|_| ()).map_err(|error| {
                            NativeCompensationError { class: "OSError", message: error.to_string() }.into()
                        })
                    })
                } else {
                    Self::follow_installation(session, imu, inputs.installation, clock);
                    Ok(())
                }
            },
            inputs,
        )
    }

    /// Private seam that keeps the connection/read/configuration order testable without an IMU worker.
    fn cycle_with_sensor(
        &mut self,
        connected: bool,
        angles: impl Fn() -> [f64; 3],
        follow_installation: impl FnOnce(&mut Session) -> Result<(), CompensationError>,
        inputs: &CompensationInputs<'_>,
    ) -> Result<Duration, CompensationError> {
        if let Some(session) = self.session.as_mut() {
            if connected {
                follow_installation(session)?;
            } else {
                self.session = None;
                if native_compensation_kind(inputs)? == CompensationKind::Sensor {
                    return Ok(COMPENSATION_IDLE);
                }
            }
        }
        if self.session.is_none() {
            let kind = native_compensation_kind(inputs)?;
            let starts = (connected && kind == CompensationKind::Sensor) || kind == CompensationKind::Virtual;
            if !starts {
                self.sensor = MotionSensor::default();
                self.sensor_values.reset();
                return Ok(if kind == CompensationKind::Sensor { COMPENSATION_IDLE * 2 } else { COMPENSATION_IDLE });
            }
            self.session = Some(Session::default());
        }
        if inputs.native_kind.is_some() {
            let sensor_angles = connected.then_some(&angles as &dyn Fn() -> [f64; 3]);
            self.sensor.pass_with_source(TiltSource::Actuators, sensor_angles, inputs, Some(&self.sensor_values))?;
        } else {
            let source = if connected && inputs.settings.kind == CompensationKind::Sensor {
                TiltSource::Sensor(angles())
            } else {
                TiltSource::Actuators
            };
            self.sensor.pass(source, inputs, Some(&self.sensor_values))?;
        }
        self.export();
        if inputs.rig.model.six_actuator() {
            self.log_pass(inputs);
        }
        match inputs.native_refresh_rate {
            Some(rate) => native_refresh_pause(rate),
            None => refresh_pause(inputs.settings.refresh_rate),
        }
    }

    /// When the installation differs from the one remembered in this session, re-send the sensor
    /// configuration on a thread; then remember the installation.
    fn follow_installation(session: &mut Session, imu: &Imu, read: &InstallationReader, clock: &Arc<dyn Clock>) {
        let installation = read();
        if session.previous_installation.as_ref().is_some_and(|previous| *previous != installation) {
            if let Err(e) = imu.send_installation(Arc::clone(read), Arc::clone(clock)) {
                debug!("cannot start jy901_set_config: {e}");
            }
        }
        session.previous_installation = Some(installation);
    }

    fn follow_native_installation(
        session: &mut Session,
        read: &NativeInstallationReader,
        start: impl FnOnce() -> Result<(), CompensationError>,
    ) -> Result<(), CompensationError> {
        if let Some(previous) = &session.previous_native_installation {
            let current = read()?;
            if !openrigcue_model::python_compare::native_equal(previous, &current) {
                start()?;
            }
        }
        let current = read()?;
        session.previous_native_installation = (!matches!(current, LegacyValue::None)).then_some(current);
        Ok(())
    }

    /// The export step: the live values of the six fields are summed; a non-zero total replaces every
    /// field of the export, a zero total leaves it as it was.
    fn export(&mut self) {
        let pose = self.sensor.pose();
        let total = pose.fields().iter().fold(0.0, |sum, v| sum + v);
        if total != 0.0 {
            self.exported = pose;
        }
    }

    /// The CSV row of a pass on a P6/H6 rig, skipped while `m1..m4` are all centred (missing
    /// positions read as 511); a failure is logged ("CSV append error: ").
    fn log_pass(&self, inputs: &CompensationInputs<'_>) {
        let motors = inputs.actuators.motors.unwrap_or([CENTRE_COUNT; 6]);
        if motors[1..5].iter().all(|m| *m == CENTRE_COUNT) {
            return;
        }
        let Some(log) = &self.log else {
            error!("CSV append error: no export file (the rig was not a P6 or H6 when compensation started)");
            return;
        };
        let counts = motors.map(|m| m as f64);
        let row = PoseLogRow {
            game_running: inputs.game_running.map(str::to_string),
            pose: self.exported,
            motors,
            estimated_roll: estimate_roll(counts),
            estimated_pitch: estimate_pitch(counts),
        };
        if let Err(e) = log.append(&row) {
            error!("CSV append error: {e}");
        }
    }
}

/// Pitch and roll rates in degrees per second, in the axes of the published pose.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TiltRates {
    pub pitch: f64,
    pub roll: f64,
}

/// One pose of the VR stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackedPose {
    /// The live axis values of this pass (not the export's zero-total hold).
    pub pose: RigPose,
    /// The gyro rates routed to the pose's tilt axes, when the IMU sent an angular velocity packet.
    pub tilt_rates: Option<TiltRates>,
}

/// The pitch and roll rates of a gyro reading, routed the way `compute_tilt` routes the angles: the iterated
/// axis reads its IMU index (pitch X, roll Y), the value lands on the swapped axis with that axis'
/// reversal, and an axis that is not live (disabled, or the switch off) reads 0 like its value.
///
/// The IMU's body rates stand in for the rates of
/// its Euler angles, which they equal at small tilts.
pub fn tilt_rates(settings: &CompensationSettings, switch_on: bool, gyro: [f64; 3]) -> TiltRates {
    let mut rates = TiltRates::default();
    for axis in [Axis::Pitch, Axis::Roll] {
        let target = if settings.swap_roll_pitch { axis.swapped() } else { axis };
        let live = settings.axis(target).enabled && switch_on;
        let rate = if live { sensor_angle(axis, gyro) * settings.axis(target).factor() } else { 0.0 };
        match target {
            Axis::Pitch => rates.pitch = rate,
            _ => rates.roll = rate,
        }
    }
    rates
}

/// The rig pose for the VR consumers (the OpenRigCue rig-pose stream), computed with the same per-axis pass
/// as [`Compensation`] but once per IMU reading (200 Hz once the sensor is configured) instead of once per
/// `1 / refresh_rate`, so prediction starts from the newest sample.
///
/// It keeps its own axis state: the [`Compensation`] pass still feeds the page readouts,
/// the pose export and the CSV log. It does not apply the export's zero-total hold: an all-zero
/// pose is a valid pose for a consumer, which also gets the switch state separately.
#[derive(Debug, Default)]
pub struct PoseTracker {
    sensor: MotionSensor,
}

impl PoseTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// One pass. `angles` is the IMU reading when one is connected, `gyro` its angular velocity.
    ///
    /// `None` outside a session (`'W'` without a sensor, any type other than
    /// `'W'`/`'V'`); the axis state is reset then. Errors are those
    /// of the pass.
    pub fn update(
        &mut self,
        angles: Option<[f64; 3]>,
        gyro: Option<[f64; 3]>,
        inputs: &CompensationInputs<'_>,
    ) -> Result<Option<TrackedPose>, CompensationError> {
        let source = match (native_compensation_kind(inputs)?, angles) {
            (CompensationKind::Sensor, Some(xyz)) => TiltSource::Sensor(xyz),
            (CompensationKind::Virtual, _) => TiltSource::Actuators,
            _ => {
                self.sensor = MotionSensor::default();
                return Ok(None);
            }
        };
        self.sensor.pass(source, inputs, None)?;
        let tilt_rates = match (source, gyro) {
            (TiltSource::Sensor(_), Some(gyro)) => Some(tilt_rates(inputs.settings, inputs.switch_on, gyro)),
            _ => None,
        };
        Ok(Some(TrackedPose { pose: self.sensor.pose(), tilt_rates }))
    }
}

/// The CSV log opened when compensation starts on a P6 or H6 rig (`model` is `MOTIONBOX.model` at
/// that moment); `None` on other rigs.
pub fn start_log(model: RigModel, documents: Option<&Path>, started: NaiveDateTime) -> Option<PoseLog> {
    model.six_actuator().then(|| PoseLog::create(documents, started))
}

#[cfg(test)]
mod tests {
    use super::super::imu::command;
    use super::super::imu::test_support::connected_imu;
    use super::*;
    use std::sync::Mutex;

    fn native_displacement() -> NativeDisplacementSettings {
        NativeDisplacementSettings { height: Ok(321.0), sway_factor: Ok(50.into()), surge_factor: Ok(70.into()) }
    }

    fn scalar_error(key: &str) -> NativeCompensationError {
        NativeCompensationError { class: "KeyError", message: format!("'{key}'") }
    }

    fn axis_controls() -> NativeAxisControls {
        NativeAxisControls {
            reverse: std::array::from_fn(|_| Ok(false)),
            swap_roll_pitch: Ok(false),
            offsets: [Ok(1.0), Ok(2.0)],
            actuator_spacing_mm: Ok(558.into()),
        }
    }

    #[test]
    fn native_installation_read_start_and_reread_order_retains_previous_on_failure() {
        use std::collections::VecDeque;
        let reads = Arc::new(Mutex::new(VecDeque::from([Ok(LegacyValue::from("V")), Ok(LegacyValue::from("H"))])));
        let events = Arc::new(Mutex::new(Vec::new()));
        let values = reads.clone();
        let trace = events.clone();
        let read: NativeInstallationReader = Arc::new(move || {
            trace.lock().unwrap().push("read");
            values.lock().unwrap().pop_front().expect("expected native read")
        });
        let mut session = Session { previous_native_installation: Some(LegacyValue::from("H")), ..Default::default() };
        Compensation::follow_native_installation(&mut session, &read, || {
            events.lock().unwrap().push("start");
            Ok(())
        })
        .unwrap();
        assert_eq!(*events.lock().unwrap(), ["read", "start", "read"]);
        assert_eq!(
            session.previous_native_installation,
            Some(LegacyValue::from("H")),
            "the fresh second read is stored"
        );
        reads.lock().unwrap().push_back(Ok(LegacyValue::from("V")));
        let failure: CompensationError = scalar_error("start").into();
        assert_eq!(
            Compensation::follow_native_installation(&mut session, &read, || Err(failure.clone())),
            Err(failure)
        );
        assert_eq!(session.previous_native_installation, Some(LegacyValue::from("H")));
        reads.lock().unwrap().extend([Ok(LegacyValue::from("V")), Err(scalar_error("installation"))]);
        assert_eq!(
            Compensation::follow_native_installation(&mut session, &read, || Ok(())),
            Err(scalar_error("installation").into())
        );
        assert_eq!(session.previous_native_installation, Some(LegacyValue::from("H")));
    }

    #[test]
    fn native_installation_none_skips_comparison_and_missing_stops_before_axes() {
        let none: NativeInstallationReader = Arc::new(|| Ok(LegacyValue::None));
        let mut session = Session::default();
        Compensation::follow_native_installation(&mut session, &none, || panic!("no previous value")).unwrap();
        assert_eq!(session.previous_native_installation, None);
        let h: NativeInstallationReader = Arc::new(|| Ok(LegacyValue::from("H")));
        Compensation::follow_native_installation(&mut session, &h, || panic!("actual None is the previous sentinel"))
            .unwrap();
        assert_eq!(session.previous_native_installation, Some(LegacyValue::from("H")));

        let settings = settings(CompensationKind::Sensor);
        let input = inputs(&settings, &OTHER_RIG, &NO_POSITIONS);
        let mut capture = Compensation::new();
        capture.cycle_with_sensor(true, || [12.0, 24.0, 0.0], |_| Ok(()), &input).unwrap();
        let before = capture.sensor_values().snapshot();
        let pose = capture.pose();
        let missing: NativeInstallationReader = Arc::new(|| Err(scalar_error("installation")));
        assert_eq!(
            capture.cycle_with_sensor(
                true,
                || panic!("installation failure precedes angles"),
                |session| Compensation::follow_native_installation(session, &missing, || panic!("missing read")),
                &input
            ),
            Err(scalar_error("installation").into())
        );
        assert_eq!(capture.sensor_values().snapshot(), before);
        assert_eq!(capture.pose(), pose);
    }

    #[test]
    fn native_kind_controls_cold_entry_and_keeps_last_export_on_idle_or_failure() {
        let settings = settings(CompensationKind::Virtual);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let input = inputs(&settings, &OTHER_RIG, &positions);
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let mut capture = Compensation::new();
        capture
            .cycle(None, &CompensationInputs { native_kind: Some(&Ok(CompensationKind::Virtual)), ..input }, &clock)
            .unwrap();
        let old = capture.pose();
        let before = capture.sensor_values().snapshot();
        let missing = Err(scalar_error("type"));
        assert_eq!(
            capture.cycle(None, &CompensationInputs { native_kind: Some(&missing), ..input }, &clock),
            Err(scalar_error("type").into())
        );
        assert_eq!(capture.sensor_values().snapshot(), before);
        for (kind, duration) in [(CompensationKind::Off, 1), (CompensationKind::Sensor, 2)] {
            assert_eq!(
                capture.cycle(None, &CompensationInputs { native_kind: Some(&Ok(kind)), ..input }, &clock),
                Ok(Duration::from_secs(duration))
            );
            assert_eq!(capture.sensor_values().snapshot(), MotionSensorValues::default().snapshot());
            assert_eq!(capture.pose(), old);
        }
    }

    #[test]
    fn native_kind_connected_session_retains_active_order_and_skips_unused_reads() {
        use std::cell::RefCell;
        let events = RefCell::new(Vec::new());
        let settings = settings(CompensationKind::Sensor);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let mut controls = axis_controls();
        let input = inputs(&settings, &OTHER_RIG, &positions);
        let mut capture = Compensation::new();
        capture
            .cycle_with_sensor(
                true,
                || [12.0, 24.0, 0.0],
                |_| panic!("cold entry has no prior installation"),
                &CompensationInputs {
                    native_kind: Some(&Ok(CompensationKind::Sensor)),
                    native_axis_controls: Some(&controls),
                    ..input
                },
            )
            .unwrap();
        assert_eq!(capture.sensor().pitch.raw, 12.0);
        capture
            .cycle_with_sensor(
                true,
                || panic!("non-W type must not read IMU angles"),
                |_| {
                    events.borrow_mut().push("installation");
                    Ok(())
                },
                &CompensationInputs {
                    native_kind: Some(&Ok(CompensationKind::Off)),
                    native_axis_controls: Some(&controls),
                    ..input
                },
            )
            .unwrap();
        assert_ne!(capture.sensor().pitch.raw, 12.0, "an existing connected session uses actuators for non-W type");
        let before = capture.sensor_values().snapshot();
        let missing = Err(scalar_error("type"));
        controls.reverse[0] = Err(scalar_error("reverse"));
        assert_eq!(
            capture.cycle_with_sensor(
                true,
                || panic!("failed type must not read angles"),
                |_| {
                    events.borrow_mut().push("installation");
                    Ok(())
                },
                &CompensationInputs { native_kind: Some(&missing), native_axis_controls: Some(&controls), ..input }
            ),
            Err(scalar_error("reverse").into())
        );
        assert_eq!(capture.sensor_values().snapshot(), before);
        controls.reverse[0] = Ok(false);
        assert_eq!(
            capture.cycle_with_sensor(
                true,
                || panic!("failed type must not read angles"),
                |_| {
                    events.borrow_mut().push("installation");
                    Ok(())
                },
                &CompensationInputs { native_kind: Some(&missing), native_axis_controls: Some(&controls), ..input }
            ),
            Err(scalar_error("type").into())
        );
        assert_eq!(capture.sensor_values().snapshot(), before);
        capture
            .cycle_with_sensor(
                true,
                || panic!("inactive pass skips angles"),
                |_| {
                    events.borrow_mut().push("installation");
                    Ok(())
                },
                &CompensationInputs {
                    native_kind: Some(&missing),
                    native_axis_controls: Some(&controls),
                    switch_on: false,
                    ..input
                },
            )
            .unwrap();
        assert_eq!(capture.sensor().pitch.value, 0.0);
        assert_eq!(*events.borrow(), vec!["installation"; 4]);
        assert_eq!(
            capture.cycle_with_sensor(
                false,
                || panic!("disconnected"),
                |_| panic!("disconnected"),
                &CompensationInputs { native_kind: Some(&missing), ..input }
            ),
            Err(scalar_error("type").into())
        );
    }

    #[test]
    fn native_kind_sensor_angles_follow_controls_and_precede_offset_failure() {
        use std::cell::Cell;
        let reads = Cell::new(0);
        let settings = settings(CompensationKind::Sensor);
        let mut controls = axis_controls();
        let kind = Ok(CompensationKind::Sensor);
        let mut capture = Compensation::new();
        controls.reverse[0] = Err(scalar_error("reverse"));
        let run = |capture: &mut Compensation, controls: &NativeAxisControls| {
            capture.cycle_with_sensor(
                true,
                || {
                    reads.set(reads.get() + 1);
                    [12.0, 24.0, 0.0]
                },
                |_| Ok(()),
                &CompensationInputs {
                    native_kind: Some(&kind),
                    native_axis_controls: Some(controls),
                    ..inputs(&settings, &OTHER_RIG, &NO_POSITIONS)
                },
            )
        };
        assert_eq!(run(&mut capture, &controls), Err(scalar_error("reverse").into()));
        assert_eq!(reads.get(), 0);
        controls.reverse[0] = Ok(false);
        controls.offsets[0] = Err(scalar_error("offset"));
        assert_eq!(run(&mut capture, &controls), Err(scalar_error("offset").into()));
        assert_eq!(reads.get(), 1, "source reads the current angle before its offset");
        controls.offsets[0] = Ok(1.0);
        run(&mut capture, &controls).unwrap();
        assert_eq!(reads.get(), 3, "each tilt axis reads the current sensor angles");
    }

    #[test]
    fn native_kind_pose_tracker_ignores_typed_mode_and_preserves_state_on_missing_type() {
        let settings = settings(CompensationKind::Virtual);
        let input = inputs(&settings, &OTHER_RIG, &NO_POSITIONS);
        let mut tracker = PoseTracker::new();
        let sensor = Ok(CompensationKind::Sensor);
        tracker
            .update(Some([12.0, 24.0, 0.0]), None, &CompensationInputs { native_kind: Some(&sensor), ..input })
            .unwrap();
        let old = tracker.sensor;
        let missing = Err(scalar_error("type"));
        assert_eq!(
            tracker.update(None, None, &CompensationInputs { native_kind: Some(&missing), ..input }),
            Err(scalar_error("type").into())
        );
        assert_eq!(tracker.sensor, old);
        assert_eq!(
            tracker.update(
                Some([99.0, 99.0, 0.0]),
                None,
                &CompensationInputs { native_kind: Some(&Ok(CompensationKind::Off)), ..input }
            ),
            Ok(None)
        );
        assert_eq!(tracker.sensor, MotionSensor::default());
    }

    #[test]
    fn native_spacing_fails_only_in_reached_generic_actuator_branch() {
        let settings = settings(CompensationKind::Virtual);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let mut controls = axis_controls();
        controls.actuator_spacing_mm = Err(scalar_error("mm_distance_left_right"));
        let input =
            CompensationInputs { native_axis_controls: Some(&controls), ..inputs(&settings, &OTHER_RIG, &positions) };
        let values = MotionSensorValues::default();
        let mut sensor = MotionSensor::default();
        sensor.pass(TiltSource::Sensor([12.0, 24.0, 0.0]), &input, Some(&values)).unwrap();
        let before = values.snapshot();
        assert_eq!(
            sensor.pass(TiltSource::Actuators, &input, Some(&values)),
            Err(scalar_error("mm_distance_left_right").into())
        );
        assert_eq!(values.snapshot(), before);
        let dof = Rig { model: RigModel::Other, dof_reality: Some(true), horizontal_surge: None };
        sensor.pass(TiltSource::Actuators, &CompensationInputs { rig: &dof, ..input }, None).unwrap();
        sensor.pass(TiltSource::Actuators, &CompensationInputs { switch_on: false, ..input }, None).unwrap();
        let unknown = Rig { dof_reality: None, ..OTHER_RIG };
        assert_eq!(
            sensor.pass(TiltSource::Actuators, &CompensationInputs { rig: &unknown, ..input }, None),
            Err(CompensationError::UnknownModel)
        );
    }

    #[test]
    fn native_spacing_preserves_control_precedence_and_zero_division_detail() {
        let settings = settings(CompensationKind::Virtual);
        let mut controls = axis_controls();
        controls.actuator_spacing_mm = Err(scalar_error("mm_distance_left_right"));
        controls.swap_roll_pitch = Err(scalar_error("swap_roll_pitch"));
        controls.reverse[0] = Err(scalar_error("reverse"));
        let mut sensor = MotionSensor::default();
        let check = |sensor: &mut MotionSensor, controls: &NativeAxisControls| {
            sensor.pass(
                TiltSource::Actuators,
                &CompensationInputs {
                    native_axis_controls: Some(controls),
                    ..inputs(&settings, &OTHER_RIG, &NO_POSITIONS)
                },
                None,
            )
        };
        assert_eq!(check(&mut sensor, &controls), Err(scalar_error("reverse").into()));
        controls.reverse[0] = Ok(false);
        assert_eq!(check(&mut sensor, &controls), Err(scalar_error("swap_roll_pitch").into()));
        controls.swap_roll_pitch = Ok(true);
        controls.reverse[1] = Err(scalar_error("target_reverse"));
        assert_eq!(check(&mut sensor, &controls), Err(scalar_error("target_reverse").into()));
        controls.reverse[1] = Ok(false);
        assert_eq!(check(&mut sensor, &controls), Err(scalar_error("mm_distance_left_right").into()));
        controls.actuator_spacing_mm = Ok(0.into());
        assert_eq!(
            check(&mut sensor, &controls),
            Err(NativeCompensationError { class: "ZeroDivisionError", message: "float division by zero".into() }.into())
        );
    }

    #[test]
    fn native_spacing_uses_same_distance_for_both_angles_and_keeps_legacy_error() {
        let settings = settings(CompensationKind::Virtual);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let mut controls = axis_controls();
        controls.actuator_spacing_mm = Ok(750.into());
        let mut sensor = MotionSensor::default();
        sensor
            .pass(
                TiltSource::Actuators,
                &CompensationInputs {
                    native_axis_controls: Some(&controls),
                    ..inputs(&settings, &OTHER_RIG, &positions)
                },
                None,
            )
            .unwrap();
        assert!((sensor.pitch.raw - 0.2_f64.atan().to_degrees()).abs() < 1e-12);
        assert!((sensor.roll.raw - (1.0_f64 / 15.0).atan().to_degrees()).abs() < 1e-12);
        let unspaced = CompensationSettings { actuator_spacing_mm: 0, ..settings };
        assert_eq!(
            sensor.pass(TiltSource::Actuators, &inputs(&unspaced, &OTHER_RIG, &positions), None),
            Err(CompensationError::ZeroActuatorSpacing)
        );
    }

    #[test]
    fn native_controls_reverse_failure_precedes_current_axis_and_preserves_prior_rows() {
        let settings = settings(CompensationKind::Sensor);
        let input = inputs(&settings, &OTHER_RIG, &NO_POSITIONS);
        for failed in 0..6 {
            let values = MotionSensorValues::default();
            let mut sensor = MotionSensor::default();
            let mut controls = axis_controls();
            sensor
                .pass(
                    TiltSource::Sensor([12.0, 24.0, 0.0]),
                    &CompensationInputs { native_axis_controls: Some(&controls), ..input },
                    Some(&values),
                )
                .unwrap();
            let before = values.snapshot();
            controls.reverse[failed] = Err(scalar_error("reverse"));
            let result = sensor.pass(
                TiltSource::Sensor([18.0, 36.0, 0.0]),
                &CompensationInputs { native_axis_controls: Some(&controls), ..input },
                Some(&values),
            );
            assert_eq!(result, Err(scalar_error("reverse").into()));
            for axis in Axis::ORDER.into_iter().skip(failed) {
                assert_eq!(values.get(axis.name()), before.get(axis.name()).cloned());
                assert_eq!(
                    values.get(&format!("calculated_{}", axis.name())),
                    before.get(&format!("calculated_{}", axis.name())).cloned()
                );
            }
            if failed > 0 {
                assert_eq!(sensor.pitch.raw, 18.0);
            }
        }
    }

    #[test]
    fn native_controls_swap_and_target_reverse_are_required_before_sensor_offset() {
        let settings = settings(CompensationKind::Sensor);
        let input = inputs(&settings, &OTHER_RIG, &NO_POSITIONS);
        let values = MotionSensorValues::default();
        let initial = values.snapshot();
        let mut controls = axis_controls();
        controls.swap_roll_pitch = Err(scalar_error("swap"));
        controls.offsets[0] = Err(scalar_error("offset"));
        let mut sensor = MotionSensor::default();
        assert_eq!(
            sensor.pass(
                TiltSource::Sensor([18.0, 36.0, 0.0]),
                &CompensationInputs { native_axis_controls: Some(&controls), ..input },
                Some(&values)
            ),
            Err(scalar_error("swap").into())
        );
        assert_eq!(values.get("raw_pitch"), initial.get("raw_pitch").cloned());
        controls.swap_roll_pitch = Ok(true);
        controls.reverse[1] = Err(scalar_error("target_reverse"));
        assert_eq!(
            sensor.pass(
                TiltSource::Sensor([18.0, 36.0, 0.0]),
                &CompensationInputs { native_axis_controls: Some(&controls), ..input },
                Some(&values)
            ),
            Err(scalar_error("target_reverse").into())
        );
        assert_eq!(values.get("calculated_roll"), initial.get("calculated_roll").cloned());
        controls.reverse[1] = Ok(true);
        assert_eq!(
            sensor.pass(
                TiltSource::Sensor([18.0, 36.0, 0.0]),
                &CompensationInputs { native_axis_controls: Some(&controls), ..input },
                Some(&values)
            ),
            Err(scalar_error("offset").into())
        );
        assert_eq!(values.get("raw_roll"), initial.get("raw_roll").cloned());
        controls.offsets[0] = Ok(1.0);
        sensor
            .pass(
                TiltSource::Sensor([18.0, 36.0, 0.0]),
                &CompensationInputs { native_axis_controls: Some(&controls), ..input },
                Some(&values),
            )
            .unwrap();
        assert_eq!(sensor.roll.calculated, -19.0);
        assert_eq!(sensor.roll.raw, 18.0);
        assert_eq!(sensor.pitch.calculated, 38.0);
        assert_eq!(sensor.pitch.raw, 36.0);
    }

    #[test]
    fn native_controls_virtual_skips_sensor_offsets_and_switch_off_skips_all_controls() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let settings = settings(CompensationKind::Virtual);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let mut controls = axis_controls();
        controls.offsets = [Err(scalar_error("pitch_offset")), Err(scalar_error("roll_offset"))];
        let mut capture = Compensation::new();
        capture
            .cycle(
                None,
                &CompensationInputs {
                    native_axis_controls: Some(&controls),
                    ..inputs(&settings, &OTHER_RIG, &positions)
                },
                &clock,
            )
            .unwrap();
        assert_ne!(capture.pose().pitch, 0.0);
        controls.swap_roll_pitch = Err(scalar_error("swap"));
        controls.reverse = std::array::from_fn(|_| Err(scalar_error("reverse")));
        capture
            .cycle(
                None,
                &CompensationInputs {
                    native_axis_controls: Some(&controls),
                    switch_on: false,
                    ..inputs(&settings, &OTHER_RIG, &positions)
                },
                &clock,
            )
            .unwrap();
        assert_eq!(capture.sensor().pitch.value, 0.0);
    }

    #[test]
    fn native_enabled_error_retains_current_live_after_calculation_and_later_prefix() {
        let settings = settings(CompensationKind::Sensor);
        let input = inputs(&settings, &OTHER_RIG, &NO_POSITIONS);
        for failed in 0..6 {
            let mut enabled = std::array::from_fn(|_| Ok(true));
            let values = MotionSensorValues::default();
            let mut sensor = MotionSensor::default();
            sensor
                .pass(
                    TiltSource::Sensor([12.0, 24.0, 0.0]),
                    &CompensationInputs { native_axis_enabled: Some(&enabled), ..input },
                    Some(&values),
                )
                .unwrap();
            let before = values.snapshot();
            for (index, axis) in Axis::ORDER.into_iter().enumerate() {
                values.set(&format!("calculated_{}", axis.name()), LegacyValue::Float(1000.0 + index as f64));
            }
            let calculated_before = values.snapshot();
            let error = scalar_error("enabled");
            enabled[failed] = Err(error.clone());
            assert_eq!(
                sensor.pass(
                    TiltSource::Sensor([18.0, 36.0, 0.0]),
                    &CompensationInputs { native_axis_enabled: Some(&enabled), ..input },
                    Some(&values)
                ),
                Err(error.into())
            );
            let failed_axis = Axis::ORDER[failed];
            assert_ne!(
                values.get(&format!("calculated_{}", failed_axis.name())),
                calculated_before.get(&format!("calculated_{}", failed_axis.name())).cloned(),
                "calculation finishes before enabled read"
            );
            assert_eq!(
                values.get(failed_axis.name()),
                before.get(failed_axis.name()).cloned(),
                "current live value not yet written"
            );
            for axis in Axis::ORDER.into_iter().skip(failed + 1) {
                assert_eq!(values.get(axis.name()), before.get(axis.name()).cloned());
                assert_eq!(
                    values.get(&format!("calculated_{}", axis.name())),
                    calculated_before.get(&format!("calculated_{}", axis.name())).cloned()
                );
            }
        }
    }

    #[test]
    fn native_enabled_error_with_switch_off_keeps_early_integer_zero_and_stops_later_rows() {
        let settings = settings(CompensationKind::Sensor);
        let mut enabled = std::array::from_fn(|_| Ok(true));
        let input = inputs(&settings, &OTHER_RIG, &NO_POSITIONS);
        let mut sensor = MotionSensor::default();
        let values = MotionSensorValues::default();
        sensor
            .pass(
                TiltSource::Sensor([12.0, 24.0, 0.0]),
                &CompensationInputs { native_axis_enabled: Some(&enabled), ..input },
                Some(&values),
            )
            .unwrap();
        let previous = values.snapshot();
        enabled[1] = Err(scalar_error("enabled"));
        assert!(sensor
            .pass(
                TiltSource::Sensor([18.0, 36.0, 0.0]),
                &CompensationInputs { native_axis_enabled: Some(&enabled), switch_on: false, ..input },
                Some(&values)
            )
            .is_err());
        for axis in [Axis::Pitch, Axis::Roll] {
            assert_eq!(values.get(axis.name()), Some(LegacyValue::Int(0)));
        }
        for axis in Axis::ORDER.into_iter().skip(2) {
            assert_eq!(values.get(axis.name()), previous.get(axis.name()).cloned());
        }
        for axis in Axis::ORDER {
            assert_eq!(
                values.get(&format!("calculated_{}", axis.name())),
                previous.get(&format!("calculated_{}", axis.name())).cloned()
            );
        }
    }

    #[test]
    fn native_enabled_controls_live_row_and_is_skipped_only_outside_capture() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let settings = settings(CompensationKind::Virtual);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let mut enabled = std::array::from_fn(|_| Ok(true));
        enabled[0] = Ok(false);
        let mut capture = Compensation::new();
        let input =
            CompensationInputs { native_axis_enabled: Some(&enabled), ..inputs(&settings, &OTHER_RIG, &positions) };
        capture.cycle(None, &input, &clock).unwrap();
        assert_eq!(capture.sensor_values().get("pitch"), Some(LegacyValue::Int(0)));
        assert_ne!(capture.sensor().pitch.calculated, 0.0);
        enabled[0] = Ok(true);
        capture
            .cycle(
                None,
                &CompensationInputs {
                    native_axis_enabled: Some(&enabled),
                    ..inputs(&settings, &OTHER_RIG, &positions)
                },
                &clock,
            )
            .unwrap();
        assert_ne!(capture.pose().pitch, 0.0);
        enabled[0] = Err(scalar_error("enabled"));
        for (kind, pause) in [(CompensationKind::Off, 1), (CompensationKind::Sensor, 2)] {
            let idle = CompensationSettings { kind, ..settings };
            assert_eq!(
                Compensation::new().cycle(
                    None,
                    &CompensationInputs {
                        native_axis_enabled: Some(&enabled),
                        ..inputs(&idle, &OTHER_RIG, &positions)
                    },
                    &clock
                ),
                Ok(Duration::from_secs(pause))
            );
        }
    }

    fn native_horizontal() -> NativeHorizontalSurgeSettings {
        NativeHorizontalSurgeSettings { stroke_mm: Ok(123.into()), reverse: Ok(true), factor_percent: Ok(71.into()) }
    }

    #[test]
    fn native_horizontal_reads_stroke_before_stream_and_skips_later_reads_without_position() {
        let mut native = native_horizontal();
        let settings = settings(CompensationKind::Virtual).horizontal_surge;
        let absent = HorizontalSurgeActuator { position: None, max_motor_abs: 65_536 };
        native.stroke_mm = Err(scalar_error("surge_horizontal_stroke"));
        native.reverse = Err(scalar_error("surge_horizontal_reverse"));
        native.factor_percent = Err(scalar_error("surge_horizontal_factor"));
        assert_eq!(
            horizontal_surge_native(&absent, &settings, Some(&native)),
            Err(scalar_error("surge_horizontal_stroke").into())
        );
        native.stroke_mm = Ok(123.into());
        assert_eq!(horizontal_surge_native(&absent, &settings, Some(&native)), Ok(0.0));
        let present = HorizontalSurgeActuator { position: Some(LegacyValue::Float(49_152.0)), ..absent };
        assert_eq!(
            horizontal_surge_native(&present, &settings, Some(&native)),
            Err(scalar_error("surge_horizontal_reverse").into())
        );
        native.reverse = Ok(true);
        assert_eq!(
            horizontal_surge_native(&present, &settings, Some(&native)),
            Err(scalar_error("surge_horizontal_factor").into())
        );
    }

    #[test]
    fn native_horizontal_preserves_prior_tilt_and_later_axes_on_failure_then_recovers() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let settings = settings(CompensationKind::Virtual);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let rig = Rig {
            horizontal_surge: Some(HorizontalSurgeActuator {
                position: Some(LegacyValue::Float(49_152.0)),
                max_motor_abs: 65_536,
            }),
            ..OTHER_RIG
        };
        let displacement = native_displacement();
        let mut native = native_horizontal();
        let mut capture = Compensation::new();
        let mut run = |native: &NativeHorizontalSurgeSettings, positions: &ActuatorPositions| {
            let input = CompensationInputs {
                native_displacement: Some(&displacement),
                native_horizontal_surge: Some(native),
                ..inputs(&settings, &rig, positions)
            };
            let result = capture.cycle(None, &input, &clock);
            (result, *capture.sensor(), capture.pose(), capture.sensor_values().snapshot())
        };
        let good = run(&native, &positions);
        assert!(good.0.is_ok());
        let changed = ActuatorPositions {
            corners: Corners { front_left: 300.0, front_right: 50.0, rear_left: 0.0, rear_right: 0.0 },
            ..positions
        };
        for stage in 0..3 {
            native = native_horizontal();
            let failure =
                scalar_error(["surge_horizontal_stroke", "surge_horizontal_reverse", "surge_horizontal_factor"][stage]);
            match stage {
                0 => native.stroke_mm = Err(failure.clone()),
                1 => native.reverse = Err(failure.clone()),
                _ => native.factor_percent = Err(failure.clone()),
            }
            let bad = run(&native, &changed);
            assert_eq!(bad.0, Err(failure.into()));
            assert_ne!(bad.1.pitch, good.1.pitch);
            assert_ne!(bad.1.roll, good.1.roll);
            assert_ne!(bad.1.sway, good.1.sway);
            assert_eq!(bad.1.surge.value, good.1.surge.value, "failed surge live row is retained");
            assert_eq!(bad.1.yaw, good.1.yaw);
            assert_eq!(bad.1.heave, good.1.heave);
            assert_eq!(bad.2, good.2, "export requires complete axis pass");
            let tilt_only = bad.1.pitch.calculated.to_radians().tan() * 321.0 * 70.0 / 100.0;
            assert_eq!(bad.3.get("calculated_surge"), Some(&LegacyValue::Float(tilt_only)));
        }
        native = native_horizontal();
        let recovered = run(&native, &changed);
        assert!(recovered.0.is_ok());
        assert_ne!(recovered.2, good.2);
    }

    #[test]
    fn native_horizontal_normal_mapping_and_inactive_gates_match() {
        let native = native_horizontal();
        let settings = settings(CompensationKind::Virtual);
        let compatibility = HorizontalSurgeSettings { stroke_mm: 123, reverse: true, factor_percent: 71 };
        for (position, maximum) in
            [(49_152.0, 65_536), (-1.0, 65_536), (80_000.0, 65_536), (f64::NAN, 65_536), (0.0, 0)]
        {
            let actuator =
                HorizontalSurgeActuator { position: Some(LegacyValue::Float(position)), max_motor_abs: maximum };
            assert_eq!(
                horizontal_surge_native(&actuator, &settings.horizontal_surge, Some(&native)),
                horizontal_surge(&actuator, &compatibility)
            );
        }
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let bad = NativeHorizontalSurgeSettings { stroke_mm: Err(scalar_error("stroke")), ..native };
        let input =
            CompensationInputs { native_horizontal_surge: Some(&bad), ..inputs(&settings, &OTHER_RIG, &NO_POSITIONS) };
        let mut capture = Compensation::new();
        assert!(capture.cycle(None, &input, &clock).is_ok(), "no horizontal actuator skips its settings");
        let rig = Rig {
            horizontal_surge: Some(HorizontalSurgeActuator { position: None, max_motor_abs: 65_536 }),
            ..OTHER_RIG
        };
        assert!(
            capture.cycle(None, &CompensationInputs { rig: &rig, switch_on: false, ..input }, &clock).is_ok(),
            "switch off skips all calculations"
        );
    }

    #[test]
    fn native_refresh_timeout_rounds_up_and_preserves_conversion_errors() {
        for (rate, nanoseconds) in
            [(1, 1_000_000_000), (90, 11_111_112), (7, 142_857_143), (30, 33_333_334), (i64::MAX, 1)]
        {
            let nanoseconds: u64 = nanoseconds;
            let expected = if cfg!(target_os = "windows") { nanoseconds.div_ceil(100) * 100 } else { nanoseconds };
            assert_eq!(native_refresh_pause(&Ok(rate.into())), Ok(Duration::from_nanos(expected)), "{rate} Hz");
        }
        for (rate, class, message) in [
            (0, "ZeroDivisionError", "float division by zero"),
            (-5, "ValueError", "sleep length must be non-negative"),
            (i64::MIN, "ValueError", "sleep length must be non-negative"),
        ] {
            assert_eq!(
                native_refresh_pause(&Ok(rate.into())),
                Err(NativeCompensationError { class, message: message.into() }.into())
            );
        }
        let failure = scalar_error("refresh_rate");
        assert_eq!(native_refresh_pause(&Err(failure.clone())), Err(failure.into()));
        assert_eq!(refresh_pause(90), Ok(Duration::from_secs_f64(1.0 / 90.0)), "compatibility API stays unchanged");
    }

    #[test]
    fn native_refresh_failure_is_after_export_and_reached_with_switch_off() {
        let clock = Arc::new(super::super::link::fakes::ManualClock::default());
        let capture_clock: Arc<dyn Clock> = clock.clone();
        let settings = settings(CompensationKind::Virtual);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let failure = Err(scalar_error("refresh_rate"));
        let input =
            CompensationInputs { native_refresh_rate: Some(&failure), ..inputs(&settings, &OTHER_RIG, &positions) };
        let mut capture = Compensation::new();
        let error = capture.cycle(None, &input, &capture_clock).unwrap_err();
        assert_eq!(error, failure.as_ref().unwrap_err().clone().into());
        assert_eq!(capture.pose(), capture.sensor().pose());
        assert_ne!(capture.pose().pitch, 0.0);
        assert_ne!(capture.pose().roll, 0.0);
        assert_eq!(capture.sensor_values().get("heave"), Some(LegacyValue::Int(0)));
        let held = capture.pose();
        assert_eq!(
            capture.cycle(None, &CompensationInputs { switch_on: false, ..input }, &capture_clock),
            Err(error),
            "the switch gates axes, not the end-of-capture rate read"
        );
        assert_eq!(capture.pose(), held, "zero-total output retains the previous export");
        for axis in ["pitch", "roll", "sway", "surge", "yaw", "heave"] {
            assert_eq!(capture.sensor_values().get(axis), Some(LegacyValue::Int(0)));
        }
        let mut tracker = PoseTracker::new();
        assert!(tracker.update(None, None, &input).unwrap().is_some(), "pose sampling has no capture sleep");
        assert_eq!(clock.elapsed(), Duration::ZERO, "no sleeping or worker is executed");
    }

    #[test]
    fn native_refresh_failure_is_skipped_while_idle_and_after_earlier_axis_error() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let failure = Err(scalar_error("refresh_rate"));
        let mut capture = Compensation::new();
        for (kind, seconds) in [(CompensationKind::Off, 1), (CompensationKind::Sensor, 2)] {
            let settings = settings(kind);
            let input = CompensationInputs {
                native_refresh_rate: Some(&failure),
                ..inputs(&settings, &OTHER_RIG, &NO_POSITIONS)
            };
            assert_eq!(capture.cycle(None, &input, &clock), Ok(Duration::from_secs(seconds)));
        }
        let settings = settings(CompensationKind::Virtual);
        let mut displacement = native_displacement();
        let earlier = scalar_error("height");
        displacement.height = Err(earlier.clone());
        let input = CompensationInputs {
            native_displacement: Some(&displacement),
            native_refresh_rate: Some(&failure),
            ..inputs(&settings, &OTHER_RIG, &NO_POSITIONS)
        };
        assert_eq!(capture.cycle(None, &input, &clock), Err(earlier.into()));
        assert_eq!(capture.pose(), RigPose::default());
        displacement.height = Ok(321.0);
        let input = CompensationInputs {
            native_displacement: Some(&displacement),
            native_refresh_rate: Some(&failure),
            ..inputs(&settings, &OTHER_RIG, &NO_POSITIONS)
        };
        assert_eq!(capture.cycle(None, &input, &clock), Err(failure.as_ref().unwrap_err().clone().into()));
    }

    #[test]
    fn native_displacement_factor_error_preserves_unscaled_row_and_later_prefix() {
        for axis in [Axis::Sway, Axis::Surge] {
            let settings = settings(CompensationKind::Sensor);
            let mut native = native_displacement();
            let input = inputs(&settings, &OTHER_RIG, &NO_POSITIONS);
            let values = MotionSensorValues::default();
            let mut sensor = MotionSensor::default();
            sensor
                .pass(
                    TiltSource::Sensor([12.0, 24.0, 0.0]),
                    &CompensationInputs { native_displacement: Some(&native), ..input },
                    Some(&values),
                )
                .unwrap();
            let mut before = sensor;
            let snapshots = values.snapshot();
            let failure = scalar_error(axis.name());
            if axis == Axis::Sway {
                native.sway_factor = Err(failure.clone());
            } else {
                native.surge_factor = Err(failure.clone());
            }
            let error = sensor
                .pass(
                    TiltSource::Sensor([18.0, 36.0, 0.0]),
                    &CompensationInputs { native_displacement: Some(&native), ..input },
                    Some(&values),
                )
                .unwrap_err();
            assert_eq!(error, CompensationError::NativeScalar(failure));
            let angle: f64 = if axis == Axis::Sway { -36.0 } else { 19.0 };
            let unscaled = angle.to_radians().tan() * 321.0;
            assert_eq!(values.get(&format!("calculated_{}", axis.name())), Some(LegacyValue::Float(unscaled)));
            assert_eq!(
                values.get(axis.name()),
                snapshots.get(axis.name()).cloned(),
                "failed row's live value retained"
            );
            assert_eq!(sensor.axis_mut(axis).value, before.axis_mut(axis).value);
            for later in if axis == Axis::Sway {
                &[Axis::Surge, Axis::Yaw, Axis::Heave][..]
            } else {
                &[Axis::Yaw, Axis::Heave][..]
            } {
                assert_eq!(values.get(later.name()), snapshots.get(later.name()).cloned(), "later axis {later:?}");
            }
            assert_eq!(sensor.pitch.raw, 18.0);
            assert_eq!(sensor.roll.raw, 36.0);
        }
    }

    #[test]
    fn native_displacement_height_error_precedes_row_write_and_inactive_skips_it() {
        let settings = settings(CompensationKind::Sensor);
        let mut native = native_displacement();
        let input = inputs(&settings, &OTHER_RIG, &NO_POSITIONS);
        let values = MotionSensorValues::default();
        let mut sensor = MotionSensor::default();
        sensor
            .pass(
                TiltSource::Sensor([12.0, 24.0, 0.0]),
                &CompensationInputs { native_displacement: Some(&native), ..input },
                Some(&values),
            )
            .unwrap();
        let previous = values.get("calculated_sway");
        native.height = Err(scalar_error("height"));
        assert!(sensor
            .pass(
                TiltSource::Sensor([18.0, 36.0, 0.0]),
                &CompensationInputs { native_displacement: Some(&native), ..input },
                Some(&values)
            )
            .is_err());
        assert_eq!(values.get("calculated_sway"), previous);
        let off = CompensationInputs { native_displacement: Some(&native), switch_on: false, ..input };
        sensor.pass(TiltSource::Sensor([18.0, 36.0, 0.0]), &off, Some(&values)).unwrap();
        assert_eq!(values.get("calculated_sway"), previous);
        assert_eq!(values.get("sway"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn native_displacement_capture_and_tracker_keep_pose_until_recovery() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let settings = settings(CompensationKind::Virtual);
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let mut native = native_displacement();
        let mut capture = Compensation::new();
        let mut tracker = PoseTracker::new();
        let mut run = |native: &NativeDisplacementSettings| {
            let input =
                CompensationInputs { native_displacement: Some(native), ..inputs(&settings, &OTHER_RIG, &positions) };
            let captured = capture.cycle(None, &input, &clock);
            let tracked = tracker.update(None, None, &input);
            (captured, tracked, capture.pose(), capture.sensor_values().snapshot())
        };
        let good = run(&native);
        assert!(good.0.is_ok());
        assert_ne!(good.2.sway, 0.0);
        native.surge_factor = Err(scalar_error("surge_factor"));
        let bad = run(&native);
        assert!(bad.0.is_err());
        assert!(bad.1.is_err());
        assert_eq!(bad.2, good.2, "capture output is published after the complete pass");
        assert_eq!(bad.3.get("sway"), good.3.get("sway"), "earlier axis completes");
        assert_eq!(bad.3.get("surge"), good.3.get("surge"), "live failed axis remains prior");
        native.surge_factor = Ok(70.into());
        let recovered = run(&native);
        assert_eq!(recovered.2, good.2);
        assert_eq!(recovered.1.unwrap().unwrap().pose, good.1.unwrap().unwrap().pose);
    }

    fn settings(kind: CompensationKind) -> CompensationSettings {
        let on = AxisSettings { enabled: true, reverse: false, offset: 0.0 };
        CompensationSettings {
            kind,
            pitch: AxisSettings { offset: 1.0, ..on },
            roll: AxisSettings { reverse: true, ..on },
            sway: on,
            surge: on,
            yaw: on,
            heave: AxisSettings { enabled: false, ..on },
            swap_roll_pitch: false,
            pitch_factor: 100,
            roll_factor: 100,
            yaw_factor: 100,
            estimator_factor_failures: [false; 3],
            native_estimator_factors: None,
            surge_factor: 100,
            sway_factor: 100,
            height: 0.0,
            actuator_spacing_mm: 558,
            horizontal_surge: HorizontalSurgeSettings { stroke_mm: 100, reverse: false, factor_percent: 100 },
            refresh_rate: 90,
        }
    }

    fn horizontal() -> &'static InstallationReader {
        static READER: std::sync::OnceLock<InstallationReader> = std::sync::OnceLock::new();
        READER.get_or_init(|| Arc::new(|| "H".to_owned()))
    }

    const OTHER_RIG: Rig = Rig { model: RigModel::Other, dof_reality: Some(false), horizontal_surge: None };
    const NO_POSITIONS: ActuatorPositions = ActuatorPositions {
        corners: Corners { front_left: 0.0, front_right: 0.0, rear_left: 0.0, rear_right: 0.0 },
        motors: None,
    };

    fn inputs<'a>(
        settings: &'a CompensationSettings,
        rig: &'a Rig,
        actuators: &'a ActuatorPositions,
    ) -> CompensationInputs<'a> {
        CompensationInputs {
            settings,
            native_kind: None,
            native_displacement: None,
            native_refresh_rate: None,
            native_horizontal_surge: None,
            native_axis_enabled: None,
            native_axis_controls: None,
            switch_on: true,
            rig,
            actuators,
            heave_from_actuators: true,
            game_running: Some("none"),
            installation: horizontal(),
            native_installation: None,
        }
    }

    /// 'W' session: pitch/roll from the IMU angles (offset added, then reversed), yaw
    /// 0 off P6/H6, heave disabled, surge/sway from the tilts (height 0 here), export, 1 / refresh_rate.
    #[test]
    fn sensor_pass() {
        let (imu, _, clock) = connected_imu();
        let s = settings(CompensationKind::Sensor);
        let mut c = Compensation::new();
        let pause = c.cycle(Some(&imu), &inputs(&s, &OTHER_RIG, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(pause, Duration::from_secs_f64(1.0 / 90.0));
        let m = c.sensor();
        assert_eq!(m.pitch, AxisReading { value: 91.0, calculated: 91.0, raw: 90.0 });
        assert_eq!(m.roll, AxisReading { value: 45.0, calculated: 45.0, raw: -45.0 });
        assert_eq!(m.yaw.value, 0.0);
        assert_eq!(m.heave, AxisReading { value: 0.0, calculated: 0.0, raw: 0.0 });
        assert_eq!(c.pose(), RigPose { sway: 0.0, surge: 0.0, heave: 0.0, yaw: 0.0, roll: 45.0, pitch: 91.0 });
        let values = c.sensor_values();
        assert_eq!(values.get("raw_pitch"), Some(LegacyValue::Float(90.0)));
        assert_eq!(values.get("calculated_pitch"), Some(LegacyValue::Float(91.0)));
        assert_eq!(values.get("pitch"), Some(LegacyValue::Float(91.0)));
        assert_eq!(values.get("yaw"), Some(LegacyValue::Int(0)));
        assert_eq!(values.get("heave"), Some(LegacyValue::Int(0)));
    }

    /// With swap_roll_pitch: the axis is swapped for the stored values and the
    /// reversal, while the IMU index and the offset follow the iterated axis; pitch is gated before the
    /// roll iteration writes calculated_pitch, so it lags one pass.
    #[test]
    fn swap_roll_pitch_follows_the_loop_order() {
        let (imu, _, clock) = connected_imu();
        let s = CompensationSettings { swap_roll_pitch: true, ..settings(CompensationKind::Sensor) };
        let mut c = Compensation::new();
        c.cycle(Some(&imu), &inputs(&s, &OTHER_RIG, &NO_POSITIONS), &clock).unwrap();
        // 'pitch' iteration: Angle[0] = 90 + pitch offset 1, roll's reversal -> calculated_roll -91.
        // 'roll' iteration: Angle[1] = -45 + roll offset 0, pitch's factor 1 -> calculated_pitch -45.
        assert_eq!(c.sensor().roll, AxisReading { value: -91.0, calculated: -91.0, raw: 90.0 });
        assert_eq!(c.sensor().pitch, AxisReading { value: 0.0, calculated: -45.0, raw: -45.0 });
        assert_eq!(c.sensor_values().get("pitch"), Some(LegacyValue::Int(0)));
        c.cycle(Some(&imu), &inputs(&s, &OTHER_RIG, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(c.sensor().pitch.value, -45.0);
        assert_eq!(c.sensor_values().get("pitch"), Some(LegacyValue::Float(-45.0)));
    }

    /// Gates: with the switch off every axis is 0 and the calculated values are left alone,
    /// and the zero total leaves the export as it was; the next non-zero pass replaces it.
    #[test]
    fn switch_and_zero_total_gates() {
        let (imu, _, clock) = connected_imu();
        let s = settings(CompensationKind::Sensor);
        let mut c = Compensation::new();
        c.cycle(Some(&imu), &inputs(&s, &OTHER_RIG, &NO_POSITIONS), &clock).unwrap();
        let exported = c.pose();
        let off = CompensationInputs { switch_on: false, ..inputs(&s, &OTHER_RIG, &NO_POSITIONS) };
        c.cycle(Some(&imu), &off, &clock).unwrap();
        assert_eq!(c.sensor().pitch, AxisReading { value: 0.0, calculated: 91.0, raw: 90.0 });
        assert_eq!(c.pose(), exported, "a zero total leaves data_out unchanged");
        for axis in Axis::ORDER {
            assert_eq!(c.sensor_values().get(axis.name()), Some(LegacyValue::Int(0)));
        }
        assert_eq!(c.sensor_values().get("raw_pitch"), Some(LegacyValue::Float(90.0)));
        let reversed = CompensationSettings { pitch: AxisSettings { reverse: true, ..s.pitch }, ..s };
        c.cycle(Some(&imu), &inputs(&reversed, &OTHER_RIG, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(c.sensor().pitch.value, -91.0);
        assert_eq!(c.pose().pitch, -91.0);
    }

    /// The installation code is compared (as text) after each pass' pause, from the second
    /// pass of a session on; a change starts the sensor-configuration thread, which reads the code
    /// again ('H' horizontal, anything else vertical).
    #[test]
    fn resends_changed_installation() {
        let (imu, link, clock) = connected_imu();
        let s = settings(CompensationKind::Sensor);
        let code = Arc::new(Mutex::new("V".to_owned()));
        let shared = Arc::clone(&code);
        let reader: InstallationReader = Arc::new(move || shared.lock().unwrap().clone());
        let set = |value: &str| *code.lock().unwrap() = value.to_owned();
        let pass = |c: &mut Compensation| {
            c.cycle(
                Some(&imu),
                &CompensationInputs { installation: &reader, ..inputs(&s, &OTHER_RIG, &NO_POSITIONS) },
                &clock,
            )
            .unwrap()
        };
        let mut c = Compensation::new();
        pass(&mut c);
        set("H");
        pass(&mut c);
        assert!(link.written().is_empty(), "the first comparison has no previous installation");
        set("x");
        pass(&mut c);
        let expected =
            [command::UNLOCK, command::RATE_200HZ, command::BANDWIDTH_42HZ, command::VERTICAL, command::SAVE].concat();
        for _ in 0..100_000 {
            if link.written().len() >= expected.len() {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(link.written(), expected);
    }

    /// Idle paths: a lost sensor ends the 'W' session (1 s); outside a session the
    /// readouts are reset and the pause is 1 s (+1 s in 'W' mode); a bad rate fails after the pass.
    #[test]
    fn idle_paths_and_rate_errors() {
        let (imu, _, clock) = connected_imu();
        let s = settings(CompensationKind::Sensor);
        let i = inputs(&s, &OTHER_RIG, &NO_POSITIONS);
        let mut c = Compensation::new();
        c.cycle(Some(&imu), &i, &clock).unwrap();
        assert_eq!(c.cycle(None, &i, &clock), Ok(Duration::from_secs(1)));
        assert_eq!(c.sensor().pitch.value, 91.0);
        assert_eq!(c.cycle(None, &i, &clock), Ok(Duration::from_secs(2)));
        assert_eq!(c.sensor(), &MotionSensor::default());
        assert_eq!(c.pose().pitch, 91.0, "data_out keeps the last pose");
        let off = settings(CompensationKind::Off);
        assert_eq!(c.cycle(Some(&imu), &inputs(&off, &OTHER_RIG, &NO_POSITIONS), &clock), Ok(Duration::from_secs(1)));
        for rate in [0, -5] {
            let stalled = CompensationSettings { refresh_rate: rate, ..s };
            assert_eq!(
                c.cycle(Some(&imu), &inputs(&stalled, &OTHER_RIG, &NO_POSITIONS), &clock),
                Err(CompensationError::BadRefreshRate(rate))
            );
            assert_eq!(c.sensor().pitch.value, 91.0, "the pass ran before the sleep failed");
        }
    }

    const P6: Rig = Rig { model: RigModel::P6, dof_reality: Some(true), horizontal_surge: None };

    #[test]
    fn native_estimator_factor_failures_are_local_and_preserve_integer_zero() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let positions = ActuatorPositions { motors: Some([600, 420, 700, 350, 510, 730]), ..NO_POSITIONS };
        let base = CompensationSettings {
            yaw: AxisSettings { enabled: true, ..settings(CompensationKind::Virtual).yaw },
            ..settings(CompensationKind::Virtual)
        };
        let mut good = Compensation::new();
        good.cycle(None, &inputs(&base, &P6, &positions), &clock).unwrap();
        for index in 0..3 {
            let mut bad = base;
            bad.estimator_factor_failures[index] = true;
            let mut capture = Compensation::new();
            capture.cycle(None, &inputs(&base, &P6, &positions), &clock).unwrap();
            capture.cycle(None, &inputs(&bad, &P6, &positions), &clock).unwrap();
            let names = ["pitch", "roll", "yaw"];
            for (other, name) in names.iter().enumerate() {
                let calculated = format!("calculated_{name}");
                if other == index {
                    if index < 2 {
                        assert_eq!(capture.sensor_values().get(name), Some(LegacyValue::Float(0.0)), "{name}");
                        assert_eq!(capture.sensor_values().get(&format!("raw_{name}")), Some(LegacyValue::Int(0)));
                        assert_eq!(capture.sensor_values().get(&calculated), Some(LegacyValue::Float(0.0)));
                    } else {
                        assert_eq!(capture.sensor_values().get(name), Some(LegacyValue::Int(0)), "{name}");
                        assert_eq!(capture.sensor_values().get(&calculated), Some(LegacyValue::Int(0)));
                    }
                } else {
                    assert_eq!(capture.sensor_values().get(name), good.sensor_values().get(name), "{name}");
                }
            }
            capture.cycle(None, &inputs(&base, &P6, &positions), &clock).unwrap();
            assert_eq!(capture.sensor(), good.sensor(), "recovery {index}");
        }
    }

    #[test]
    fn native_estimator_factor_failures_are_ignored_outside_reached_estimates() {
        let mut base = settings(CompensationKind::Virtual);
        let mut bad = base;
        bad.estimator_factor_failures = [true; 3];
        let positions = ActuatorPositions {
            corners: Corners { front_left: 741.0, front_right: 610.0, rear_right: 511.0, rear_left: 0.0 },
            motors: Some([600, 420, 700, 350, 510, 730]),
        };
        let mut expected = MotionSensor::default();
        expected.pass(TiltSource::Actuators, &inputs(&base, &OTHER_RIG, &positions), None).unwrap();
        let mut actual = MotionSensor::default();
        actual.pass(TiltSource::Actuators, &inputs(&bad, &OTHER_RIG, &positions), None).unwrap();
        assert_eq!(actual, expected, "non-six-actuator rigs do not use these factors");
        let stopped = CompensationInputs { switch_on: false, ..inputs(&bad, &P6, &positions) };
        actual.pass(TiltSource::Actuators, &stopped, None).unwrap();
        assert_eq!(actual.pitch.calculated, expected.pitch.calculated, "switch-off skips computation");
        base.kind = CompensationKind::Sensor;
        bad.kind = CompensationKind::Sensor;
        let angles = TiltSource::Sensor([12.0, 24.0, 0.0]);
        expected.pass(angles, &inputs(&base, &P6, &positions), None).unwrap();
        actual.pass(angles, &inputs(&bad, &P6, &positions), None).unwrap();
        assert_eq!(actual.pitch, expected.pitch, "WitMotion pitch uses its angle, not the factor");
        assert_eq!(actual.roll, expected.roll, "WitMotion roll uses its angle, not the factor");
        assert_eq!(actual.yaw.calculated, 0.0, "WitMotion mode still estimates six-actuator yaw");
    }

    #[test]
    fn native_literal_zero_kinds_and_idle_reset() {
        let (_, _, clock) = connected_imu();
        let s = settings(CompensationKind::Virtual);
        let mut c = Compensation::new();
        c.cycle(None, &inputs(&s, &P6, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(c.sensor_values().get("raw_pitch"), Some(LegacyValue::Int(0)));
        assert_eq!(c.sensor_values().get("calculated_pitch"), Some(LegacyValue::Float(0.0)));
        assert_eq!(c.sensor_values().get("calculated_yaw"), Some(LegacyValue::Int(0)));
        let h4 = Rig { model: RigModel::H4, dof_reality: Some(true), horizontal_surge: None };
        let positions =
            ActuatorPositions { corners: Corners { rear_right: 511.0, ..NO_POSITIONS.corners }, ..NO_POSITIONS };
        c.cycle(None, &inputs(&s, &h4, &positions), &clock).unwrap();
        assert_eq!(c.sensor_values().get("calculated_heave"), Some(LegacyValue::Int(0)));
        let reader = c.sensor_values();
        reader.set("raw_yaw", LegacyValue::Float(f64::NAN));
        let off = settings(CompensationKind::Off);
        c.cycle(None, &inputs(&off, &h4, &positions), &clock).unwrap();
        assert_eq!(reader.get("raw_yaw"), Some(LegacyValue::Int(0)));
        assert_eq!(reader.get("calculated_pitch"), None);
    }

    #[test]
    fn clamped_horizontal_surge_map_limits_the_position_before_reverse_and_scaling() {
        let settings = HorizontalSurgeSettings { stroke_mm: 100, reverse: true, factor_percent: 80 };
        let actuator = |position, max_motor_abs| HorizontalSurgeActuator {
            position: Some(LegacyValue::Float(position)),
            max_motor_abs,
        };
        assert_eq!(horizontal_surge(&actuator(8000.0, 4000), &settings).unwrap(), -40.0);
        assert_eq!(horizontal_surge(&actuator(-100.0, 4000), &settings).unwrap(), 40.0);
        assert_eq!(horizontal_surge(&actuator(0.0, 0), &settings).unwrap(), -40.0);
        assert_eq!(horizontal_surge(&actuator(f64::NAN, 4000), &settings).unwrap(), -40.0);
    }

    #[test]
    fn native_surge_calculation_commits_the_clamped_zero_range_value() {
        let (imu, _, clock) = connected_imu();
        let s = settings(CompensationKind::Sensor);
        let rig = Rig {
            horizontal_surge: Some(HorizontalSurgeActuator {
                position: Some(LegacyValue::Float(0.0)),
                max_motor_abs: 0,
            }),
            ..OTHER_RIG
        };
        let mut c = Compensation::new();
        c.sensor_values().set("calculated_surge", LegacyValue::Float(123.0));
        c.cycle(Some(&imu), &inputs(&s, &rig, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(c.sensor_values().get("calculated_surge"), Some(LegacyValue::Float(50.0)));
        assert_eq!(c.sensor_values().get("surge"), Some(LegacyValue::Float(50.0)));
    }

    /// The VR stream runs the per-axis pass on every reading: same axis values as the compensation
    /// pass, the swap lag included, no zero-total hold, no session without a sensor in 'W' mode.
    #[test]
    fn tracker_matches_the_capture_angles_pass() {
        let (imu, _, clock) = connected_imu();
        for swap in [false, true] {
            let s = CompensationSettings { swap_roll_pitch: swap, height: 700.0, ..settings(CompensationKind::Sensor) };
            let i = inputs(&s, &OTHER_RIG, &NO_POSITIONS);
            let mut c = Compensation::new();
            let mut t = PoseTracker::new();
            for _ in 0..2 {
                c.cycle(Some(&imu), &i, &clock).unwrap();
                let tracked = t.update(Some(imu.angles()), None, &i).unwrap().unwrap();
                assert_eq!(tracked.pose, c.sensor().pose(), "swap {swap}");
                assert_eq!(tracked.tilt_rates, None);
            }
        }
        let s = settings(CompensationKind::Sensor);
        let mut t = PoseTracker::new();
        let off = CompensationInputs { switch_on: false, ..inputs(&s, &OTHER_RIG, &NO_POSITIONS) };
        assert_eq!(
            t.update(Some([10.0, 0.0, 0.0]), None, &off).unwrap().unwrap().pose,
            RigPose::default(),
            "mc_switch off: zeros, not the held export"
        );
        assert_eq!(t.update(None, None, &inputs(&s, &OTHER_RIG, &NO_POSITIONS)), Ok(None));
        assert_eq!(t.sensor, MotionSensor::default());
        let v = settings(CompensationKind::Virtual);
        let corners = Corners { front_left: 200.0, front_right: 200.0, rear_left: 0.0, rear_right: 0.0 };
        let generic = ActuatorPositions { corners, motors: None };
        let tracked = t
            .update(Some([50.0, 50.0, 0.0]), Some([1.0, 1.0, 0.0]), &inputs(&v, &OTHER_RIG, &generic))
            .unwrap()
            .unwrap();
        assert_eq!(
            tracked.pose.pitch,
            corner_tilt(Tilt::Pitch, &corners, 558.0, 558.0).unwrap(),
            "'V' ignores the sensor"
        );
        assert_eq!(tracked.tilt_rates, None);
        assert_eq!(
            t.update(None, None, &inputs(&settings(CompensationKind::Off), &OTHER_RIG, &NO_POSITIONS)),
            Ok(None)
        );
    }

    /// The gyro rates follow the tilt routing: X for the pitch iteration, Y for roll, stored on the
    /// swapped axis with its reversal, 0 while the axis is not live.
    #[test]
    fn gyro_rates_follow_the_tilt_routing() {
        let s = settings(CompensationKind::Sensor); // roll reversed
        assert_eq!(tilt_rates(&s, true, [10.0, 20.0, 30.0]), TiltRates { pitch: 10.0, roll: -20.0 });
        let swapped = CompensationSettings { swap_roll_pitch: true, ..s };
        assert_eq!(tilt_rates(&swapped, true, [10.0, 20.0, 30.0]), TiltRates { pitch: 20.0, roll: -10.0 });
        assert_eq!(tilt_rates(&s, false, [10.0, 20.0, 30.0]), TiltRates::default());
        let no_pitch = CompensationSettings { pitch: AxisSettings { enabled: false, ..s.pitch }, ..s };
        assert_eq!(tilt_rates(&no_pitch, true, [10.0, 20.0, 30.0]), TiltRates { pitch: 0.0, roll: -20.0 });
        let (imu, _, _) = connected_imu();
        let mut t = PoseTracker::new();
        let tracked = t
            .update(Some(imu.angles()), Some([10.0, 20.0, 30.0]), &inputs(&s, &OTHER_RIG, &NO_POSITIONS))
            .unwrap()
            .unwrap();
        assert_eq!(tracked.tilt_rates, Some(TiltRates { pitch: 10.0, roll: -20.0 }));
    }

    /// 'V' mode without a sensor: every pass is a one-pass session (the sensor check
    /// ends it, without an extra pause), so passes run every 1 / refresh_rate; on a P6 pitch/roll are
    /// the six-actuator fits * factor / 100 (offset 0, then reversed), yaw the yaw estimate.
    #[test]
    fn virtual_mode_on_a_six_actuator_rig() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let s = CompensationSettings {
            pitch_factor: 50,
            roll_factor: 200,
            yaw_factor: 40,
            ..settings(CompensationKind::Virtual)
        };
        let motors = [600, 511, 511, 650, 511, 700];
        let positions = ActuatorPositions { motors: Some(motors), ..NO_POSITIONS };
        let m = motors.map(|v| v as f64);
        let mut c = Compensation::new();
        let period = Duration::from_secs_f64(1.0 / 90.0);
        for _ in 0..3 {
            assert_eq!(c.cycle(None, &inputs(&s, &P6, &positions), &clock), Ok(period));
        }
        let pitch = estimate_pitch(m) * 50.0 / 100.0;
        let roll = estimate_roll(m) * 200.0 / 100.0;
        assert_eq!(c.sensor().pitch, AxisReading { value: pitch, calculated: pitch, raw: pitch });
        assert_eq!(c.sensor().roll, AxisReading { value: -roll, calculated: -roll, raw: roll });
        assert_eq!(c.sensor().yaw.value, -estimate_yaw(m) * 40.0 / 100.0);
        // Without m0..m5 the estimates give 0.
        let mut c = Compensation::new();
        c.cycle(None, &inputs(&s, &P6, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(
            (c.sensor().pitch.calculated, c.sensor().roll.calculated, c.sensor().yaw.calculated),
            (0.0, 0.0, 0.0)
        );
    }

    /// 'V' mode on other rigs: DOF Reality non-P6/H6 use the seat-mover tilt (H4 tables
    /// for H4/P4), every other rig the four-corner tilt, whose zero spacing is an error.
    #[test]
    fn virtual_mode_on_four_actuator_rigs() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let s = CompensationSettings {
            roll: AxisSettings { reverse: false, ..settings(CompensationKind::Virtual).roll },
            ..settings(CompensationKind::Virtual)
        };
        let corners = Corners { front_left: 741.0, front_right: 741.0, rear_left: 0.0, rear_right: 511.0 };
        let positions = ActuatorPositions { corners, motors: None };
        let p3 = Rig { model: RigModel::Other, dof_reality: Some(true), horizontal_surge: None };
        let mut c = Compensation::new();
        c.cycle(None, &inputs(&s, &p3, &positions), &clock).unwrap();
        assert_eq!(c.sensor().pitch.calculated, table_tilt(Tilt::Pitch, 741.0, 741.0, 511.0, false));
        assert_eq!(c.sensor().roll.calculated, 6.9);
        let h4 = Rig { model: RigModel::H4, ..p3 };
        c.cycle(None, &inputs(&s, &h4, &positions), &clock).unwrap();
        assert_eq!(c.sensor().pitch.calculated, 4.5);
        let generic = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 200.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        c.cycle(None, &inputs(&s, &OTHER_RIG, &generic), &clock).unwrap();
        assert_eq!(c.sensor().pitch.calculated, corner_tilt(Tilt::Pitch, &generic.corners, 558.0, 558.0).unwrap());
        let unspaced = CompensationSettings { actuator_spacing_mm: 0, ..s };
        assert_eq!(
            c.cycle(None, &inputs(&unspaced, &OTHER_RIG, &generic), &clock),
            Err(CompensationError::ZeroActuatorSpacing)
        );
        // An unknown model: the manufacturer lookup is an error, but only once
        // an axis is computed (switch on).
        let unknown = Rig { dof_reality: None, ..OTHER_RIG };
        let off = CompensationInputs { switch_on: false, ..inputs(&s, &unknown, &generic) };
        assert!(c.cycle(None, &off, &clock).is_ok());
        assert_eq!(c.cycle(None, &inputs(&s, &unknown, &generic), &clock), Err(CompensationError::UnknownModel));
    }

    /// Heave: P6/H6 in heave mode 0 use the platform centre height * factor, H4/P4 their rear
    /// motor (512..1023 -> 0..-73, 0..510 -> 55.5..0), anything else (or another heave mode) 0.
    #[test]
    fn heave_per_rig() {
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let s = CompensationSettings {
            heave: AxisSettings { enabled: true, reverse: true, offset: 0.0 },
            ..settings(CompensationKind::Virtual)
        };
        let motors = [511, 511, 511, 1023, 511, 511];
        let positions = ActuatorPositions { motors: Some(motors), ..NO_POSITIONS };
        let mut c = Compensation::new();
        c.cycle(None, &inputs(&s, &P6, &positions), &clock).unwrap();
        assert_eq!(c.sensor().heave.value, -center_height_mm(motors).unwrap());
        let ignored = CompensationInputs { heave_from_actuators: false, ..inputs(&s, &P6, &positions) };
        c.cycle(None, &ignored, &clock).unwrap();
        assert_eq!(c.sensor().heave.value, 0.0);
        let h4 = Rig { model: RigModel::H4, dof_reality: Some(true), horizontal_surge: None };
        let seat =
            |rear_right| ActuatorPositions { corners: Corners { rear_right, ..NO_POSITIONS.corners }, motors: None };
        for (rear, heave) in [
            (1023.0, -73.0),
            (767.5, -(255.5 * 73.0 / 511.0)),
            (511.0, 0.0),
            (255.0, 55.5 - 255.0 * 55.5 / 510.0),
            (0.0, 55.5),
        ] {
            c.cycle(None, &inputs(&s, &h4, &seat(rear)), &clock).unwrap();
            assert_eq!(c.sensor().heave.value, heave, "rear_right {rear}");
        }
        c.cycle(None, &inputs(&s, &OTHER_RIG, &seat(0.0)), &clock).unwrap();
        assert_eq!(c.sensor().heave.value, 0.0);
    }

    /// Surge/sway: tan(calculated pitch or roll) * (height * factor) * axis factor /
    /// divisor (20 on P6/H6, 100 otherwise), plus the horizontal surge actuator mapped to +-stroke/2.
    #[test]
    fn surge_and_sway_from_tilts_and_surge_actuator() {
        let (imu, _, clock) = connected_imu();
        let s = CompensationSettings {
            pitch: AxisSettings { enabled: true, reverse: false, offset: -45.0 },
            roll: AxisSettings { enabled: true, reverse: false, offset: 0.0 },
            sway: AxisSettings { enabled: true, reverse: true, offset: 0.0 },
            height: 800.0,
            surge_factor: 50,
            sway_factor: 25,
            ..settings(CompensationKind::Sensor)
        };
        let mut c = Compensation::new();
        c.cycle(Some(&imu), &inputs(&s, &OTHER_RIG, &NO_POSITIONS), &clock).unwrap();
        let surge = 45f64.to_radians().tan() * (800.0 * 1.0) * 50.0 / 100.0;
        let sway = (-45f64).to_radians().tan() * -800.0 * 25.0 / 100.0;
        assert_eq!((c.sensor().surge.value, c.sensor().sway.value), (surge, sway));
        let mut c = Compensation::new();
        c.cycle(Some(&imu), &inputs(&s, &P6, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(c.sensor().surge.value, 45f64.to_radians().tan() * 800.0 * 50.0 / 20.0);
        // Horizontal surge actuator: position 3000 of 4000 in -50..50 is 25 mm, reversed, * 80 / 100.
        let hs = CompensationSettings {
            horizontal_surge: HorizontalSurgeSettings { stroke_mm: 100, reverse: true, factor_percent: 80 },
            ..s
        };
        let actuator = HorizontalSurgeActuator { max_motor_abs: 4000, position: Some(LegacyValue::Float(3000.0)) };
        let rig = Rig { horizontal_surge: Some(actuator.clone()), ..OTHER_RIG };
        let mut c = Compensation::new();
        c.cycle(Some(&imu), &inputs(&hs, &rig, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(c.sensor().surge.value, surge + -25.0 * 80.0 / 100.0);
        let no_stream =
            Rig { horizontal_surge: Some(HorizontalSurgeActuator { position: None, ..actuator.clone() }), ..OTHER_RIG };
        c.cycle(Some(&imu), &inputs(&hs, &no_stream, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(c.sensor().surge.value, surge + 0.0);
        let unscaled =
            Rig { horizontal_surge: Some(HorizontalSurgeActuator { max_motor_abs: 0, ..actuator }), ..OTHER_RIG };
        c.cycle(Some(&imu), &inputs(&hs, &unscaled, &NO_POSITIONS), &clock).unwrap();
        assert_eq!(c.sensor().surge.value, surge - 40.0);
    }

    /// CSV rows: written on every pass of P6/H6 rigs unless m1..m4 are all 511, with
    /// the exported pose and the six-actuator estimates; start_log only opens a file on P6/H6.
    #[test]
    fn csv_rows_on_six_actuator_rigs() {
        let dir = std::env::temp_dir().join(format!("openrigcue-compensation-log-{}", std::process::id()));
        let started = NaiveDateTime::parse_from_str("2026-11-09 08:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        assert!(start_log(RigModel::H4, Some(&dir), started).is_none());
        let log = start_log(RigModel::P6, Some(&dir), started).unwrap();
        let path = log.path().to_path_buf();
        let clock: Arc<dyn Clock> = Arc::new(super::super::link::fakes::ManualClock::default());
        let s = settings(CompensationKind::Virtual);
        let mut c = Compensation::new();
        c.attach_log(log);
        let idle = ActuatorPositions { motors: Some([600, 511, 511, 511, 511, 700]), ..NO_POSITIONS };
        c.cycle(None, &inputs(&s, &P6, &idle), &clock).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1, "m1..m4 centred: header only");
        let moving = ActuatorPositions { motors: Some([600, 520, 511, 511, 511, 700]), ..NO_POSITIONS };
        c.cycle(None, &inputs(&s, &P6, &moving), &clock).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(rows.len(), 1);
        let fields: Vec<&str> = rows[0].split(',').collect();
        assert_eq!(fields[1], "none");
        assert_eq!(&fields[8..14], &["600", "520", "511", "511", "511", "700"]);
        let m = [600.0, 520.0, 511.0, 511.0, 511.0, 700.0];
        assert_eq!(fields[14], super::super::pose_log::python_float_repr(estimate_roll(m)));
        assert_eq!(fields[7], super::super::pose_log::python_float_repr(c.pose().pitch));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

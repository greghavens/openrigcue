//! The accessory threads (Intellibox, SIM3D/HaptiCR, belt,
//! iFlag, motion compensation and the connection scan), fed from the service state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openrigcue_config::settings::{CompensationAxis, CompensationSensor, LedSettings, SegmentKind, Settings};
use openrigcue_config::tunes::Tune;
use openrigcue_effects::accessories::belt::{BeltCalibration, BeltMaxima, BeltTuning};
use openrigcue_effects::accessories::compensation::{
    ActuatorPositions, AxisSettings, CompensationError, CompensationInputs, CompensationKind, CompensationSettings,
    HorizontalSurgeActuator, HorizontalSurgeSettings, NativeAxisControls, NativeCompensationError,
    NativeDisplacementSettings, NativeHorizontalSurgeSettings, Rig, RigModel,
};
use openrigcue_effects::accessories::dispatcher::{
    refresh_period, Accessories, AccessoryFeed, BeltCycleInputs, BeltSlotWatchdog, DeviceSearch, IFlagCycle,
    IntelliboxInputs, PortMemory, ScanConfig, ScanEnvironment, ScanError, ScanPass, Sim3dCycleInputs, WindSwitches,
};
use openrigcue_effects::accessories::firmware::SystemAvrdude;
use openrigcue_effects::accessories::hapticr::{EffectLevel, HapticMaxima, HapticSwitches, HapticTuning};
use openrigcue_effects::accessories::iflag::{IFlagInputs, IFlagSettings};
use openrigcue_effects::accessories::imu::{InstallationReader, NativeInstallationReader};
use openrigcue_effects::accessories::intellibox::{InitParameters, NativeInitParameters};
use openrigcue_effects::accessories::led::{
    LedByteOrder, LedConfig, LedMode, NativeLedError, NativeLedSettings, NativeSegmentRanges, NativeSegmentSettings,
    SegmentConfig, SegmentType,
};
use openrigcue_effects::accessories::link::{Clock, SystemClock, SystemPorts};
use openrigcue_effects::accessories::port_scan::{AllowedPorts, Device};
use openrigcue_effects::accessories::rig_geometry::Corners;
use openrigcue_effects::accessories::sim3d::{
    BrakeEffects, PortRole, SeatEffects, Sim3dMaxima, Sim3dSwitches, Sim3dTuning,
};
use openrigcue_effects::accessories::wind::{FanMode, WindTuning};
use openrigcue_model::Telemetry;

use crate::motion::convert::py_int;
use crate::motion::{model_is_serial, MotionHandle};
use crate::runtime::{OutputMode, Shared};
use crate::state::ServiceState;

/// Motion models whose `port_mapping` holds the entry `Surge`, with that entry's index.
const SURGE_PORTS: [(&str, usize); 18] = [
    ("5 Actuators (2 Front + 2 Rear + Surge) CANBus", 4),
    ("6 Actuators (2 Front + 2 Rear + TL + Surge) CANBus", 5),
    ("6 Actuators (2 Front + 2 Rear + Surge + Belt) CANBus", 4),
    ("7 Actuators (2 Front + 2 Rear + TL + Surge + Belt) CANBus", 5),
    ("7 Actuators (2 Front + 2 Rear + 2 TL + Surge) CANBus", 5),
    ("8 Actuators (2 Front + 2 Rear + 2 TL + Surge + Belt) CANBus", 5),
    ("5 Actuators [2 Front + 2 Rear + Surge]", 4),
    ("6 Actuators [2 Front + 2 Rear + TL + Surge]", 5),
    ("6 Actuators [2 Front + 2 Rear + Surge + Belt]", 4),
    ("7 Actuators [2 Front + 2 Rear + TL + Surge + Belt]", 5),
    ("7 Actuators [2 Front + 2 Rear + 2 TL + Surge]", 5),
    ("8 Actuators [2 Front + 2 Rear + 2 TL + Surge + Belt]", 5),
    ("5 Actuators <2 Front + 2 Rear + Surge>", 4),
    ("6 Actuators <2 Front + 2 Rear + TL + Surge>", 5),
    ("6 Actuators <2 Front + 2 Rear + Surge + Belt>", 4),
    ("7 Actuators <2 Front + 2 Rear + TL + Surge + Belt>", 5),
    ("7 Actuators <2 Front + 2 Rear + 2 TL + Surge>", 6),
    ("8 Actuators <2 Front + 2 Rear + 2 TL + Surge + Belt>", 6),
];

/// File name of the pose section's Linux side (`Local\SimRacingStudioMotionRigPose`, `\` flattened), which the
/// bridge publishes inside the game's prefix.
pub const POSE_EXPORT_FILE: &str = "Local_SimRacingStudioMotionRigPose";

fn fan_mode(mode: OutputMode) -> FanMode {
    match mode {
        OutputMode::Off => FanMode::Off,
        OutputMode::Auto => FanMode::Auto,
        OutputMode::AlwaysOn => FanMode::AlwaysOn,
    }
}

fn led_mode(mode: OutputMode) -> LedMode {
    match mode {
        OutputMode::Off => LedMode::Off,
        OutputMode::Auto => LedMode::Auto,
        OutputMode::AlwaysOn => LedMode::AlwaysOn,
    }
}

/// The wind keys of the active tune.
pub fn wind_tuning(tune: &Tune) -> WindTuning {
    WindTuning {
        min_percent: tune.power_wind_min_speed_perc,
        max_percent: tune.power_wind_max_speed_perc,
        center_min_percent: tune.power_center_wind_min_speed_perc,
        center_max_percent: tune.power_center_wind_max_speed_perc,
        speed_factor: tune.speed_factor,
        curving: tune.wind_curving == 1,
        curving_effect: tune.curving_effect,
        ejection_seat: tune.enable_ejection_seat,
        native_speed_factor: None,
        native_curving: None,
        native_ejection: None,
        native_arguments: None,
        native_bounds: None,
    }
}

fn native_wind_tuning(profile: &crate::profile::ActiveProfile) -> WindTuning {
    use openrigcue_effects::accessories::wind::{NativeCurving, NativeSpeedFactor, NativeWindBounds, NativeWindError};
    use openrigcue_model::{native_value::LegacyValue, python_compare::native_equal};
    let mut tuning = wind_tuning(&profile.tune);
    let native = profile.native_tune();
    let read = |key| -> Result<LegacyValue, NativeWindError> { Ok(native_item(&native, key)?) };
    let keys = [
        "power_wind_min_speed_perc",
        "power_wind_max_speed_perc",
        "power_center_wind_min_speed_perc",
        "power_center_wind_max_speed_perc",
    ];
    tuning.native_arguments = Some(keys.into_iter().try_for_each(|key| read(key).map(|_| ())));
    tuning.native_bounds = (|| {
        Some(NativeWindBounds {
            minimum: read(keys[0]).ok()?,
            maximum: read(keys[1]).ok()?,
            center_minimum: read(keys[2]).ok()?,
            center_maximum: read(keys[3]).ok()?,
        })
    })();
    // The range map takes doubles; a failed conversion is absorbed here.
    let factor = |key| {
        read(key)
            .ok()
            .and_then(|value| value.py_as_double().ok())
            .map_or(NativeSpeedFactor::Failed, NativeSpeedFactor::Number)
    };
    tuning.native_speed_factor = Some(factor("speed_factor"));
    tuning.native_curving = Some(NativeCurving {
        enabled: read("wind_curving").map(|value| native_equal(&value, &LegacyValue::Int(1))),
        factor: factor("curving_effect"),
    });
    tuning.native_ejection = Some(read("enable_ejection_seat").and_then(|value| native_str2bool(&value)));
    tuning
}

/// The `LED` section as the LED strip reads it; `hex_colors` is the order of the named colours as
/// [`Accessories::load_hex_colors`] last loaded it, the "Always On" colour follows the current `LED.model`.
pub fn led_config(leds: &LedSettings, hex_colors: Option<LedByteOrder>) -> LedConfig {
    let segment = |i: usize| {
        let s = &leds.segments[i];
        SegmentConfig {
            start: i64::from(s.start),
            end: i64::from(s.end),
            kind: segment_type(s.kind),
            always_on: s.color,
        }
    };
    LedConfig {
        segment_count: i64::from(leds.nbr_segments),
        led_count: i64::from(leds.nbr_leds),
        brightness: i64::from(leds.brightness),
        byte_order: hex_colors,
        always_on_order: LedByteOrder::from_model(&leds.model),
        segments: [segment(0), segment(1), segment(2), segment(3)],
    }
}

fn segment_type(kind: SegmentKind) -> SegmentType {
    match kind {
        SegmentKind::Rpm => SegmentType::Rpm,
        SegmentKind::Speed => SegmentType::Speed,
        SegmentKind::Flag => SegmentType::Flag,
        SegmentKind::RpmFlag => SegmentType::RpmOrFlag,
        SegmentKind::SpeedFlag => SegmentType::SpeedOrFlag,
        SegmentKind::SpotterRight => SegmentType::SpotterRight,
        SegmentKind::SpotterLeft => SegmentType::SpotterLeft,
        SegmentKind::SpotterRightFlag => SegmentType::SpotterRightOrFlag,
        SegmentKind::SpotterLeftFlag => SegmentType::SpotterLeftOrFlag,
        SegmentKind::Off => SegmentType::Off,
    }
}

fn native_led_settings(settings: &Settings) -> NativeLedSettings {
    native_led_settings_from(&openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot()))
}

/// The segment stream's required reads of the `LED` section, as integers of any width.
fn native_led_settings_from(raw: &openrigcue_model::native_value::LegacyValue) -> NativeLedSettings {
    let field = |key: &str| -> Result<openrigcue_model::native_value::LegacyValue, NativeLedError> {
        Ok(native_path(raw, &["LED", key])?)
    };
    let integer = |key: &str| -> Result<_, NativeLedError> { Ok(native_int(&field(key)?)?) };
    NativeLedSettings {
        segment_count: integer("nbr_segments"),
        led_count: integer("nbr_leds"),
        always_on: std::array::from_fn(|index| native_led_color(raw, index)),
        ranges: native_led_ranges(raw),
        segments: std::array::from_fn(|index| {
            let prefix = format!("segment{}", index + 1);
            NativeSegmentSettings {
                start: integer(&format!("{prefix}_start")),
                end: integer(&format!("{prefix}_end")),
                // The segment type is matched by string equality; any other value matches no type.
                kind: field(&format!("{prefix}_type")).map(|value| {
                    value
                        .python_string()
                        .and_then(|text| text.scalar_string())
                        .map_or(SegmentType::Unknown, |code| SegmentType::from_code(&code))
                }),
            }
        }),
    }
}

/// The eight `segmentN_start|end` values the segment-change check compares, with the integers
/// the segment command packs.
fn native_led_ranges(raw: &openrigcue_model::native_value::LegacyValue) -> NativeSegmentRanges {
    let field = |index: usize| -> Result<openrigcue_model::native_value::LegacyValue, NativeLedError> {
        let key = format!("segment{}_{}", index / 2 + 1, if index.is_multiple_of(2) { "start" } else { "end" });
        Ok(native_path(raw, &["LED", &key])?)
    };
    NativeSegmentRanges {
        values: std::array::from_fn(field),
        positions: std::array::from_fn(|index| Ok(native_int(&field(index)?)?)),
    }
}

/// The "Always On" colour of a segment: the three integer channels of `LED.segmentN_color`
/// joined as two-digit hexadecimal fields and read back as one integer (green first for
/// GRB; channels of any width), or `None` for any other `LED.model` (`RGB` when missing).
fn native_led_color(
    raw: &openrigcue_model::native_value::LegacyValue,
    index: usize,
) -> Result<openrigcue_effects::accessories::led::ColorValue, NativeLedError> {
    use openrigcue_model::native_value::{BigInt, Key, LegacyValue};
    let section = native_item(raw, "LED")?;
    let color = native_item(&section, &format!("segment{}_color", index + 1))?.resolved();
    let channel = |index: usize| -> Result<LegacyValue, NativeLedError> {
        let (class, message) = match &color {
            LegacyValue::List(values) | LegacyValue::Tuple(values) => {
                if let Some(value) = values.get(index) {
                    return Ok(value.clone());
                }
                ("IndexError", format!("{} index out of range", color.type_name()))
            }
            LegacyValue::Str(_) | LegacyValue::Unicode(_) => {
                let text = color.python_string().expect("string value");
                if let Some(&point) = text.codepoints().get(index) {
                    let cell = openrigcue_model::native_value::PythonString::from_codepoints(vec![point])
                        .expect("one code point");
                    return Ok(LegacyValue::from_python_string(cell));
                }
                ("IndexError", "string index out of range".into())
            }
            LegacyValue::Bytes(values) => {
                if let Some(value) = values.get(index) {
                    return Ok(LegacyValue::Int(i64::from(*value)));
                }
                ("IndexError", "index out of range".into())
            }
            LegacyValue::ByteArray(_) | LegacyValue::MemoryView(_) => {
                if let Some(value) = color.buffer_bytes().and_then(|values| values.get(index).copied()) {
                    return Ok(LegacyValue::Int(i64::from(value)));
                }
                let message = if matches!(color, LegacyValue::ByteArray(_)) {
                    "bytearray index out of range"
                } else {
                    "index out of bounds on dimension 1"
                };
                ("IndexError", message.into())
            }
            LegacyValue::Dict(values) => {
                if let Some(value) = values.get_key(&Key::Int(index as i64)) {
                    return Ok(value.clone());
                }
                ("KeyError", index.to_string())
            }
            _ => ("TypeError", format!("'{}' object is not subscriptable", color.type_name())),
        };
        Err(NativeLedError { class, message })
    };
    let mut channels: Vec<BigInt> = Vec::with_capacity(3);
    for position in 0..3 {
        channels.push(native_int(&channel(position)?)?);
    }
    let model = section.resolved().as_dict().and_then(|fields| fields.get("model").map(LegacyValue::resolved));
    let is = |code: &str| {
        model.as_ref().and_then(LegacyValue::python_string).and_then(|text| text.scalar_string()).as_deref()
            == Some(code)
    };
    let grb = match model {
        None => false,
        Some(_) if is("RGB") => false,
        Some(_) if is("GRB") => true,
        Some(_) => return Ok(None),
    };
    if grb {
        channels.swap(0, 1);
    }
    // Two-digit upper-case hexadecimal of an integer: magnitude digits with a leading '-' for a negative channel.
    let digits: String = channels.iter().map(|channel| format!("{channel:02X}")).collect();
    let text = format!("0x{digits}");
    if digits.contains('-') {
        return Err(NativeLedError {
            class: "ValueError",
            message: format!(
                "invalid literal for int() with base 16: {}",
                openrigcue_model::python_repr::repr_str(&text),
            ),
        });
    }
    Ok(Some(BigInt::parse_bytes(digits.as_bytes(), 16).expect("hexadecimal digits")))
}

/// The four values the initialisation-change check compares and the initialisation converts.
fn native_intellibox_init(
    raw: &openrigcue_model::native_value::LegacyValue,
    profile: &openrigcue_model::native_value::LegacyValue,
) -> NativeInitParameters {
    let read = |root, path: &[&str]| -> Result<openrigcue_model::native_value::LegacyValue, NativeLedError> {
        Ok(native_path(root, path)?)
    };
    let values = [
        read(profile, &["gear_shift_dim_perc"]),
        read(raw, &["LED", "brightness"]),
        read(raw, &["LED", "nbr_leds"]),
        read(raw, &["INTELLIBOX", "fan_frequency"]),
    ];
    let integer =
        |index: usize| -> Result<_, NativeLedError> { Ok(native_int(values[index].as_ref().map_err(Clone::clone)?)?) };
    NativeInitParameters {
        dim_percent: values[0]
            .clone()
            .and_then(|value| value.py_as_double().map_err(|e| NativeFailure::operation(e).into())),
        brightness: integer(1),
        led_count: integer(2),
        fan_frequency: integer(3),
        values,
    }
}

fn level(enabled: bool, slider: f64) -> EffectLevel {
    EffectLevel { enabled, level: py_int(slider) }
}

/// The SIM3D keys of the active tune (`rumble_effects`, `rumble_switch`).
pub fn sim3d_tuning(tune: &Tune) -> Sim3dTuning {
    let e = &tune.rumble_effects;
    let s = &tune.rumble_switch;
    Sim3dTuning {
        throttle: SeatEffects {
            wheel_slip: level(s.throttle_wheel_slip, e.throttle_wheel_slip),
            rumble_strips: level(s.throttle_rumble_strips, e.throttle_rumble_strips),
            gear: level(s.throttle_gear, e.throttle_gear),
            engine: level(s.throttle_engine, e.throttle_engine),
            surge_acceleration: level(s.throttle_surge_acceleration, e.throttle_surge_acceleration),
            heave: level(s.throttle_heave, e.throttle_heave),
            traction_loss: level(s.throttle_traction_loss, e.throttle_traction_loss),
        },
        seat: SeatEffects {
            wheel_slip: level(s.seat_wheel_slip, e.seat_wheel_slip),
            rumble_strips: level(s.seat_rumble_strips, e.seat_rumble_strips),
            gear: level(s.seat_gear, e.seat_gear),
            engine: level(s.seat_engine, e.seat_engine),
            surge_acceleration: level(s.seat_surge_acceleration, e.seat_surge_acceleration),
            heave: level(s.seat_heave, e.seat_heave),
            traction_loss: level(s.seat_traction_loss, e.seat_traction_loss),
        },
        brake: BrakeEffects {
            wheel_lock: level(s.brake_wheel_slip, e.brake_wheel_slip),
            rumble_strips: level(s.brake_rumble_strips, e.brake_rumble_strips),
            gear: level(s.brake_gear, e.brake_gear),
            abs_lock: level(s.brake_abs_lock, e.brake_abs_lock),
            surge_deceleration: level(s.brake_surge_deceleration, e.brake_surge_deceleration),
            heave: level(s.brake_heave, e.brake_heave),
            traction_loss: level(s.brake_traction_loss, e.brake_traction_loss),
        },
        clutch_gear: level(s.clutch_gear, e.clutch_gear),
        rpm_start_rumble: py_int(e.rpm_start_rumble),
        rpm_start_fading: py_int(e.rpm_start_fading),
        rpm_stop_fading: py_int(e.rpm_stop_fading),
        wheel_slip_min_activation: py_int(e.wheel_slip_min_activation),
        wheel_lock_min_activation: py_int(e.wheel_lock_min_activation),
        traction_loss_min_activation: py_int(e.traction_loss_min_activation),
        native_engine_rpm: None,
        native_engine_control: [None, None],
        native_event_controls: None,
        native_continuous_controls: None,
        native_traction_maximum: None,
        native_map_maxima: None,
        native_profile: None,
    }
}

/// The HaptiCR keys of the active tune (`haptic_effects`, `haptic_switch`).
pub fn haptic_tuning(tune: &Tune) -> HapticTuning {
    let e = &tune.haptic_effects;
    let s = &tune.haptic_switch;
    HapticTuning {
        wheel_slip: level(s.wheel_wheel_slip, e.wheel_wheel_slip),
        abs_lock: level(s.wheel_abs_lock, e.wheel_abs_lock),
        gear: level(s.wheel_gear, e.wheel_gear),
        heave: level(s.wheel_heave, e.wheel_heave),
        rumble_strips: level(s.wheel_rumble_strips, e.wheel_rumble_strips),
        surge_acceleration: level(s.wheel_surge_acceleration, e.wheel_surge_acceleration),
        surge_deceleration: level(s.wheel_surge_deceleration, e.wheel_surge_deceleration),
        traction_loss: level(s.wheel_traction_loss, e.wheel_traction_loss),
        engine: level(s.wheel_engine, e.wheel_engine),
        rpm_start_rumble: py_int(e.rpm_start_rumble),
        rpm_start_fading: py_int(e.rpm_start_fading),
        rpm_stop_fading: py_int(e.rpm_stop_fading),
        wheel_slip_min_activation: py_int(e.wheel_slip_min_activation),
        wheel_lock_min_activation: py_int(e.wheel_lock_min_activation),
        traction_loss_min_activation: py_int(e.traction_loss_min_activation),
        native_engine_rpm: None,
        native_engine_control: None,
        native_event_controls: None,
        native_continuous_controls: None,
        native_traction_maximum: None,
        native_map_maxima: None,
        native_profile: None,
    }
}

/// SIM3D tuning for the stream: the raw profile row, which every read, integer conversion and
/// comparison uses at its own stage (selected levels, activation thresholds, rpm fade points, maxima),
/// plus the same reads carried per control (exact integers) for consumers without the row.
fn native_sim3d_tuning(profile: &crate::profile::ActiveProfile) -> Sim3dTuning {
    let mut tune = sim3d_tuning(&profile.tune);
    let raw = profile.native_tune();
    tune.native_traction_maximum = Some(native_traction_maximum(&raw));
    tune.native_map_maxima = Some(native_map_maxima(&raw));
    let control = |key: &str| native_engine_control(&raw, "rumble_switch", "rumble_effects", key);
    let continuous = |name: &str| {
        let surge = if name == "brake" { "surge_deceleration" } else { "surge_acceleration" };
        [surge, "heave", "traction_loss"].map(|suffix| control(&format!("{name}_{suffix}")))
    };
    tune.native_continuous_controls = Some(openrigcue_effects::accessories::sim3d::NativeSim3dContinuous {
        throttle: continuous("throttle"),
        brake: continuous("brake"),
        seat: continuous("seat"),
    });
    let group = |name: &str| ["wheel_slip", "rumble_strips", "gear"].map(|suffix| control(&format!("{name}_{suffix}")));
    tune.native_event_controls = Some(openrigcue_effects::accessories::sim3d::NativeSim3dEvents {
        throttle: group("throttle"),
        seat: group("seat"),
        brake: ["brake_abs_lock", "brake_wheel_slip", "brake_rumble_strips", "brake_gear"].map(control),
        clutch: control("clutch_gear"),
    });
    tune.native_engine_control = ["throttle_engine", "seat_engine"].map(|key| Some(control(key)));
    tune.native_engine_rpm = Some(native_engine_rpm(&raw, "rumble_effects"));
    tune.native_profile = Some(raw);
    tune
}

/// HaptiCR tuning for the stream, as [`native_sim3d_tuning`].
fn native_haptic_tuning(profile: &crate::profile::ActiveProfile) -> HapticTuning {
    let mut tune = haptic_tuning(&profile.tune);
    let raw = profile.native_tune();
    tune.native_traction_maximum = Some(native_traction_maximum(&raw));
    tune.native_map_maxima = Some(native_map_maxima(&raw));
    let control = |key: &str| native_engine_control(&raw, "haptic_switch", "haptic_effects", key);
    tune.native_continuous_controls = Some(
        ["wheel_surge_acceleration", "wheel_surge_deceleration", "wheel_heave", "wheel_traction_loss"].map(control),
    );
    tune.native_event_controls =
        Some(["wheel_abs_lock", "wheel_wheel_slip", "wheel_rumble_strips", "wheel_gear"].map(control));
    tune.native_engine_control = Some(control("wheel_engine"));
    tune.native_engine_rpm = Some(native_engine_rpm(&raw, "haptic_effects"));
    tune.native_profile = Some(raw);
    tune
}

/// The tune's `max_telemetry_vars` value of `key` as a double for the range map.
fn native_map_maxima(
    profile: &openrigcue_model::native_value::LegacyValue,
) -> openrigcue_effects::accessories::NativeMapMaxima {
    use openrigcue_effects::accessories::hapticr::NativeEngineError;
    let field = |key| -> Result<f64, NativeEngineError> {
        let value = native_path(profile, &["max_telemetry_vars", key])?;
        Ok(value.py_as_double().map_err(NativeFailure::operation)?)
    };
    openrigcue_effects::accessories::NativeMapMaxima {
        surge_acceleration: field("surge_acceleration"),
        surge_deceleration: field("surge_deceleration"),
        heave: field("heave"),
    }
}

/// The required integer rpm points of a tune section (`rpm_start_rumble`, `rpm_start_fading`, `rpm_stop_fading`).
fn native_engine_rpm(
    profile: &openrigcue_model::native_value::LegacyValue,
    section: &str,
) -> openrigcue_effects::accessories::hapticr::NativeEngineRpm {
    use openrigcue_effects::accessories::hapticr::NativeEngineError;
    let field = |key| -> Result<_, NativeEngineError> { Ok(native_int(&native_path(profile, &[section, key])?)?) };
    openrigcue_effects::accessories::hapticr::NativeEngineRpm {
        start: field("rpm_start_rumble"),
        fade_start: field("rpm_start_fading"),
        fade_end: field("rpm_stop_fading"),
    }
}

/// Whether the tune's switch of `key` is truthy, and its effect level as an integer.
fn native_engine_control(
    profile: &openrigcue_model::native_value::LegacyValue,
    switches: &str,
    effects: &str,
    key: &str,
) -> openrigcue_effects::accessories::hapticr::NativeEngineControl {
    use openrigcue_effects::accessories::hapticr::NativeEngineError;
    openrigcue_effects::accessories::hapticr::NativeEngineControl {
        enabled: native_path(profile, &[switches, key]).map(|value| value.is_truthy()).map_err(NativeEngineError::from),
        level: native_path(profile, &[effects, key])
            .and_then(|value| native_int(&value))
            .map_err(NativeEngineError::from),
    }
}

fn native_traction_maximum(
    profile: &openrigcue_model::native_value::LegacyValue,
) -> openrigcue_effects::accessories::NativeTractionMaximum {
    use openrigcue_effects::accessories::NativeTractionMaximum;
    match native_path(profile, &["max_telemetry_vars", "traction_loss"]) {
        Ok(value) => NativeTractionMaximum::Value(value),
        Err(_) => NativeTractionMaximum::Missing,
    }
}

/// The belt keys of the active tune.
pub fn belt_tuning(tune: &Tune) -> BeltTuning {
    BeltTuning {
        surge_enabled: tune.belt_surge_enabled,
        surge_reverse: tune.belt_surge_reverse,
        surge: tune.belt_surge,
        surge_smoothing: py_int(tune.belt_surge_smoothing),
        heave_enabled: tune.belt_heave_enabled,
        heave_factor: tune.belt_heave_factor,
        heave_smoothing: py_int(tune.belt_heave_smoothing),
        sway_enabled: tune.belt_sway_curving_enabled,
        sway_reverse: tune.belt_sway_curving_reverse,
        sway_factor: tune.belt_sway_curving_factor,
        sway_smoothing: py_int(tune.belt_sway_smoothing),
    }
}

/// The iFlag settings of the `IFLAG` section and of the tune's `iflag` entry.
pub fn iflag_settings(settings: &Settings, tune: &Tune) -> IFlagSettings {
    let flags: HashMap<String, bool> = match serde_json::to_value(&tune.iflag) {
        Ok(serde_json::Value::Object(map)) => map.into_iter().filter_map(|(k, v)| Some((k, v.as_bool()?))).collect(),
        _ => HashMap::new(),
    };
    let f = &tune.iflag;
    IFlagSettings {
        rotation: i64::from(settings.iflag.rotation_degrees),
        brightness: i64::from(settings.iflag.brightness),
        native_output: None,
        native_controls: None,
        native_flags: None,
        mirror: settings.iflag.mirror,
        gear_display: f.gear_display,
        rpm: f.rpm,
        sli: f.sli,
        spotter_left: f.spotter_left,
        spotter_right: f.spotter_right,
        flags,
    }
}

fn native_iflag_settings(settings: &Settings, profile: &crate::profile::ActiveProfile) -> IFlagSettings {
    use openrigcue_effects::accessories::hapticr::NativeEngineError;
    use openrigcue_effects::accessories::iflag::{NativeIFlagControls, NativeIFlagOutput};
    use openrigcue_model::native_value::LegacyValue;
    let mut projected = iflag_settings(settings, &profile.tune);
    let raw = LegacyValue::Dict(settings.native_snapshot());
    let value = |key| -> Result<LegacyValue, NativeEngineError> { Ok(native_path(&raw, &["IFLAG", key])?) };
    let field = |key| -> Result<_, NativeEngineError> { Ok(native_int(&value(key)?)?) };
    projected.native_output = Some(NativeIFlagOutput {
        rotation: field("rotation"),
        brightness: field("brightness"),
        mirror: value("mirror").and_then(|value| native_str2bool(&value)),
    });
    let native = profile.native_tune();
    let control = |key| -> Result<bool, NativeEngineError> { Ok(native_path(&native, &["iflag", key])?.is_truthy()) };
    projected.native_controls = Some(NativeIFlagControls {
        gear_display: control("gear_display").unwrap_or(false),
        rpm: control("rpm").unwrap_or(false),
        sli: control("sli"),
        spotter_left: control("spotter_left").unwrap_or(false),
        spotter_right: control("spotter_right").unwrap_or(false),
    });
    // The per-flag branch tests for a stored boolean false, rather than testing truthiness.
    // A failed required lookup suppresses that flag.
    projected.native_flags = Some(
        native_item(&native, "iflag")
            .ok()
            .map(|flags| flags.resolved())
            .and_then(|flags| {
                flags.as_dict().map(|flags| {
                    flags
                        .iter()
                        .filter_map(|(key, value)| match key {
                            openrigcue_model::native_value::Key::Str(name) => {
                                Some((name.clone(), !matches!(value.resolved(), LegacyValue::Bool(false))))
                            }
                            _ => None,
                        })
                        .collect()
                })
            })
            .unwrap_or_default(),
    );
    projected
}

fn axis(axis: &CompensationAxis) -> AxisSettings {
    AxisSettings { enabled: axis.enabled, reverse: axis.reverse, offset: axis.offset }
}

/// The `MOTION_COMPENSATION` section as the compensation pass reads it.
pub fn compensation_settings(settings: &Settings) -> CompensationSettings {
    compensation_settings_from_native(settings, &settings.native_snapshot())
}

fn compensation_settings_from_native(
    settings: &Settings,
    raw: &openrigcue_model::native_value::Dict,
) -> CompensationSettings {
    let c = &settings.compensation;
    // The estimator absorbs a failed required read, integer conversion or float conversion of
    // the arbitrary-width integer in its float product; the product then uses that converted factor.
    let factors = ["pitch_factor", "roll_factor", "yaw_factor"].map(|key| {
        native_compensation_field(raw, key)
            .ok()
            .and_then(|value| native_int(&value).ok())
            .and_then(|value| openrigcue_model::native_value::LegacyValue::from_integer(value).py_as_double().ok())
    });
    let typed = [c.pitch_factor, c.roll_factor, c.yaw_factor].map(py_int);
    CompensationSettings {
        kind: match c.sensor {
            CompensationSensor::Witmotion => CompensationKind::Sensor,
            CompensationSensor::Virtual => CompensationKind::Virtual,
        },
        pitch: axis(&c.pitch),
        roll: axis(&c.roll),
        sway: axis(&c.sway),
        surge: axis(&c.surge),
        yaw: axis(&c.yaw),
        heave: axis(&c.heave),
        swap_roll_pitch: c.swap_roll_pitch,
        pitch_factor: typed[0],
        roll_factor: typed[1],
        yaw_factor: typed[2],
        estimator_factor_failures: factors.map(|factor| factor.is_none()),
        native_estimator_factors: Some(std::array::from_fn(|i| factors[i].unwrap_or(typed[i] as f64))),
        surge_factor: py_int(c.surge_factor),
        sway_factor: py_int(c.sway_factor),
        height: c.height,
        actuator_spacing_mm: py_int(c.left_right_distance_mm),
        horizontal_surge: HorizontalSurgeSettings {
            stroke_mm: py_int(c.surge_horizontal_stroke),
            reverse: c.surge_horizontal_reverse,
            factor_percent: py_int(c.surge_horizontal_factor),
        },
        refresh_rate: i64::from(c.refresh_rate),
    }
}

fn native_displacement_settings(
    _settings: &Settings,
    raw: &openrigcue_model::native_value::Dict,
) -> NativeDisplacementSettings {
    NativeDisplacementSettings {
        height: native_compensation_field(raw, "height").and_then(|value| native_explicit_float(&value)),
        sway_factor: native_compensation_integer(raw, "sway_factor"),
        surge_factor: native_compensation_integer(raw, "surge_factor"),
    }
}

/// An explicit float conversion (text is parsed), resolving object handles.
fn native_explicit_float(value: &openrigcue_model::native_value::LegacyValue) -> Result<f64, NativeCompensationError> {
    openrigcue_cueing::PyValue::from(&value.resolved()).to_float().map_err(native_compensation_error)
}

fn native_compensation_field(
    raw: &openrigcue_model::native_value::Dict,
    key: &str,
) -> Result<openrigcue_model::native_value::LegacyValue, NativeCompensationError> {
    let root = openrigcue_model::native_value::LegacyValue::Dict(raw.clone());
    Ok(native_path(&root, &["MOTION_COMPENSATION", key])?)
}

fn native_horizontal_surge_settings(
    _settings: &Settings,
    raw: &openrigcue_model::native_value::Dict,
) -> NativeHorizontalSurgeSettings {
    NativeHorizontalSurgeSettings {
        stroke_mm: native_compensation_integer(raw, "surge_horizontal_stroke"),
        reverse: native_compensation_field(raw, "surge_horizontal_reverse").and_then(|value| native_str2bool(&value)),
        factor_percent: native_compensation_integer(raw, "surge_horizontal_factor"),
    }
}

fn native_str2bool<E: From<NativeFailure>>(value: &openrigcue_model::native_value::LegacyValue) -> Result<bool, E> {
    openrigcue_cueing::str2bool(&openrigcue_cueing::PyValue::from(value))
        .map_err(|error| NativeFailure { class: error.exception().name(), message: error.message().into() }.into())
}

fn native_axis_enabled(raw: &openrigcue_model::native_value::Dict) -> [Result<bool, NativeCompensationError>; 6] {
    ["pitch", "roll", "sway", "surge", "yaw", "heave"]
        .map(|axis| native_str2bool(&native_compensation_axis_field(raw, axis, "enabled")?))
}

fn native_compensation_axis_field(
    raw: &openrigcue_model::native_value::Dict,
    axis: &str,
    key: &str,
) -> Result<openrigcue_model::native_value::LegacyValue, NativeCompensationError> {
    Ok(native_item(&native_compensation_field(raw, axis)?, key)?)
}

fn native_axis_controls(_settings: &Settings, raw: &openrigcue_model::native_value::Dict) -> NativeAxisControls {
    NativeAxisControls {
        reverse: ["pitch", "roll", "sway", "surge", "yaw", "heave"]
            .map(|axis| native_str2bool(&native_compensation_axis_field(raw, axis, "reverse")?)),
        swap_roll_pitch: native_compensation_field(raw, "swap_roll_pitch").and_then(|value| native_str2bool(&value)),
        offsets: ["pitch", "roll"].map(|axis| {
            native_compensation_axis_field(raw, axis, "offset").and_then(|value| native_explicit_float(&value))
        }),
        actuator_spacing_mm: native_compensation_integer(raw, "mm_distance_left_right"),
    }
}

fn native_compensation_kind(
    raw: &openrigcue_model::native_value::Dict,
) -> Result<CompensationKind, NativeCompensationError> {
    let value = native_compensation_field(raw, "type")?;
    let text = value.python_string();
    let is = |code: &str| text.as_ref().is_some_and(|text| text.scalar_string().as_deref() == Some(code));
    Ok(if is("W") {
        CompensationKind::Sensor
    } else if is("V") {
        CompensationKind::Virtual
    } else {
        CompensationKind::Off
    })
}

fn native_compensation_error(error: openrigcue_cueing::CueError) -> NativeCompensationError {
    NativeCompensationError { class: error.exception().name(), message: error.message().into() }
}

/// A required `MOTION_COMPENSATION` integer, of arbitrary width.
fn native_compensation_integer(
    raw: &openrigcue_model::native_value::Dict,
    key: &str,
) -> Result<openrigcue_model::native_value::BigInt, NativeCompensationError> {
    Ok(native_int(&native_compensation_field(raw, key)?)?)
}

/// One failed value operation: the error class name and message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeFailure {
    class: &'static str,
    message: String,
}

impl NativeFailure {
    /// A `Class: message` error of the shared value operations.
    fn operation(error: String) -> Self {
        let (class, message) = openrigcue_effects::accessories::values::split_exception(&error);
        NativeFailure { class, message }
    }
}

macro_rules! native_failure_into {
    ($($target:ty),*) => {$(
        impl From<NativeFailure> for $target {
            fn from(failure: NativeFailure) -> Self {
                Self { class: failure.class, message: failure.message }
            }
        }
    )*};
}
native_failure_into!(
    NativeCompensationError,
    NativeLedError,
    openrigcue_effects::accessories::hapticr::NativeEngineError,
    openrigcue_effects::accessories::wind::NativeWindError
);

/// A required lookup of `key` in `parent`, resolving object handles (a dictionary alias is not a
/// rejection): a missing key of a dictionary is a key failure, a lookup in any other value a type failure.
fn native_item(
    parent: &openrigcue_model::native_value::LegacyValue,
    key: &str,
) -> Result<openrigcue_model::native_value::LegacyValue, NativeFailure> {
    parent.item_owned(key).map_err(|message| NativeFailure {
        class: if parent.resolved().as_dict().is_some() { "KeyError" } else { "TypeError" },
        message,
    })
}

/// Successive required lookups along `path`, starting at `root`.
fn native_path(
    root: &openrigcue_model::native_value::LegacyValue,
    path: &[&str],
) -> Result<openrigcue_model::native_value::LegacyValue, NativeFailure> {
    path.iter().try_fold(root.clone(), |parent, key| native_item(&parent, key))
}

/// The value as an integer: the arbitrary-precision result, or the conversion's error.
fn native_int(
    value: &openrigcue_model::native_value::LegacyValue,
) -> Result<openrigcue_model::native_value::BigInt, NativeFailure> {
    value.py_integer().map_err(NativeFailure::operation)
}

/// `MOTION_COMPENSATION.installation`, read from the configuration whenever it is called.
fn installation_reader(state: &ServiceState) -> InstallationReader {
    let state = state.clone();
    Arc::new(move || state.config.read().settings().compensation.installation.clone())
}

fn native_installation_reader(state: &ServiceState) -> NativeInstallationReader {
    let state = state.clone();
    Arc::new(move || native_compensation_field(&state.config.read().settings().native_snapshot(), "installation"))
}

/// The pause of a refresh loop, evaluated at the sleep, after the cycle:
/// the section's integer `refresh_rate` of any width, then `1.0 / refresh` (the Windows rules apply no
/// cap); `None` where that fails (a failed read or conversion, a zero, negative or overflowing rate),
/// which ends the refresh thread.
fn native_refresh_period(state: &ServiceState, hardware: &str) -> Option<Duration> {
    use openrigcue_model::native_value::LegacyValue;
    let raw = LegacyValue::Dict(state.config.read().settings().native_snapshot());
    let rate = native_int(&native_path(&raw, &[hardware, "refresh_rate"]).ok()?).ok()?;
    let seconds = LegacyValue::Float(1.0).py_truediv(&LegacyValue::from_integer(rate)).and_then(|v| v.py_as_double());
    openrigcue_model::python_time::sleep_duration(seconds.ok()?).ok()
}

/// One telemetry value: the stored value, or its typed projection; `None` when the key is missing.
fn native_channel(telemetry: &Telemetry, key: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
    telemetry
        .native_value(key)
        .cloned()
        .or_else(|| telemetry.get_assigned(key).map(openrigcue_model::native_value::LegacyValue::Float))
}

/// Whether a game listener runs, as the SIM3D and HaptiCR streams test it: a listener other than none
/// (`None` here), the emulator and the shaker test's `test`.
fn game_listener_running(listener: Option<&str>) -> bool {
    listener.is_some_and(|l| l != "emulator" && l != "test")
}

/// The rig as the compensation pass sees the selected motion model; `stream` is the published motion stream.
pub fn compensation_rig(model: &str, stream: Option<&[i64]>) -> Rig {
    compensation_rig_native(model, None, stream)
}

/// [`compensation_rig`] reading the published motor list itself (any
/// width or in-place driver edit), the projection only when no such list was published.
fn compensation_rig_native(
    model: &str,
    native: Option<&openrigcue_model::native_value::LegacyValue>,
    stream: Option<&[i64]>,
) -> Rig {
    let spec = openrigcue_devices::motion::motion_model(model, false);
    let horizontal_surge =
        SURGE_PORTS.iter().find(|(name, _)| *name == model).map(|&(_, index)| HorizontalSurgeActuator {
            max_motor_abs: spec.map_or(0, |s| s.max_motor_abs),
            position: match native {
                Some(list) => list.get_index(index).ok(),
                None => stream.and_then(|s| s.get(index)).map(|&p| openrigcue_model::native_value::LegacyValue::Int(p)),
            },
        });
    Rig {
        model: RigModel::from_model(model),
        dof_reality: spec.map(|s| s.manufacturer == "DOF REALITY"),
        horizontal_surge,
    }
}

/// The actuator positions as compensation reads them (zeros by default).
fn compensation_actuators(
    travel: Option<&openrigcue_devices::motion::driver_support::CornerTravel>,
    published: Option<&openrigcue_devices::motion::ActuatorPositions>,
) -> ActuatorPositions {
    let corners = travel.map_or_else(Corners::default, |t| Corners {
        front_left: t.front_left,
        front_right: t.front_right,
        rear_left: t.rear_left,
        rear_right: t.rear_right,
    });
    ActuatorPositions { corners, motors: published.map(|p| p.motors) }
}

/// The older cache file, read only to fill rows the cache table lacks.
pub const PORT_CACHE_FILE: &str = "ports.toml";

/// The port cache (the autocommitted `cache` table of `metadata.db`), shared by accessory scans and Thanos.
#[derive(Clone)]
pub struct PortCache {
    inner: Arc<Mutex<CacheFile>>,
}

struct CacheFile {
    path: Option<PathBuf>,
    store: CacheStore,
}

enum CacheStore {
    Memory(openrigcue_model::native_value::Dict),
    Native(Arc<openrigcue_config::import::sqlitedict::AutocommitTable>),
}

fn device_key(device: Device) -> &'static str {
    match device {
        Device::Intellibox => "intellibox",
        Device::Belt => "belt",
        Device::HaptiCr => "hapticr",
        Device::Sim3d => "sim3d",
        Device::IFlag => "iflag",
        Device::MotionCompensation => "mc",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PortCacheError {
    #[error(transparent)]
    Database(#[from] openrigcue_config::import::sqlitedict::DbError),
    #[error("cached com_port is not a dictionary")]
    InvalidPorts,
}

impl CacheFile {
    fn get(&self, key: &str) -> Result<Option<openrigcue_model::native_value::LegacyValue>, PortCacheError> {
        match &self.store {
            CacheStore::Memory(table) => Ok(table.get(key).cloned()),
            CacheStore::Native(table) => Ok(table.get(key)?),
        }
    }

    fn contains(&self, key: &str) -> Result<bool, PortCacheError> {
        match &self.store {
            CacheStore::Memory(table) => Ok(table.contains(key)),
            CacheStore::Native(table) => Ok(table.contains(key)?),
        }
    }

    fn set(&mut self, key: &str, value: openrigcue_model::native_value::LegacyValue) -> Result<(), PortCacheError> {
        match &mut self.store {
            CacheStore::Memory(table) => table.insert(key, value),
            CacheStore::Native(table) => table.set(key, &value)?,
        }
        Ok(())
    }

    fn set_com_port(&mut self, key: &str, port: &str) -> Result<(), PortCacheError> {
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let mut ports = match self.get("com_port")? {
            None => Dict::new(),
            Some(LegacyValue::Dict(ports)) => ports,
            Some(_) => return Err(PortCacheError::InvalidPorts),
        };
        ports.insert(key, LegacyValue::Str(port.to_owned()));
        self.set("com_port", LegacyValue::Dict(ports))
    }
}

impl PortCache {
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheFile {
                path: None,
                store: CacheStore::Memory(openrigcue_model::native_value::Dict::new()),
            })),
        }
    }

    /// Opens the cache table without decoding unrelated rows. The older ports.toml is a migration
    /// input only: each absent row is imported once; existing table values always win.
    pub fn open(path: &Path, legacy: Option<&Path>) -> Result<Self, PortCacheError> {
        use openrigcue_config::import::sqlitedict::AutocommitTable;
        Self::from_open_table(path, Arc::new(AutocommitTable::open(path, "cache")?), legacy)
    }

    pub(crate) fn from_open_table(
        path: &Path,
        table: Arc<openrigcue_config::import::sqlitedict::AutocommitTable>,
        legacy: Option<&Path>,
    ) -> Result<Self, PortCacheError> {
        use openrigcue_model::native_value::{Key, LegacyValue};
        let mut file = CacheFile { path: Some(path.to_owned()), store: CacheStore::Native(table) };
        if let Some(legacy) = legacy.filter(|path| path.exists()) {
            match std::fs::read_to_string(legacy)
                .ok()
                .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
                .and_then(|value| serde_json::to_value(value).ok())
                .map(LegacyValue::from)
            {
                Some(LegacyValue::Dict(table)) => {
                    for (key, value) in table.iter() {
                        if let Key::Str(key) = key {
                            if !file.contains(key)? {
                                file.set(key, value.clone())?;
                            }
                        }
                    }
                }
                _ => tracing::warn!(path = %legacy.display(), "cannot read the older Rust port cache"),
            }
        }
        Ok(Self { inner: Arc::new(Mutex::new(file)) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheFile> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Launcher close keeps the table handle, with its worker closed.
    pub(crate) fn close(&self) -> Result<(), PortCacheError> {
        if let CacheStore::Native(table) = &self.lock().store {
            table.close()?;
        }
        Ok(())
    }

    pub fn documents(&self) -> Option<PathBuf> {
        self.lock().path.as_deref().and_then(Path::parent).map(Path::to_path_buf)
    }

    fn read_port(&self, row: &str, key: &str) -> Option<String> {
        use openrigcue_model::native_value::LegacyValue;
        match self.lock().get(row) {
            Ok(Some(LegacyValue::Dict(table))) => match table.get(key) {
                Some(LegacyValue::Str(port)) => Some(port.clone()),
                _ => None,
            },
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, "cannot read the port cache");
                None
            }
        }
    }
}

impl PortMemory for PortCache {
    fn recall(&self, device: Device) -> Option<String> {
        match device {
            Device::Intellibox => self.read_port("intellibox", "com_port"),
            device => self.read_port("com_port", device_key(device)),
        }
    }

    fn remember(&mut self, device: Device, port: &str) {
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let mut file = self.lock();
        let result = match device {
            Device::Intellibox => {
                let mut row = Dict::new();
                row.insert("com_port", LegacyValue::Str(port.to_owned()));
                file.set("intellibox", LegacyValue::Dict(row))
            }
            device => file.set_com_port(device_key(device), port),
        };
        if let Err(error) = result {
            tracing::warn!(%error, "cannot store the port cache");
        }
    }
}

impl openrigcue_devices::motion::thanos::ComPortCache for PortCache {
    fn get(&self, key: &str) -> Option<String> {
        self.read_port("com_port", key)
    }
    fn set(&self, key: &str, port: &str) {
        if let Err(error) = self.lock().set_com_port(key, port) {
            tracing::warn!(%error, "cannot store the port cache");
        }
    }
    fn python_str(&self) -> String {
        self.lock().get("com_port").ok().flatten().map_or_else(|| "{}".into(), |value| cache_repr(&value))
    }
}

fn cache_repr(value: &openrigcue_model::native_value::LegacyValue) -> String {
    use openrigcue_devices::motion::python::{bytes_repr, str_repr};
    use openrigcue_model::native_value::{Key, LegacyValue};
    match value {
        LegacyValue::Str(value) => str_repr(value),
        LegacyValue::Bytes(value) => bytes_repr(value),
        LegacyValue::Dict(table) => format!(
            "{{{}}}",
            table
                .iter()
                .map(|(key, value)| {
                    let key = match key {
                        Key::Str(key) => str_repr(key),
                        Key::Int(key) => key.to_string(),
                        Key::Bool(_) | Key::Float(_) => key.to_text(),
                        Key::None => "None".into(),
                        Key::Extra(_) => openrigcue_model::python_repr::repr(&key.to_value()),
                    };
                    format!("{key}: {}", cache_repr(value))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        LegacyValue::List(items) => format!("[{}]", items.iter().map(cache_repr).collect::<Vec<_>>().join(", ")),
        LegacyValue::Tuple(items) => format!(
            "({}{})",
            items.iter().map(cache_repr).collect::<Vec<_>>().join(", "),
            if items.len() == 1 { "," } else { "" }
        ),
        LegacyValue::Set(items) if items.is_empty() => "set()".into(),
        LegacyValue::Set(items) => format!("{{{}}}", items.iter().map(cache_repr).collect::<Vec<_>>().join(", ")),
        value => value.to_string(),
    }
}

/// What the accessory threads read from the service each cycle.
pub struct ServiceFeed {
    state: ServiceState,
    motion: MotionHandle,
    accessories: Arc<Accessories>,
    memory: Mutex<PortCache>,
    /// Where the pose file for the bridge is written (`/dev/shm/openrigcue`), `None` to not write it.
    pose_dir: Option<PathBuf>,
    /// The VR compensation settings; their SRS-export switch gates the bridge pose file.
    vr: Option<crate::compensation::VrCompensation>,
    /// `MOTION_COMPENSATION.installation`, for the compensation pass and the sensor reader.
    installation: InstallationReader,
    native_installation: NativeInstallationReader,
    /// The translation into the selected UI language; English (the key) without it.
    ui: Option<Arc<crate::api::ui_state::UiServices>>,
}

impl ServiceFeed {
    pub fn new(
        state: ServiceState,
        motion: MotionHandle,
        accessories: Arc<Accessories>,
        memory: PortCache,
        pose_dir: Option<PathBuf>,
    ) -> ServiceFeed {
        // The named colours are loaded once at start-up, with the loaded
        // configuration.
        accessories.load_hex_colors(&state.config.read().settings().leds.model);
        let installation = installation_reader(&state);
        let native_installation = native_installation_reader(&state);
        ServiceFeed {
            state,
            motion,
            accessories,
            memory: Mutex::new(memory),
            pose_dir,
            vr: None,
            installation,
            native_installation,
            ui: None,
        }
    }

    /// Translates the texts the scan publishes (the Intellibox flashing failure) into the UI language.
    pub fn with_ui(mut self, ui: Arc<crate::api::ui_state::UiServices>) -> ServiceFeed {
        self.ui = Some(ui);
        self
    }

    fn translate(&self, text: &str) -> String {
        self.ui.as_ref().map_or_else(|| text.to_owned(), |ui| ui.translate(text))
    }

    /// Writes the bridge pose file only while the VR settings' SRS export is on.
    pub fn with_vr(mut self, vr: crate::compensation::VrCompensation) -> ServiceFeed {
        self.vr = Some(vr);
        self
    }

    /// The inputs of one compensation pass (from the configuration, the runtime state and
    /// the model table), shared by the sensor pass and the VR pose stream.
    pub fn with_compensation_inputs<R>(&self, work: impl FnOnce(&CompensationInputs<'_>) -> R) -> R {
        let switch_on = self.state.controls.read().switches.compensation;
        let game = self.state.session.read().game_key().to_owned();
        let motion = self.state.motion.snapshot();
        let (
            settings,
            native_kind,
            native_displacement,
            native_refresh_rate,
            native_horizontal_surge,
            native_axis_enabled,
            native_axis_controls,
            model,
        ) = {
            let config = self.state.config.read();
            let raw = config.settings().native_snapshot();
            (
                compensation_settings_from_native(config.settings(), &raw),
                native_compensation_kind(&raw),
                native_displacement_settings(config.settings(), &raw),
                native_compensation_integer(&raw, "refresh_rate"),
                native_horizontal_surge_settings(config.settings(), &raw),
                native_axis_enabled(&raw),
                native_axis_controls(config.settings(), &raw),
                config.settings().motion.model.clone(),
            )
        };
        let rig = compensation_rig_native(
            &model,
            motion.native_stream.as_ref(),
            motion.stream.as_ref().map(|s| s.as_slice()),
        );
        let actuators = compensation_actuators(motion.travel.as_ref(), motion.actuators.as_ref());
        let inputs = CompensationInputs {
            settings: &settings,
            native_kind: Some(&native_kind),
            native_displacement: Some(&native_displacement),
            native_refresh_rate: Some(&native_refresh_rate),
            native_horizontal_surge: Some(&native_horizontal_surge),
            native_axis_enabled: Some(&native_axis_enabled),
            native_axis_controls: Some(&native_axis_controls),
            switch_on,
            rig: &rig,
            actuators: &actuators,
            // Whether the published heave value equals 0.
            heave_from_actuators: motion.native_heave_cue.as_ref().map_or(motion.heave_cue == 0.0, |heave| {
                openrigcue_model::python_compare::native_equal(
                    heave,
                    &openrigcue_model::native_value::LegacyValue::Int(0),
                )
            }),
            game_running: Some(&game),
            installation: &self.installation,
            native_installation: Some(&self.native_installation),
        };
        work(&inputs)
    }

    fn wind_switches(&self) -> WindSwitches {
        let (switches, canned) = {
            let controls = self.state.controls.read();
            (controls.switches, controls.canned)
        };
        let session = self.state.session.read();
        WindSwitches {
            left_right: fan_mode(switches.wind),
            center: fan_mode(switches.wind_center),
            dual_max_steering_percent: session.wind_steering_angle.map_or(0.0, f64::from),
            // The ejection-seat canned effect, held by its hotkey.
            ejection_requested: canned.ejection_seat == Some(1),
            lla: session.fan_override,
        }
    }

    fn period(&self, rate: u32) -> Option<Duration> {
        refresh_period(i64::from(rate))
    }

    /// The accessory ports as the motion scan's port arbitration sees them.
    fn claimed_for_motion(&self) -> openrigcue_devices::discovery::ClaimedPorts {
        let claimed = self.accessories.claimed_ports(&[]);
        let held = |device: Device| Some(claimed.of(device).to_owned()).filter(|p| !p.is_empty());
        openrigcue_devices::discovery::ClaimedPorts {
            motion_compensation: held(Device::MotionCompensation),
            belt: held(Device::Belt),
            hapticr: held(Device::HaptiCr),
            intellibox: held(Device::Intellibox),
            iflag: held(Device::IFlag),
            sim3d: held(Device::Sim3d),
        }
    }

    /// The switches the belt slot shares with the UI (`belt_switch`, `belt_test_mode`, `belt_test_value`).
    fn sync_belt_switches(&self) {
        let controls = self.state.controls.snapshot();
        let mut slot = self.accessories.belt.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.switches.on = controls.switches.belt;
        slot.switches.test_mode = controls.tests.belt_test;
        slot.assign_test_value(controls.belt_test_value(), controls.tests.belt_test_value);
    }

    fn write_pose(&self) {
        let Some(dir) = &self.pose_dir else { return };
        let pose = self.accessories.imu.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pose();
        if let Err(error) = write_in_place(&dir.join(POSE_EXPORT_FILE), &pose.to_bytes()) {
            tracing::debug!(%error, "cannot write the rig pose file");
        }
    }
}

/// Writes `bytes` from offset 0 into the file at `path`, keeping the file (and its inode) for readers.
fn write_in_place(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)
}

impl AccessoryFeed for ServiceFeed {
    fn intellibox(
        &self,
        cycle: &mut dyn FnMut(&IntelliboxInputs<'_>, Option<Duration>) -> Option<Duration>,
    ) -> Option<Duration> {
        // The initial `spotter_right` lookup waits 0.5 s on a missing key.
        if !*self.state.telemetry_ready.read() {
            return Some(Duration::from_millis(500));
        }
        let telemetry: Telemetry = self.state.telemetry.snapshot();
        let switches = self.state.controls.read().switches;
        let shift_light = f64::from(u8::from(self.state.session.read().shift_light));
        let wind = self.wind_switches();
        let (tuning, init, native_init, led, native_led, period) = {
            let config = self.state.config.read();
            let settings = config.settings();
            let profile = self.state.profile.read();
            let init = InitParameters {
                gear_shift_dim_percent: profile.tune.gear_shift_dim_perc,
                led_brightness: i64::from(settings.leds.brightness),
                led_count: i64::from(settings.leds.nbr_leds),
                fan_frequency: i64::from(settings.wind.fan_frequency),
            };
            let led = led_config(&settings.leds, self.accessories.hex_colors());
            (
                native_wind_tuning(&profile),
                init,
                native_intellibox_init(
                    &openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot()),
                    &profile.native_tune(),
                ),
                led,
                native_led_settings(settings),
                self.period(settings.wind.refresh_rate),
            )
        };
        // The helper's reads after a device command see the configuration and profile as they are then.
        let reread = || {
            let raw = openrigcue_model::native_value::LegacyValue::Dict(
                self.state.config.read().settings().native_snapshot(),
            );
            let profile = self.state.profile.read().native_tune();
            openrigcue_effects::accessories::intellibox::NativeHelperConfig {
                ranges: native_led_ranges(&raw),
                init: native_intellibox_init(&raw, &profile),
            }
        };
        let live_period = || native_refresh_period(&self.state, "INTELLIBOX");
        let inputs = IntelliboxInputs {
            telemetry: &telemetry,
            shift_light,
            wind,
            tuning: &tuning,
            led: &led,
            native_led: Some(&native_led),
            led_mode: led_mode(switches.led),
            tach_test: switches.tach == OutputMode::AlwaysOn,
            init,
            native_init: Some(&native_init),
            native_reread: Some(&reread),
            live_period: Some(&live_period),
        };
        cycle(&inputs, period)
    }

    fn sim3d(&self, cycle: &mut dyn FnMut(&Sim3dCycleInputs<'_>) -> Option<Duration>) -> Option<Duration> {
        let telemetry: Telemetry = self.state.telemetry.snapshot();
        let controls = self.state.controls.snapshot();
        let game_running = game_listener_running(self.state.session.read().listener.as_deref());
        let wind = self.wind_switches();
        let port = |label: &str| PortRole::from_label(label).unwrap_or(PortRole::None);
        // Everything is copied out before the cycle talks to the devices, so no configuration lock is held then.
        let (sim3d, hapticr_enabled, tuning, haptic, wind_tuning, sim3d_maxima, haptic_maxima) = {
            let config = self.state.config.read();
            let profile = self.state.profile.read();
            let ranges = &profile.tune.max_telemetry_vars;
            (
                config.settings().sim3d.clone(),
                enable_hapticr(config.settings()),
                native_sim3d_tuning(&profile),
                native_haptic_tuning(&profile),
                native_wind_tuning(&profile),
                Sim3dMaxima {
                    surge_acceleration: ranges.surge_acceleration,
                    heave: ranges.heave,
                    traction_loss: ranges.traction_loss,
                },
                HapticMaxima {
                    surge_acceleration: ranges.surge_acceleration,
                    surge_deceleration: ranges.surge_deceleration,
                    heave: ranges.heave,
                    traction_loss: ranges.traction_loss,
                },
            )
        };
        let haptic_reread = || {
            let telemetry: Telemetry = self.state.telemetry.snapshot();
            let controls = self.state.controls.snapshot();
            let game_running = game_listener_running(self.state.session.read().listener.as_deref());
            let config = self.state.config.read();
            let profile = self.state.profile.read();
            let ranges = &profile.tune.max_telemetry_vars;
            openrigcue_effects::accessories::dispatcher::HapticStage {
                telemetry,
                enabled: enable_hapticr(config.settings()),
                switches: HapticSwitches {
                    test_mode: controls.tests.haptic_test,
                    on: controls.switches.haptic,
                    game_running,
                },
                tuning: native_haptic_tuning(&profile),
                maxima: HapticMaxima {
                    surge_acceleration: ranges.surge_acceleration,
                    surge_deceleration: ranges.surge_deceleration,
                    heave: ranges.heave,
                    traction_loss: ranges.traction_loss,
                },
            }
        };
        let live_period = || native_refresh_period(&self.state, "SIM3D");
        let inputs = Sim3dCycleInputs {
            telemetry: &telemetry,
            wind,
            wind_tuning: &wind_tuning,
            sim3d_model_set: sim3d.model != "None",
            hapticr_enabled,
            ports: [port(&sim3d.port1), port(&sim3d.port2), port(&sim3d.port3), port(&sim3d.port4)],
            switches: Sim3dSwitches {
                test_port: controls.tests.rumble_test,
                on: controls.switches.sim3d,
                game_running,
            },
            tuning: &tuning,
            maxima: &sim3d_maxima,
            haptic_switches: HapticSwitches {
                test_mode: controls.tests.haptic_test,
                on: controls.switches.haptic,
                game_running,
            },
            haptic_tuning: &haptic,
            haptic_maxima: &haptic_maxima,
            haptic_reread: Some(&haptic_reread),
            period: self.period(sim3d.refresh_rate),
            live_period: Some(&live_period),
        };
        cycle(&inputs)
    }

    fn belt(&self, cycle: &mut dyn FnMut(&BeltCycleInputs<'_>) -> Option<Duration>) -> Option<Duration> {
        self.sync_belt_switches();
        let telemetry: Telemetry = self.state.telemetry.snapshot();
        let (tuning, maxima, calibration, period, native_profile, native_settings) = {
            let config = self.state.config.read();
            let settings = config.settings();
            let profile = self.state.profile.read();
            let ranges = &profile.tune.max_telemetry_vars;
            (
                belt_tuning(&profile.tune),
                BeltMaxima {
                    surge_acceleration: ranges.surge_acceleration,
                    surge_deceleration: ranges.surge_deceleration,
                    heave: ranges.heave,
                    sway: ranges.sway,
                },
                BeltCalibration {
                    power: py_int(settings.belt.calibration_factor),
                    retraction: py_int(settings.belt.calibration_retraction),
                },
                self.period(settings.belt.refresh_rate),
                profile.native_tune(),
                openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot()),
            )
        };
        let live_period = || native_refresh_period(&self.state, "BELT_TENSIONER");
        let inputs = BeltCycleInputs {
            telemetry: &telemetry,
            tuning: &tuning,
            maxima: &maxima,
            calibration: &calibration,
            native_profile: Some(&native_profile),
            native_settings: Some(&native_settings),
            period,
            live_period: Some(&live_period),
        };
        let was_on = self.state.controls.snapshot().switches.belt;
        let pause = cycle(&inputs);
        // A failed calibration switches the belt and its test off for everyone, not only for the driver.
        let still_on = self.accessories.belt.lock().unwrap_or_else(std::sync::PoisonError::into_inner).switches.on;
        if was_on && !still_on {
            let mut controls = self.state.controls.write();
            controls.switches.belt = false;
            controls.tests.belt_test = false;
        }
        pause
    }

    fn iflag(&self, cycle: &mut IFlagCycle<'_>) -> Option<Duration> {
        let telemetry: Telemetry = self.state.telemetry.snapshot();
        let controls = self.state.controls.snapshot();
        let shift_light = f64::from(u8::from(self.state.session.read().shift_light));
        let settings = {
            let config = self.state.config.read();
            let profile = self.state.profile.read();
            native_iflag_settings(config.settings(), &profile)
        };
        let inputs = IFlagInputs {
            gear: telemetry.get_assigned("gear").unwrap_or(11.0),
            native_gear: telemetry.native_value("gear").cloned(),
            rpm: telemetry.get_assigned("rpm").unwrap_or(0.0),
            max_rpm: telemetry.get_assigned("max_rpm").unwrap_or(0.0),
            flag: telemetry.get_assigned("flag").unwrap_or(0.0),
            spotter_left: telemetry.get_assigned("spotter_left").unwrap_or(0.0),
            spotter_right: telemetry.get_assigned("spotter_right").unwrap_or(0.0),
            shift_light,
            test: controls.tests.iflag_test,
            on: controls.switches.iflag,
            native: Some(openrigcue_effects::accessories::iflag::NativeIFlagTelemetry {
                rpm: native_channel(&telemetry, "rpm"),
                max_rpm: native_channel(&telemetry, "max_rpm"),
                flag: native_channel(&telemetry, "flag"),
                spotter_left: native_channel(&telemetry, "spotter_left"),
                spotter_right: native_channel(&telemetry, "spotter_right"),
            }),
        };
        let reread = || {
            let config = self.state.config.read();
            let profile = self.state.profile.read();
            native_iflag_settings(config.settings(), &profile)
        };
        let live_period = || native_refresh_period(&self.state, "IFLAG");
        cycle(&settings, &inputs, &live_period, Some(&reread))
    }

    fn compensation(
        &self,
        cycle: &mut dyn FnMut(&CompensationInputs<'_>) -> Result<Duration, CompensationError>,
    ) -> Result<Duration, CompensationError> {
        let result = self.with_compensation_inputs(|inputs| cycle(inputs));
        if self.vr.as_ref().is_none_or(|vr| vr.settings().srs_export) {
            self.write_pose();
        }
        result
    }

    fn serial_event_wait(&self) -> Option<Duration> {
        let model = self.state.config.read().settings().motion.model.clone();
        openrigcue_effects::accessories::dispatcher::serial_event_wait_timeout(model_is_serial(&model))
    }

    /// One connection scan pass after its wait: step 2 (the motion connect), the accessory steps 3-6, then
    /// step 7 (the motion port check, after the accessory scans and their port-loss check).
    fn motion_connect(&self) {
        let claimed = self.claimed_for_motion();
        self.motion.connect_step(&self.state, claimed);
    }

    fn scan(&self, pass: &mut ScanPass<'_>) -> Result<(), ScanError> {
        self.motion_connect();
        let motion_ports = self.motion.ports();
        let config = || scan_config(self.state.config.read().settings());
        let ports = SystemPorts;
        let mut memory = self.memory.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let avrdude_log = openrigcue_effects::accessories::firmware::log_path(&memory.documents().unwrap_or_default());
        let mut avrdude = SystemAvrdude::default();
        let translate = |text: &str| self.translate(text);
        let mut env = ScanEnvironment {
            ports: &ports,
            opener: &ports,
            clock: Arc::new(SystemClock) as Arc<dyn Clock>,
            memory: &mut *memory,
            avrdude: &mut avrdude,
            motion_ports: &motion_ports,
            installation: Arc::clone(&self.installation),
            native_installation: Some(Arc::clone(&self.native_installation)),
            translate: &translate,
            avrdude_log,
            belt_watchdog: Arc::new(BeltSlotWatchdog(Arc::downgrade(&self.accessories.belt))),
        };
        let scan_result = pass(&mut env, &config);
        drop(env);
        drop(memory);
        let scan_result = scan_result
            .and_then(|()| self.motion.check_ports(&self.state).map_err(|e| ScanError::Motion(e.to_string())));
        crate::device_access::check(&self.state);
        scan_result
    }
}

/// `APP.enable_hapticr` as a boolean: the HaptiCR switch, which a configuration migration sets to `no`; `false`
/// while the configuration holds no such key.
pub fn enable_hapticr(settings: &Settings) -> bool {
    // The stored value through the lenient boolean parse (text such as 'yes' is true); a missing key or a
    // value the parse rejects is an error there, which skips the HaptiCR stream as a false switch does.
    let raw = openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot());
    native_path(&raw, &["APP", "enable_hapticr"])
        .ok()
        .and_then(|value| native_str2bool::<NativeCompensationError>(&value).ok())
        .unwrap_or(false)
}

/// The search settings of the connection scan.
pub fn scan_config(settings: &Settings) -> ScanConfig {
    let search = |enabled: bool, port: &str| DeviceSearch { enabled, com_port: port.to_owned() };
    ScanConfig {
        allowed: AllowedPorts::parse(&settings.app.allowed_serial_ports.join(",")),
        intellibox: search(settings.wind.auto_search, &settings.wind.serial_port),
        belt: search(settings.belt.auto_search, &settings.belt.serial_port),
        sim3d: search(settings.sim3d.auto_search, &settings.sim3d.serial_port),
        sim3d_model_set: settings.sim3d.model != "None",
        hapticr: search(settings.hapticr.auto_search, &settings.hapticr.serial_port),
        hapticr_enabled: enable_hapticr(settings),
        iflag: search(settings.iflag.auto_search, &settings.iflag.serial_port),
        imu: search(settings.compensation.auto_search, &settings.compensation.serial_port),
        imu_sensor_mode: settings.compensation.sensor == CompensationSensor::Witmotion,
    }
}

/// Starts the motion-compensation CSV log on six-actuator rigs, when the compensation thread starts.
pub fn start_pose_log(accessories: &Accessories, model: &str, documents: &Path) {
    let started = chrono::Local::now().naive_local();
    if let Some(log) =
        openrigcue_effects::accessories::compensation::start_log(RigModel::from_model(model), Some(documents), started)
    {
        accessories.attach_pose_log(log);
    }
}

/// The scan wake of the connection scan: with `enable_event_driven_device_scan` a udev watcher reports device
/// changes to the scan trigger, which starts raised so the first pass runs at once; otherwise a scan every second.
///
/// A watcher that cannot start is logged and the scan still waits for device events.
pub fn scan_wake(settings: &Settings, stop: Arc<AtomicBool>) -> openrigcue_effects::accessories::dispatcher::ScanWake {
    use openrigcue_effects::accessories::dispatcher::ScanWake;
    if !settings.features.event_driven_device_scan {
        return ScanWake::EverySecond;
    }
    let trigger = primed_scan_trigger();
    if let Err(error) = openrigcue_effects::accessories::serial_events::watch_udev(Arc::clone(&trigger), stop) {
        tracing::error!(%error, "usb_device_monitor: the device event monitor did not start");
    }
    ScanWake::OnSerialEvents(trigger)
}

/// The connection scan event, already raised, so the first pass runs at start-up and finds the devices
/// that are already plugged in.
fn primed_scan_trigger() -> Arc<openrigcue_effects::accessories::serial_events::ScanTrigger> {
    let trigger = Arc::new(openrigcue_effects::accessories::serial_events::ScanTrigger::new());
    trigger.raise();
    trigger
}

/// The view of the accessories the Hardware page shows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccessoryStatus {
    pub intellibox: String,
    pub intellibox_port: String,
    pub belt: String,
    pub belt_port: String,
    pub hapticr: String,
    pub sim3d: String,
    pub iflag: String,
    pub compensation: String,
    pub compensation_port: String,
    /// The Intellibox model and version (-1 until identified).
    pub intellibox_model: i64,
    pub intellibox_version: i64,
    /// The model, version and serial number of the connected belt.
    pub belt_identity: Option<BeltIdentityView>,
    /// The belt homing state: 0 not calibrated, 1 calibrated, 2 requested, 3 running.
    pub belt_homing: u8,
    pub iflag_port: String,
    /// The iFlag firmware version: integer zero initially/on parse failure, then the float version.
    pub iflag_firmware: serde_json::Value,
    pub sim3d_port: String,
    pub hapticr_port: String,
    /// The HUD status of the OBD2HUD thread (`crate::hud`): `Not Connected`, `Connected` or the
    /// flashing progress.
    pub hud: String,
}

/// `belt_model`, `belt_version`, `belt_sn`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BeltIdentityView {
    pub model: i64,
    pub version: i64,
    pub serial: String,
}

/// The belt homing state as its integer code.
pub fn homing_code(homing: openrigcue_effects::accessories::belt::Homing) -> u8 {
    use openrigcue_effects::accessories::belt::Homing;
    match homing {
        Homing::NotCalibrated => 0,
        Homing::Calibrated => 1,
        Homing::Requested => 2,
        Homing::InProgress => 3,
    }
}

/// The belt slot's homing state for a homing code the UI writes (0 or 2).
pub fn homing_of(code: u8) -> openrigcue_effects::accessories::belt::Homing {
    use openrigcue_effects::accessories::belt::Homing;
    match code {
        1 => Homing::Calibrated,
        2 => Homing::Requested,
        3 => Homing::InProgress,
        _ => Homing::NotCalibrated,
    }
}

/// Each device's status and the held ports.
pub fn accessory_status(accessories: &Accessories) -> AccessoryStatus {
    let identity = accessories.belt_identity();
    AccessoryStatus {
        intellibox: accessories.connection_status(Device::Intellibox),
        intellibox_port: accessories.connection_port(Device::Intellibox),
        belt: accessories.connection_status(Device::Belt),
        belt_port: accessories.connection_port(Device::Belt),
        hapticr: accessories.connection_status(Device::HaptiCr),
        sim3d: accessories.connection_status(Device::Sim3d),
        iflag: accessories.connection_status(Device::IFlag),
        compensation: accessories.connection_status(Device::MotionCompensation),
        compensation_port: accessories.connection_port(Device::MotionCompensation),
        intellibox_model: accessories.intellibox_model(),
        intellibox_version: accessories.intellibox_version(),
        belt_identity: identity.as_ref().map(|i| BeltIdentityView {
            model: i.model,
            version: i.version,
            serial: i.serial.clone(),
        }),
        belt_homing: homing_code(accessories.belt_homing()),
        iflag_port: accessories.connection_port(Device::IFlag),
        iflag_firmware: serde_json::json!(accessories.iflag_version()),
        sim3d_port: accessories.connection_port(Device::Sim3d),
        hapticr_port: accessories.connection_port(Device::HaptiCr),
        hud: accessories.hud.status().text(),
    }
}

/// Keeps the shared state used by the API in one place.
pub type SharedAccessories = Arc<Accessories>;

/// The test the Hardware page is running (`current_test` while its buttons are disabled and the pressed one
/// reads "Testing").
static RUNNING_TEST: Mutex<Option<HardwareTest>> = Mutex::new(None);

/// The test whose button reads "Testing" while every test button is disabled; `None` when the buttons are
/// enabled and read "Test".
pub fn running_hardware_test() -> Option<HardwareTest> {
    *RUNNING_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The hardware-page "Test" buttons of wind, LEDs and tachometer: every test button is
/// disabled and the pressed one reads "Testing" ([`running_hardware_test`]); the switches go to "Always On" for
/// seven seconds, then back to what they were, and the buttons are enabled again. A press while a test runs
/// does nothing (its button is disabled); returns whether the test ran.
pub fn hardware_test(controls: &Shared<crate::runtime::Controls>, target: HardwareTest) -> bool {
    {
        let mut running = RUNNING_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if running.is_some() {
            return false;
        }
        *running = Some(target);
    }
    let previous = controls.read().switches;
    {
        let mut c = controls.write();
        match target {
            HardwareTest::Wind => {
                c.switches.wind = OutputMode::AlwaysOn;
                c.switches.wind_center = OutputMode::AlwaysOn;
            }
            HardwareTest::Led => c.switches.led = OutputMode::AlwaysOn,
            HardwareTest::Tach => c.switches.tach = OutputMode::AlwaysOn,
        }
    }
    std::thread::sleep(HARDWARE_TEST_DURATION);
    {
        let mut c = controls.write();
        match target {
            HardwareTest::Wind => {
                c.switches.wind = previous.wind;
                c.switches.wind_center = previous.wind_center;
            }
            HardwareTest::Led => c.switches.led = previous.led,
            HardwareTest::Tach => c.switches.tach = previous.tach,
        }
    }
    *RUNNING_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    true
}

/// The test lasts seven one-second sleeps.
pub const HARDWARE_TEST_DURATION: Duration = Duration::from_secs(7);

/// The outputs the hardware-page test buttons drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareTest {
    Wind,
    Led,
    Tach,
}

#[cfg(test)]
mod tests {
    /// A detached plain tree of a stored row (object handles resolved recursively), for fixtures that
    /// edit a profile row and store it back.
    fn plain(value: openrigcue_model::native_value::LegacyValue) -> openrigcue_model::native_value::LegacyValue {
        use openrigcue_model::native_value::LegacyValue;
        match value.resolved() {
            LegacyValue::Dict(fields) => {
                LegacyValue::Dict(fields.iter().map(|(k, v)| (k.clone(), plain(v.clone()))).collect())
            }
            LegacyValue::List(items) => LegacyValue::List(items.into_iter().map(plain).collect()),
            other => other,
        }
    }

    /// The initialisation's reads of a settings snapshot and profile row.
    fn init_of(
        settings: &super::Settings,
        profile: &openrigcue_model::native_value::LegacyValue,
    ) -> openrigcue_effects::accessories::intellibox::NativeInitParameters {
        super::native_intellibox_init(
            &openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot()),
            profile,
        )
    }

    /// A tune level as an integer (0 on failure), as the stream reads it.
    fn activation_of(profile: Option<&openrigcue_model::native_value::LegacyValue>, section: &str, key: &str) -> f64 {
        openrigcue_effects::accessories::values::activation(profile, section, key, -1, 1).unwrap()
    }

    /// The "Always On" colour of a settings snapshot.
    fn led_color_of(
        settings: &super::Settings,
        index: usize,
    ) -> Result<openrigcue_effects::accessories::led::ColorValue, openrigcue_effects::accessories::led::NativeLedError>
    {
        super::native_led_color(&openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot()), index)
    }

    #[test]
    fn iflag_uses_its_original_missing_field_fallbacks_for_partial_publications() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut capture = Vec::new();
        let mut cycle = |_: &IFlagSettings,
                         inputs: &IFlagInputs,
                         _: &dyn Fn() -> Option<Duration>,
                         _: Option<&dyn Fn() -> IFlagSettings>| {
            capture.push(inputs.clone());
            Some(Duration::ZERO)
        };
        feed.iflag(&mut cycle);
        state.telemetry.write().set_native("gear", LegacyValue::Int(3));
        state.telemetry.write().set_native("rpm", LegacyValue::Int(4000));
        feed.iflag(&mut cycle);
        state.telemetry.write().set_native("max_rpm", LegacyValue::Int(8000));
        feed.iflag(&mut cycle);
        let [empty, partial, complete] = capture.as_slice() else { panic!("three cycles") };
        assert_eq!((empty.gear, empty.rpm, empty.max_rpm, empty.flag), (11.0, 0.0, 0.0, 0.0));
        assert_eq!((partial.gear, partial.rpm, partial.max_rpm), (3.0, 4000.0, 0.0));
        assert_eq!(partial.native_gear, Some(LegacyValue::Int(3)));
        assert_eq!(complete.max_rpm, 8000.0);
        let native = |inputs: &IFlagInputs| inputs.native.clone().unwrap();
        assert_eq!(native(empty).rpm, None, "a missing key is the caught KeyError, not a typed zero");
        assert_eq!(native(partial).rpm, Some(LegacyValue::Int(4000)));
        assert_eq!(native(complete).max_rpm, Some(LegacyValue::Int(8000)));
    }

    #[test]
    fn intellibox_waits_before_inputs_cycle_and_invalid_refresh_rate() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| config.settings.wind.refresh_rate = 0);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut calls = 0;
        let mut cycle = |_: &IntelliboxInputs<'_>, period: Option<Duration>| {
            calls += 1;
            assert_eq!(period, None);
            None
        };
        assert_eq!(feed.intellibox(&mut cycle), Some(Duration::from_millis(500)));
        state.telemetry.write().set_native("spotter_right", openrigcue_model::native_value::LegacyValue::Int(0));
        *state.telemetry_ready.write() = true;
        assert_eq!(feed.intellibox(&mut cycle), None);
        assert_eq!(calls, 1, "only the ready loop entered the cycle");
    }

    /// The scan event starts set, so the first wait returns at once, and the next one waits.
    #[test]
    fn scan_trigger_starts_raised() {
        let trigger = super::primed_scan_trigger();
        let start = std::time::Instant::now();
        trigger.wait_and_clear(Some(std::time::Duration::from_secs(5)));
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "first scan is not delayed");
        let start = std::time::Instant::now();
        trigger.wait_and_clear(Some(std::time::Duration::from_millis(200)));
        assert!(start.elapsed() >= std::time::Duration::from_millis(150), "cleared after the first pass");
    }

    use super::*;
    use openrigcue_config::tunes::FactoryTunes;

    /// Compensation pass: horizontal surge only on models whose port mapping holds the entry Surge; its position
    /// is the motion stream's value at that entry's index (none without a stream); manufacturer DOF REALITY
    /// decides the estimator.
    #[test]
    fn compensation_rig_from_the_model() {
        let rig = compensation_rig("P6", None);
        assert_eq!(rig.model, RigModel::P6);
        assert_eq!(rig.dof_reality, Some(true));
        assert!(rig.horizontal_surge.is_none());
        assert_eq!(compensation_rig("not a model", None).dof_reality, None);
        let stream = [0, 1, 2, 3, 40000, 5, 0, 0, 0, 0, 0, 0];
        let pt = compensation_rig("5 Actuators [2 Front + 2 Rear + Surge]", Some(&stream));
        let surge = pt.horizontal_surge.expect("surge actuator");
        assert_eq!(surge.position, Some(openrigcue_model::native_value::LegacyValue::Int(40000)));
        assert_eq!(
            surge.max_motor_abs,
            openrigcue_devices::motion::motion_model("5 Actuators [2 Front + 2 Rear + Surge]", false)
                .unwrap()
                .max_motor_abs
        );
        assert_eq!(pt.dof_reality, Some(false));
        assert_eq!(
            compensation_rig("5 Actuators [2 Front + 2 Rear + Surge]", None).horizontal_surge.unwrap().position,
            None
        );
    }

    /// The integers of the MOTION_COMPENSATION numbers and the sensor type.
    #[test]
    fn compensation_settings_from_config() {
        let mut settings = Settings::default();
        settings.compensation.surge_horizontal_stroke = 99.9;
        let c = compensation_settings(&settings);
        assert_eq!(c.horizontal_surge.stroke_mm, 99);
        assert_eq!(c.refresh_rate, 90);
        assert_eq!(c.kind, CompensationKind::Virtual);
    }

    #[test]
    fn native_compensation_runtime_factor_string_uses_the_local_estimator_catch() {
        use super::*;
        use openrigcue_effects::accessories::compensation::Compensation;
        use openrigcue_model::native_value::LegacyValue;
        struct NoSleep;
        impl Clock for NoSleep {
            fn now(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
            fn sleep(&self, _: Duration) {
                panic!("the single-pass fixture must not sleep")
            }
        }
        let initial = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {
                "pitch_factor": 70, "roll_factor": 80, "yaw_factor": 90,
                "yaw": {"enabled": true}, "height": 321
            }}))
            .unwrap();
        let rig = Rig { model: RigModel::P6, dof_reality: Some(true), horizontal_surge: None };
        let positions = ActuatorPositions { motors: Some([600, 420, 700, 350, 510, 730]), ..Default::default() };
        let installation: InstallationReader = Arc::new(|| "H".into());
        let clock: Arc<dyn Clock> = Arc::new(NoSleep);
        let mut capture = Compensation::new();
        let mut pass = |settings: &Settings| {
            let settings = compensation_settings(settings);
            capture
                .cycle(
                    None,
                    &CompensationInputs {
                        settings: &settings,
                        native_kind: None,
                        native_displacement: None,
                        native_refresh_rate: None,
                        native_horizontal_surge: None,
                        native_axis_enabled: None,
                        native_axis_controls: None,
                        switch_on: true,
                        rig: &rig,
                        actuators: &positions,
                        heave_from_actuators: true,
                        game_running: None,
                        installation: &installation,
                        native_installation: None,
                    },
                    &clock,
                )
                .unwrap();
            *capture.sensor()
        };
        let before = pass(&initial);
        assert_ne!(before.pitch.raw, 0.0);
        let changed = initial.apply_ui_patch(&serde_json::json!({"compensation": {"pitch_factor": "42.5"}})).unwrap();
        assert_eq!(
            changed.native_snapshot().get("MOTION_COMPENSATION").unwrap().item("pitch_factor"),
            Ok(&LegacyValue::Str("42.5".into()))
        );
        let after = pass(&changed);
        assert_eq!(after.pitch.raw, 0.0, "int(str) failure is caught before publishing this axis");
        assert_eq!(after.pitch.calculated, 0.0);
        assert_eq!(after.roll, before.roll, "the later roll branch still runs");
        assert_eq!(after.yaw, before.yaw, "the later yaw branch still runs");
        assert_eq!(changed.compensation.height, 321.0);
    }

    #[test]
    fn native_compensation_factor_projection_preserves_native_kinds_and_unrelated_values() {
        use super::*;
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {
                "pitch_factor": 71, "roll_factor": 82, "yaw_factor": 93, "height": 321,
                "sway_factor": 64, "surge_factor": 65, "roll": {"reverse": false}
            }}))
            .unwrap();
        let wide = format!("1{}", "0".repeat(400));
        for (raw, expected, failed) in [
            (LegacyValue::Int(42), 42.0, false),
            (LegacyValue::Float(42.5), 42.0, false),
            (LegacyValue::Bool(true), 1.0, false),
            (LegacyValue::from("٤_٢"), 42.0, false),
            (LegacyValue::Bytes(b"42".to_vec()), 42.0, false),
            (LegacyValue::from("9223372036854775808"), 9_223_372_036_854_775_808.0, false),
            (LegacyValue::Float(1e100), 1e100, false),
            (LegacyValue::from("42.5"), 71.0, true),
            (LegacyValue::None, 71.0, true),
            (LegacyValue::from("bad"), 71.0, true),
            (LegacyValue::Float(f64::INFINITY), 71.0, true),
            // The integer conversion succeeds; the estimator's float product then overflows, which it absorbs.
            (LegacyValue::from(wide.as_str()), 71.0, true),
        ] {
            let mut settings = base.clone();
            settings.edit_native(|config| {
                config
                    .get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("pitch_factor", raw.clone());
            });
            let projected = compensation_settings(&settings);
            if !failed {
                assert_eq!(projected.native_estimator_factors.unwrap()[0], expected, "{raw:?}");
            }
            assert_eq!(projected.native_estimator_factors.unwrap()[1..], [82.0, 93.0]);
            assert_eq!(projected.estimator_factor_failures, [failed, false, false], "{raw:?}");
            assert_eq!((projected.roll_factor, projected.yaw_factor), (82, 93));
            assert_eq!((projected.height, projected.sway_factor, projected.surge_factor), (321.0, 64, 65));
            assert!(!projected.roll.reverse);
        }
        let mut missing = base.clone();
        missing.edit_native(|config| {
            config.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("pitch_factor");
        });
        assert_eq!(compensation_settings(&missing).estimator_factor_failures, [true, false, false]);
    }

    #[test]
    fn native_compensation_service_feed_observes_runtime_factor_changes_without_workers() {
        use super::*;
        use crate::compensation::{Framing, PoseStream, Reading};
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::compensation::Compensation;
        use openrigcue_model::native_value::LegacyValue;
        struct NoSleep;
        impl Clock for NoSleep {
            fn now(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
            fn sleep(&self, _: Duration) {
                panic!("single-pass fake must not sleep")
            }
        }
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.motion.model = "P6".into();
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "pitch_factor": 70, "roll_factor": 80, "yaw_factor": 90,
                    "yaw": {"enabled": true}, "height": 321
                }}))
                .unwrap();
        });
        state.controls.write().switches.compensation = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let positions = ActuatorPositions { motors: Some([600, 420, 700, 350, 510, 730]), ..Default::default() };
        let clock: Arc<dyn Clock> = Arc::new(NoSleep);
        let mut capture = Compensation::new();
        let mut stream = PoseStream::new();
        let mut run = |time| {
            feed.with_compensation_inputs(|input| {
                // Published actuator values are injected directly; no motion worker is started.
                let input = CompensationInputs { actuators: &positions, ..*input };
                capture.cycle(None, &input, &clock).unwrap();
                let sample = stream
                    .next(
                        &Reading::without_sensor(time),
                        &input,
                        Framing { cor_offset: Default::default(), recenter_generation: 0 },
                    )
                    .unwrap();
                (*capture.sensor(), capture.sensor_values().get("raw_pitch"), sample)
            })
        };
        let before = run(1_000_000_000);
        assert_ne!(before.0.pitch.calculated, 0.0);
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "pitch_factor": "42.5", "yaw_factor": "bad"
                }}))
                .unwrap();
        });
        let bad = run(2_000_000_000);
        assert_eq!(bad.0.pitch.calculated, 0.0);
        assert_eq!(bad.1, Some(LegacyValue::Int(0)));
        assert_eq!(bad.0.roll, before.0.roll);
        assert_eq!(bad.0.yaw.calculated, 0.0);
        assert_eq!(bad.2.values.pitch, 0.0);
        assert_eq!(bad.2.values.yaw, 0.0);
        assert_ne!(bad.2.values.roll, 0.0);
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "pitch_factor": 70, "yaw_factor": 90
                }}))
                .unwrap();
        });
        let recovered = run(3_000_000_000);
        assert_eq!(recovered.0, before.0);
        assert_eq!(recovered.2.values, before.2.values);
    }

    #[test]
    fn native_intellibox_initialization_feed_preserves_values_and_siblings() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({
                    "leds": {"brightness":"41.5", "nbr_leds":78},
                    "wind": {"fan_frequency":"bad", "refresh_rate":17}
                }))
                .unwrap();
        });
        let mut row = plain(state.profile.read().native_tune());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", LegacyValue::Float(42.5));
        row.as_dict_mut().unwrap().insert("speed_factor", LegacyValue::Int(7));
        state.profile.write().replace_native_tune(row);
        *state.telemetry_ready.write() = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut captured = None;
        feed.intellibox(&mut |inputs, _| {
            captured = Some((inputs.native_init.unwrap().clone(), inputs.tuning.speed_factor, inputs.init));
            None
        });
        let (native, speed, init) = captured.unwrap();
        assert_eq!(speed, 7.0);
        assert_eq!(native.values[0], Ok(LegacyValue::Float(42.5)));
        assert_eq!(native.dim_percent, Ok(42.5));
        assert_eq!(native.values[1], Ok(LegacyValue::Str("41.5".into())));
        assert_eq!(
            native.brightness.unwrap_err(),
            NativeLedError { class: "ValueError", message: "invalid literal for int() with base 10: '41.5'".into() }
        );
        assert_eq!(native.led_count, Ok(78.into()));
        assert_eq!(init.led_count, 78, "bad neighboring fields do not discard accepted values");
        assert_eq!(native.fan_frequency.unwrap_err().class, "ValueError");
        let raw = plain(state.profile.read().native_tune());
        let settings = state.config.read().settings().clone();
        let integer = settings.apply_ui_patch(&serde_json::json!({"leds":{"brightness":41}})).unwrap();
        assert_eq!(init_of(&integer, &raw).brightness, Ok(41.into()));
        let mut missing = integer;
        missing.edit_native(|root| {
            root.remove("INTELLIBOX");
        });
        assert_eq!(
            init_of(&missing, &raw).values[3],
            Err(NativeLedError { class: "KeyError", message: "'INTELLIBOX'".into() })
        );
        for (value, expected) in [
            (LegacyValue::Int(42), Ok(42.0)),
            (LegacyValue::Float(42.5), Ok(42.5)),
            (LegacyValue::Bool(true), Ok(1.0)),
            (LegacyValue::Str("42.5".into()), Err("must be real number, not str")),
            (LegacyValue::Bytes(b"42".to_vec()), Err("must be real number, not bytes")),
            (LegacyValue::None, Err("must be real number, not NoneType")),
        ] {
            let mut profile = raw.clone();
            profile.as_dict_mut().unwrap().insert("gear_shift_dim_perc", value);
            let result = init_of(&settings, &profile).dim_percent;
            assert_eq!(result.map_err(|error| error.message), expected.map_err(str::to_owned));
        }
    }

    #[test]
    fn native_intellibox_initialization_rejects_profile_text_before_first_write() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::intellibox::{Generation, Intellibox, IntelliboxStream, StreamFrame};
        use openrigcue_effects::accessories::led::SegmentFrame;
        use openrigcue_effects::accessories::link::SerialLink;
        use openrigcue_model::native_value::LegacyValue;
        #[derive(Clone, Default)]
        struct Wire(Arc<Mutex<Vec<u8>>>);
        impl SerialLink for Wire {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend(bytes);
                Ok(bytes.len())
            }
            fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
                Ok(None)
            }
            fn read_available(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(Vec::new())
            }
            fn bytes_waiting(&mut self) -> std::io::Result<usize> {
                Ok(0)
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn SerialLink>> {
                Ok(Box::new(self.clone()))
            }
        }
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.insert("gear_shift_dim_perc", LegacyValue::Str("70".into()));
        fields.insert("speed_factor", LegacyValue::Int(7));
        state.config.write().update(|config| config.profiles.set_native_game("ac", row));
        let active =
            crate::profile::ActiveProfile::load_native(&state.config.read(), "ac", &LegacyValue::Int(0), 1).unwrap();
        assert_eq!(active.tune.speed_factor, 7.0);
        assert_eq!(active.tune.gear_shift_dim_perc, 70.0, "typed projection accepts numeric text");
        let settings = Settings::default().apply_ui_patch(&serde_json::json!({"leds":{"brightness":73}})).unwrap();
        let init = InitParameters {
            gear_shift_dim_percent: active.tune.gear_shift_dim_perc,
            led_brightness: i64::from(settings.leds.brightness),
            led_count: i64::from(settings.leds.nbr_leds),
            fan_frequency: i64::from(settings.wind.fan_frequency),
        };
        let native = init_of(&settings, &active.native_tune());
        let frame = StreamFrame {
            left_fan: 0,
            right_fan: 0,
            harmonized_gear: 3.0,
            rpm_tone: 66,
            alert_rpm: 0.0,
            center_fan: 0,
            segments: std::array::from_fn(|_| SegmentFrame::words(1, 50, 0, Some((0, 0)))),
        };
        let wire = Wire::default();
        let mut device = Intellibox::new(Box::new(wire.clone()), Generation::V5);
        let result = IntelliboxStream::new().send_frame_native_initialization(
            &mut device,
            2,
            &frame,
            &init,
            &[1, 50, 51, 100, 101, 125, 126, 150],
            None,
            None,
            Some(&native),
        );
        assert!(result.is_err(), "native map must reject numeric text before the local send catch");
        assert!(wire.0.lock().unwrap().is_empty());
    }

    #[test]
    fn native_led_range_kind_change_resends_equal_numeric_positions() {
        use openrigcue_effects::accessories::intellibox::{Generation, Intellibox, IntelliboxStream, StreamFrame};
        use openrigcue_effects::accessories::led::SegmentFrame;
        use openrigcue_effects::accessories::link::SerialLink;
        use openrigcue_model::native_value::LegacyValue;
        #[derive(Clone, Default)]
        struct Wire(Arc<Mutex<Vec<u8>>>);
        impl SerialLink for Wire {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend(bytes);
                Ok(bytes.len())
            }
            fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
                Ok(None)
            }
            fn read_available(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(Vec::new())
            }
            fn bytes_waiting(&mut self) -> std::io::Result<usize> {
                Ok(0)
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn SerialLink>> {
                Ok(Box::new(self.clone()))
            }
        }
        let mut settings = Settings::default();
        let ranges = [1, 50, 51, 78, 78, 78, 78, 78];
        let frame = StreamFrame {
            left_fan: 0,
            right_fan: 0,
            harmonized_gear: 3.0,
            rpm_tone: 66,
            alert_rpm: 0.0,
            center_fan: 0,
            segments: std::array::from_fn(|_| SegmentFrame::words(1, 50, 0, Some((0, 0)))),
        };
        let init =
            InitParameters { gear_shift_dim_percent: 70.0, led_brightness: 100, led_count: 78, fan_frequency: 3 };
        let wire = Wire::default();
        let mut device = Intellibox::new(Box::new(wire.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        stream
            .send_frame_native_ranges(
                &mut device,
                2,
                &frame,
                &init,
                &ranges,
                None,
                Some(&native_led_settings(&settings).ranges),
            )
            .unwrap();
        wire.0.lock().unwrap().clear();
        settings.edit_native(|root| {
            root.get_mut("LED").unwrap().as_dict_mut().unwrap().insert("segment1_start", LegacyValue::Int(1));
        });
        stream
            .send_frame_native_ranges(
                &mut device,
                2,
                &frame,
                &init,
                &ranges,
                None,
                Some(&native_led_settings(&settings).ranges),
            )
            .unwrap();
        let written = wire.0.lock().unwrap();
        let commands: Vec<_> = written
            .split(|b| *b == b';')
            .filter(|part| !part.is_empty())
            .map(|part| part.split(|b| *b == b',').next().unwrap().to_vec())
            .collect();
        assert_eq!(commands, vec![b"4".to_vec(), b"8".to_vec()], "raw string-to-int change must resend the ranges");
    }

    #[test]
    fn native_led_required_values_keep_types_and_nonfactory_siblings() {
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"leds": {
                "brightness": 73, "segments": [{"start": 7, "end": 12, "kind": "SO"}]
            }}))
            .unwrap();
        let raw = native_led_settings(&base);
        assert_eq!(raw.segments[0].start, Ok(7.into()));
        assert_eq!(raw.segments[0].end, Ok(12.into()));
        assert_eq!(raw.segments[0].kind, Ok(SegmentType::SolidAlwaysOn));
        for (value, expected) in [
            (LegacyValue::Float(2.5), Ok(2)),
            (LegacyValue::Bytes(b"-1".to_vec()), Ok(-1)),
            (
                LegacyValue::from("2.5"),
                Err(NativeLedError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: '2.5'".into(),
                }),
            ),
        ] {
            let mut settings = base.clone();
            settings.edit_native(|root| {
                root.get_mut("LED").unwrap().as_dict_mut().unwrap().insert("nbr_segments", value);
            });
            assert_eq!(
                native_led_settings(&settings).segment_count,
                expected.clone().map(openrigcue_model::native_value::BigInt::from)
            );
            assert_eq!(settings.leds.brightness, 73);
            assert_eq!(native_led_settings(&settings).segments[0].start, Ok(7.into()));
        }
        let mut missing = base;
        missing.edit_native(|root| {
            root.get_mut("LED").unwrap().as_dict_mut().unwrap().remove("segment1_type");
        });
        assert_eq!(
            native_led_settings(&missing).segments[0].kind,
            Err(NativeLedError { class: "KeyError", message: "'segment1_type'".into() })
        );
    }

    #[test]
    fn native_wind_factor_feed_preserves_raw_numeric_kinds_and_missing_value() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::NativeSpeedFactor;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        *state.telemetry_ready.write() = true;
        for (value, expected) in [
            (Some(LegacyValue::Int(3)), NativeSpeedFactor::Number(3.0)),
            (Some(LegacyValue::Float(3.5)), NativeSpeedFactor::Number(3.5)),
            (Some(LegacyValue::Bool(true)), NativeSpeedFactor::Number(1.0)),
            (Some(LegacyValue::Str("3".into())), NativeSpeedFactor::Failed),
            (Some(LegacyValue::None), NativeSpeedFactor::Failed),
            (None, NativeSpeedFactor::Failed),
        ] {
            let mut row = plain(state.profile.read().native_tune());
            row.as_dict_mut().unwrap().insert("power_wind_min_speed_perc", 17.into());
            match value {
                Some(value) => {
                    row.as_dict_mut().unwrap().insert("speed_factor", value);
                }
                None => {
                    row.as_dict_mut().unwrap().remove("speed_factor");
                }
            }
            state.profile.write().replace_native_tune(row);
            feed.intellibox(&mut |inputs, _| {
                assert_eq!(inputs.tuning.native_speed_factor, Some(expected));
                assert_eq!(inputs.tuning.min_percent, 17.0);
                None
            });
            feed.sim3d(&mut |inputs| {
                assert_eq!(inputs.wind_tuning.native_speed_factor, Some(expected));
                assert_eq!(inputs.wind_tuning.min_percent, 17.0);
                None
            });
        }
    }

    #[test]
    fn native_iflag_output_feed_preserves_required_integer_values() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.edit_native(|root| {
                let section = root.get_mut("IFLAG").unwrap().as_dict_mut().unwrap();
                section.insert("rotation", "90.5".into());
                section.insert("brightness", "62".into());
            })
        });
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        feed.iflag(&mut |settings, _, _, _| {
            let native = settings.native_output.as_ref().unwrap();
            assert_eq!(native.rotation.as_ref().unwrap_err().class, "ValueError");
            assert_eq!(native.rotation.as_ref().unwrap_err().message, "invalid literal for int() with base 10: '90.5'");
            assert_eq!(native.brightness, Ok(62.into()));
            None
        });
        state.config.write().update(|config| {
            config.settings.edit_native(|root| {
                let section = root.get_mut("IFLAG").unwrap().as_dict_mut().unwrap();
                section.insert("rotation", LegacyValue::Float(90.5));
                section.insert("brightness", LegacyValue::None);
            })
        });
        let config = state.config.read();
        let native = native_iflag_settings(config.settings(), &state.profile.read()).native_output.unwrap();
        assert_eq!(native.rotation, Ok(90.into()));
        assert_eq!(native.brightness.unwrap_err().class, "TypeError");
        drop(config);
        state.config.write().update(|config| {
            config.settings.edit_native(|root| {
                root.get_mut("IFLAG").unwrap().as_dict_mut().unwrap().remove("rotation");
            })
        });
        let config = state.config.read();
        assert_eq!(
            native_iflag_settings(config.settings(), &state.profile.read())
                .native_output
                .unwrap()
                .rotation
                .unwrap_err()
                .message,
            "'rotation'"
        );
    }

    #[test]
    fn native_iflag_controls_feed_keeps_truthy_profile_text_and_sibling_values() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        let switches = row.as_dict_mut().unwrap().get_mut("iflag").unwrap().as_dict_mut().unwrap();
        for key in ["gear_display", "rpm", "sli", "spotter_left", "spotter_right"] {
            switches.insert(key, "false".into());
        }
        state.profile.write().replace_native_tune(row);
        assert!(!state.profile.read().tune.iflag.gear_display);
        assert!(!state.profile.read().tune.iflag.sli);
        feed.iflag(&mut |settings, _, _, _| {
            let native = settings.native_controls.as_ref().unwrap();
            assert!(native.gear_display);
            assert!(native.rpm);
            assert_eq!(native.sli, Ok(true));
            assert!(native.spotter_left);
            assert!(native.spotter_right);
            None
        });
        assert_eq!(state.profile.read().tune.gear_shift_dim_perc, 37.0);
    }

    #[test]
    fn native_iflag_controls_distinguish_caught_fields_and_required_sli() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let base = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        for (section, class, message) in [
            (Some(LegacyValue::Dict(Dict::new())), "KeyError", "'sli'"),
            (Some(LegacyValue::None), "TypeError", "'NoneType' object is not subscriptable"),
            (None, "KeyError", "'iflag'"),
        ] {
            let mut row = base.clone();
            let fields = row.as_dict_mut().unwrap();
            fields.insert("gear_shift_dim_perc", 37.into());
            if let Some(section) = section {
                fields.insert("iflag", section);
            } else {
                fields.remove("iflag");
            }
            state.profile.write().replace_native_tune(row);
            let profile = state.profile.read();
            let projected = native_iflag_settings(state.config.read().settings(), &profile);
            let native = projected.native_controls.unwrap();
            assert!(!native.gear_display);
            assert!(!native.rpm);
            assert!(!native.spotter_left);
            assert!(!native.spotter_right);
            let error = native.sli.unwrap_err();
            assert_eq!((error.class, error.message.as_str()), (class, message));
            assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        }
        let mut row = base;
        let fields = row.as_dict_mut().unwrap();
        fields.insert("iflag", LegacyValue::Dict(Dict::from_iter([("sli".into(), false.into())])));
        state.profile.write().replace_native_tune(row);
        let native =
            native_iflag_settings(state.config.read().settings(), &state.profile.read()).native_controls.unwrap();
        assert_eq!(native.sli, Ok(false));
        assert!(!native.gear_display && !native.rpm && !native.spotter_left && !native.spotter_right);
    }

    #[test]
    fn native_iflag_flag_feed_preserves_literal_false_identity_from_stored_rows() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let base = state.config.read().profile_row("ac").unwrap().unwrap();
        for (raw, enabled) in [
            (Some(LegacyValue::Bool(false)), false),
            (Some(LegacyValue::Int(0)), true),
            (Some(LegacyValue::Float(0.0)), true),
            (Some(LegacyValue::Str(String::new())), true),
            (Some(LegacyValue::from("false")), true),
            (Some(LegacyValue::None), true),
            (Some(LegacyValue::List(Vec::new())), true),
            (Some(LegacyValue::Bool(true)), true),
            (None, false),
        ] {
            let mut row = base.clone();
            let fields = row.as_dict_mut().unwrap();
            fields.insert("gear_shift_dim_perc", 37.into());
            let flags = fields.get_mut("iflag").unwrap().as_dict_mut().unwrap();
            match &raw {
                Some(value) => {
                    flags.insert("blue", value.clone());
                }
                None => {
                    flags.remove("blue");
                }
            }
            state.config.write().stage_game_row("ac", Some(row)).unwrap();
            let active =
                crate::profile::ActiveProfile::load_native(&state.config.read(), "ac", &LegacyValue::Int(0), 1)
                    .unwrap();
            *state.profile.write() = active;
            feed.iflag(&mut |settings, _, _, _| {
                let flags = settings.native_flags.as_ref().unwrap();
                assert_eq!(flags.get("blue").copied().unwrap_or(false), enabled, "{raw:?}");
                None
            });
            let profile = state.profile.read();
            let native = profile.native_tune();
            assert_eq!(native.item_owned("gear_shift_dim_perc"), Ok(LegacyValue::Int(37)));
            if matches!(raw, Some(LegacyValue::Bool(_) | LegacyValue::Int(_) | LegacyValue::Float(_)))
                || raw == Some(LegacyValue::from("false"))
            {
                assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
            }
            if raw == Some(LegacyValue::Int(0)) {
                assert!(!profile.tune.iflag.blue, "zero is false only in the compatibility view");
            }
            assert_eq!(
                native.item_owned("iflag").unwrap().item_owned("blue").ok().map(|v| v.resolved()).as_ref(),
                raw.as_ref()
            );
        }
    }

    #[test]
    fn native_iflag_flag_invalid_parent_is_a_local_empty_map() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let base = plain(state.profile.read().native_tune());
        for parent in [None, Some(LegacyValue::None), Some(LegacyValue::List(Vec::new()))] {
            let mut row = base.clone();
            let fields = row.as_dict_mut().unwrap();
            fields.insert("gear_shift_dim_perc", 37.into());
            match parent {
                Some(value) => {
                    fields.insert("iflag", value);
                }
                None => {
                    fields.remove("iflag");
                }
            }
            state.profile.write().replace_native_tune(row);
            let profile = state.profile.read();
            let projected = native_iflag_settings(state.config.read().settings(), &profile);
            assert!(projected.native_flags.unwrap().is_empty());
            assert!(projected.native_controls.unwrap().sli.is_err(), "earlier required SLI failure stays distinct");
            assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        }
    }

    #[test]
    fn native_iflag_mirror_feed_keeps_required_lookup_and_str2bool_errors() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        for (raw, expected) in [
            (Some(LegacyValue::from("yes")), Ok(true)),
            (Some(LegacyValue::from("no")), Ok(false)),
            (Some(LegacyValue::Bool(true)), Ok(true)),
            (Some(LegacyValue::Int(1)), Ok(true)),
            (Some(LegacyValue::Int(0)), Ok(false)),
            (Some(LegacyValue::from("maybe")), Err(("Exception", "Boolean value expected. Got maybe"))),
            (Some(LegacyValue::None), Err(("TypeError", "can only concatenate str (not \"NoneType\") to str"))),
            (None, Err(("KeyError", "'mirror'"))),
        ] {
            state.config.write().update(|config| {
                config.settings.edit_native(|root| {
                    let iflag = root.get_mut("IFLAG").unwrap().as_dict_mut().unwrap();
                    iflag.insert("brightness", "62".into());
                    match &raw {
                        Some(value) => {
                            iflag.insert("mirror", value.clone());
                        }
                        None => {
                            iflag.remove("mirror");
                        }
                    }
                });
            });
            feed.iflag(&mut |settings, _, _, _| {
                let output = settings.native_output.as_ref().unwrap();
                let actual =
                    output.mirror.as_ref().map(|value| *value).map_err(|error| (error.class, error.message.as_str()));
                assert_eq!(actual, expected, "{raw:?}");
                assert_eq!(output.brightness, Ok(62.into()));
                None
            });
        }
    }

    #[test]
    fn native_accessory_map_feed_retains_bound_errors_and_numeric_values() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut row = plain(state.profile.read().native_tune());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        for key in ["surge_acceleration", "surge_deceleration", "heave"] {
            row.as_dict_mut()
                .unwrap()
                .get_mut("max_telemetry_vars")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "2".into());
        }
        state.profile.write().replace_native_tune(row.clone());
        feed.sim3d(&mut |inputs| {
            for bounds in [
                inputs.tuning.native_map_maxima.as_ref().unwrap(),
                inputs.haptic_tuning.native_map_maxima.as_ref().unwrap(),
            ] {
                for field in [&bounds.surge_acceleration, &bounds.surge_deceleration, &bounds.heave] {
                    assert_eq!(field.as_ref().unwrap_err().class, "TypeError");
                    assert_eq!(field.as_ref().unwrap_err().message, "must be real number, not str");
                }
            }
            None
        });
        for value in [
            LegacyValue::Bool(true),
            LegacyValue::Int(0),
            LegacyValue::Float(f64::INFINITY),
            LegacyValue::Float(f64::NAN),
        ] {
            row.as_dict_mut()
                .unwrap()
                .get_mut("max_telemetry_vars")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("heave", value.clone());
            state.profile.write().replace_native_tune(row.clone());
            let actual = native_haptic_tuning(&state.profile.read()).native_map_maxima.unwrap().heave.unwrap();
            let expected = value.number().unwrap();
            assert!(actual.to_bits() == expected.to_bits() || actual.is_nan() && expected.is_nan());
        }
        row.as_dict_mut().unwrap().get_mut("max_telemetry_vars").unwrap().as_dict_mut().unwrap().remove("heave");
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        let error = native_sim3d_tuning(&profile).native_map_maxima.unwrap().heave.unwrap_err();
        assert_eq!(error.class, "KeyError");
        assert_eq!(error.message, "'heave'");
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
    }

    #[test]
    fn native_accessory_map_heave_text_fails_even_with_effect_off() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::hapticr::{wheel_level, Wheel, WheelInputs};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("max_telemetry_vars")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("heave", "2".into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("haptic_switch")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("wheel_heave", false.into());
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert_eq!(profile.tune.max_telemetry_vars.heave, 2.0);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let input = WheelInputs {
            abs_lock: 0.0,
            rumble_strips: 0.0,
            gear_change: false,
            surge: 0.0,
            heave: 0.0,
            wheel_slip: 0.0,
            rpm: 0.0,
            max_rpm: 8000.0,
            traction_loss: 0.0,
            wheel_slip_min_activation: 1.0,
        };
        let maxima = HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 };
        let error = wheel_level(Wheel::FrontLeft, &input, &native_haptic_tuning(&profile), &maxima).unwrap_err();
        assert_eq!(error.to_string(), "must be real number, not str");
    }

    #[test]
    fn native_accessory_traction_feed_preserves_denominator_and_missing_lookup() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::NativeTractionMaximum;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut row = plain(state.profile.read().native_tune());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        for value in [
            LegacyValue::Int(5),
            LegacyValue::Float(5.5),
            LegacyValue::Bool(true),
            LegacyValue::from("5"),
            LegacyValue::None,
        ] {
            row.as_dict_mut()
                .unwrap()
                .get_mut("max_telemetry_vars")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("traction_loss", value.clone());
            state.profile.write().replace_native_tune(row.clone());
            feed.sim3d(&mut |inputs| {
                assert_eq!(inputs.tuning.native_traction_maximum, Some(NativeTractionMaximum::Value(value.clone())));
                assert_eq!(
                    inputs.haptic_tuning.native_traction_maximum,
                    Some(NativeTractionMaximum::Value(value.clone()))
                );
                None
            });
        }
        row.as_dict_mut()
            .unwrap()
            .get_mut("max_telemetry_vars")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .remove("traction_loss");
        state.profile.write().replace_native_tune(row.clone());
        assert_eq!(
            native_sim3d_tuning(&state.profile.read()).native_traction_maximum,
            Some(NativeTractionMaximum::Missing)
        );
        row.as_dict_mut().unwrap().insert("max_telemetry_vars", LegacyValue::None);
        state.profile.write().replace_native_tune(row);
        assert_eq!(
            native_haptic_tuning(&state.profile.read()).native_traction_maximum,
            Some(NativeTractionMaximum::Missing)
        );
        assert_eq!(state.profile.read().tune.gear_shift_dim_perc, 37.0);
    }

    #[test]
    fn native_accessory_traction_text_maximum_uses_local_zero() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::{
            hapticr::HaptiCr,
            link::SerialLink,
            sim3d::{ShakeIt, Sim3d, Sim3dFans, Sim3dSwitches},
        };
        use openrigcue_model::native_value::LegacyValue;
        #[derive(Clone, Default)]
        struct Wire(Arc<Mutex<Vec<u8>>>);
        impl Wire {
            fn take(&self) -> Vec<u8> {
                std::mem::take(&mut *self.0.lock().unwrap())
            }
        }
        impl SerialLink for Wire {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend(bytes);
                Ok(bytes.len())
            }
            fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
                Ok(None)
            }
            fn read_available(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(vec![3, 0])
            }
            fn bytes_waiting(&mut self) -> std::io::Result<usize> {
                Ok(0)
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn SerialLink>> {
                Ok(Box::new(self.clone()))
            }
        }
        struct NoSleep;
        impl Clock for NoSleep {
            fn now(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
            fn sleep(&self, _: Duration) {
                panic!("immediate fake reply does not wait")
            }
        }
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("max_telemetry_vars")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("traction_loss", "5".into());
        for (section, key) in [
            ("rumble_switch", "throttle_gear"),
            ("rumble_switch", "seat_gear"),
            ("rumble_switch", "brake_gear"),
            ("haptic_switch", "wheel_gear"),
        ] {
            row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap().insert(key, false.into());
        }
        state.profile.write().replace_native_tune(row.clone());
        let profile = state.profile.read();
        assert_eq!(profile.tune.max_telemetry_vars.traction_loss, 5.0);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let mut telemetry = Telemetry::off();
        telemetry.gear = 0.0;
        telemetry.lateral_velocity = 5.0;
        let sim_wire = Wire::default();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(sim_wire.clone())));
        sim.send_stream(
            &telemetry,
            Sim3dFans { left: 0, center: 0, right: 0 },
            &[PortRole::Throttle; 4],
            &Sim3dSwitches { test_port: 0, on: true, game_running: true },
            &native_sim3d_tuning(&profile),
            &Sim3dMaxima { surge_acceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
            &NoSleep,
        )
        .unwrap();
        assert_eq!(&sim_wire.take()[4..11], &[3, 86, 83, 0, 0, 0, 0]);
        let haptic_wire = Wire::default();
        let mut haptic = HaptiCr::new(Box::new(haptic_wire.clone()));
        haptic
            .send_stream(
                &telemetry,
                &HapticSwitches { test_mode: 0, on: true, game_running: true },
                &native_haptic_tuning(&profile),
                &HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
            )
            .unwrap();
        let bytes = haptic_wire.take();
        let bytes: Vec<_> = bytes.into_iter().filter(|b| !b",;/".contains(b)).collect();
        assert_eq!(bytes, vec![b'2', 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        drop(profile);
        row.as_dict_mut()
            .unwrap()
            .get_mut("max_telemetry_vars")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("traction_loss", 5.into());
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        sim.send_stream(
            &telemetry,
            Sim3dFans { left: 0, center: 0, right: 0 },
            &[PortRole::Throttle; 4],
            &Sim3dSwitches { test_port: 0, on: true, game_running: true },
            &native_sim3d_tuning(&profile),
            &Sim3dMaxima { surge_acceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
            &NoSleep,
        )
        .unwrap();
        assert_eq!(&sim_wire.take()[4..11], &[3, 86, 83, 127, 127, 127, 127]);
        haptic
            .send_stream(
                &telemetry,
                &HapticSwitches { test_mode: 0, on: true, game_running: true },
                &native_haptic_tuning(&profile),
                &HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
            )
            .unwrap();
        let bytes: Vec<_> = haptic_wire.take().into_iter().filter(|b| !b",;/".contains(b)).collect();
        assert_eq!(bytes, vec![b'2', 0, 0, 0, 141, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn native_sim3d_continuous_feed_preserves_all_nine_selected_values() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut row = plain(state.profile.read().native_tune());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        for key in [
            "throttle_surge_acceleration",
            "throttle_heave",
            "throttle_traction_loss",
            "brake_surge_deceleration",
            "brake_heave",
            "brake_traction_loss",
            "seat_surge_acceleration",
            "seat_heave",
            "seat_traction_loss",
        ] {
            row.as_dict_mut()
                .unwrap()
                .get_mut("rumble_switch")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "false".into());
            row.as_dict_mut()
                .unwrap()
                .get_mut("rumble_effects")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "9.5".into());
        }
        state.profile.write().replace_native_tune(row.clone());
        feed.sim3d(&mut |inputs| {
            let controls = inputs.tuning.native_continuous_controls.as_ref().unwrap();
            for control in controls.throttle.iter().chain(&controls.brake).chain(&controls.seat) {
                assert_eq!(control.enabled, Ok(true));
                assert_eq!(control.level.as_ref().unwrap_err().class, "ValueError");
                assert_eq!(
                    control.level.as_ref().unwrap_err().message,
                    "invalid literal for int() with base 10: '9.5'"
                );
            }
            None
        });
        row.as_dict_mut()
            .unwrap()
            .get_mut("rumble_effects")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .remove("brake_surge_deceleration");
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        let error =
            native_sim3d_tuning(&profile).native_continuous_controls.unwrap().brake[0].level.clone().unwrap_err();
        assert_eq!(error.class, "KeyError");
        assert_eq!(error.message, "'brake_surge_deceleration'");
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
    }

    #[test]
    fn native_sim3d_continuous_profile_text_false_changes_motor_output() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::{
            link::SerialLink,
            sim3d::{ShakeIt, Sim3d, Sim3dFans, Sim3dSwitches},
        };
        use openrigcue_model::native_value::LegacyValue;
        #[derive(Clone, Default)]
        struct Wire(Arc<Mutex<Vec<u8>>>);
        impl SerialLink for Wire {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend(bytes);
                Ok(bytes.len())
            }
            fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
                Ok(None)
            }
            fn read_available(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(vec![3, 0])
            }
            fn bytes_waiting(&mut self) -> std::io::Result<usize> {
                Ok(0)
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn SerialLink>> {
                Ok(Box::new(self.clone()))
            }
        }
        struct NoSleep;
        impl Clock for NoSleep {
            fn now(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
            fn sleep(&self, _: Duration) {
                panic!("immediate fake reply does not wait")
            }
        }
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        // The row-based tuning reads its maximum from the profile; the
        // typed maxima argument is only the fallback without a row.
        row.as_dict_mut()
            .unwrap()
            .get_mut("max_telemetry_vars")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("heave", 2.into());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("rumble_switch")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("throttle_heave", "false".into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("rumble_effects")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("throttle_heave", 9.into());
        state.profile.write().replace_native_tune(row);
        let wire = Wire::default();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(wire.clone())));
        let mut telemetry = Telemetry::off();
        telemetry.gear = 0.0;
        telemetry.vertical_acceleration = 0.5;
        let profile = state.profile.read();
        assert!(!profile.tune.rumble_switch.throttle_heave);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        sim.send_stream(
            &telemetry,
            Sim3dFans { left: 0, center: 0, right: 0 },
            &[PortRole::Throttle; 4],
            &Sim3dSwitches { test_port: 0, on: true, game_running: true },
            &native_sim3d_tuning(&profile),
            &Sim3dMaxima { surge_acceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
            &NoSleep,
        )
        .unwrap();
        assert_eq!(&wire.0.lock().unwrap()[4..11], &[3, 86, 83, 86, 86, 86, 86]);
    }

    #[test]
    fn native_haptic_continuous_profile_text_false_is_truthy() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::hapticr::{wheel_level, Wheel, WheelInputs};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        row.as_dict_mut()
            .unwrap()
            .get_mut("max_telemetry_vars")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("heave", 2.into());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("haptic_switch")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("wheel_heave", "false".into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("haptic_effects")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("wheel_heave", 9.into());
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert!(!profile.tune.haptic_switch.wheel_heave);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let inputs = WheelInputs {
            abs_lock: 0.0,
            rumble_strips: 0.0,
            gear_change: false,
            surge: 0.0,
            heave: 0.5,
            wheel_slip: 0.0,
            rpm: 0.0,
            max_rpm: 8000.0,
            traction_loss: 0.0,
            wheel_slip_min_activation: 1.0,
        };
        let maxima = HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 };
        assert_eq!(wheel_level(Wheel::FrontLeft, &inputs, &native_haptic_tuning(&profile), &maxima).unwrap(), 95.625);
    }

    #[test]
    fn native_haptic_continuous_feed_retains_raw_controls_and_errors() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::hapticr::{wheel_level, Wheel, WheelInputs};
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut row = plain(state.profile.read().native_tune());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        for key in ["wheel_surge_acceleration", "wheel_surge_deceleration", "wheel_heave", "wheel_traction_loss"] {
            row.as_dict_mut()
                .unwrap()
                .get_mut("haptic_switch")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "false".into());
            row.as_dict_mut()
                .unwrap()
                .get_mut("haptic_effects")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "9.5".into());
        }
        state.profile.write().replace_native_tune(row.clone());
        feed.sim3d(&mut |inputs| {
            for control in inputs.haptic_tuning.native_continuous_controls.as_ref().unwrap() {
                assert_eq!(control.enabled, Ok(true));
                assert_eq!(control.level.as_ref().unwrap_err().class, "ValueError");
                assert_eq!(
                    control.level.as_ref().unwrap_err().message,
                    "invalid literal for int() with base 10: '9.5'"
                );
            }
            None
        });
        for key in ["wheel_surge_acceleration", "wheel_surge_deceleration", "wheel_heave"] {
            row.as_dict_mut()
                .unwrap()
                .get_mut("haptic_switch")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, false.into());
        }
        row.as_dict_mut()
            .unwrap()
            .get_mut("haptic_effects")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .remove("wheel_traction_loss");
        state.profile.write().replace_native_tune(row);
        let inputs = WheelInputs {
            abs_lock: 0.0,
            rumble_strips: 0.0,
            gear_change: false,
            surge: 0.0,
            heave: 0.0,
            wheel_slip: 0.0,
            rpm: 0.0,
            max_rpm: 8000.0,
            traction_loss: 0.0,
            wheel_slip_min_activation: 1.0,
        };
        let maxima = HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 };
        let profile = state.profile.read();
        let error = wheel_level(Wheel::FrontLeft, &inputs, &native_haptic_tuning(&profile), &maxima).unwrap_err();
        assert_eq!(error.to_string(), "'wheel_traction_loss'");
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
    }

    #[test]
    fn native_sim3d_brake_feed_keeps_raw_controls_and_nonfactory_sibling() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut row = plain(state.profile.read().native_tune());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        for key in ["brake_abs_lock", "brake_wheel_slip", "brake_rumble_strips", "brake_gear"] {
            row.as_dict_mut()
                .unwrap()
                .get_mut("rumble_switch")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "false".into());
            row.as_dict_mut()
                .unwrap()
                .get_mut("rumble_effects")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "9.5".into());
        }
        state.profile.write().replace_native_tune(row.clone());
        feed.sim3d(&mut |inputs| {
            for control in &inputs.tuning.native_event_controls.as_ref().unwrap().brake {
                assert_eq!(control.enabled, Ok(true));
                let error = control.level.as_ref().unwrap_err();
                assert_eq!(error.class, "ValueError");
                assert_eq!(error.message, "invalid literal for int() with base 10: '9.5'");
            }
            None
        });
        row.as_dict_mut().unwrap().get_mut("rumble_effects").unwrap().as_dict_mut().unwrap().remove("brake_abs_lock");
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        let control = &native_sim3d_tuning(&profile).native_event_controls.unwrap().brake[0];
        assert_eq!(control.level.as_ref().unwrap_err().class, "KeyError");
        assert_eq!(control.level.as_ref().unwrap_err().message, "'brake_abs_lock'");
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
    }

    #[test]
    fn native_sim3d_brake_profile_changes_fake_motor_output_and_recovers() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::{
            link::SerialLink,
            sim3d::{ShakeIt, Sim3d, Sim3dFans, Sim3dSwitches},
        };
        use openrigcue_model::native_value::LegacyValue;
        #[derive(Clone, Default)]
        struct Wire(Arc<Mutex<Vec<u8>>>);
        impl Wire {
            fn take_written(&self) -> Vec<u8> {
                std::mem::take(&mut *self.0.lock().unwrap())
            }
        }
        impl SerialLink for Wire {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend(bytes);
                Ok(bytes.len())
            }
            fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
                Ok(None)
            }
            fn read_available(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(vec![3, 0])
            }
            fn bytes_waiting(&mut self) -> std::io::Result<usize> {
                Ok(0)
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn SerialLink>> {
                Ok(Box::new(self.clone()))
            }
        }
        struct NoSleep;
        impl Clock for NoSleep {
            fn now(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
            fn sleep(&self, _: Duration) {
                panic!("immediate fake reply does not wait")
            }
        }
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("rumble_switch")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("brake_abs_lock", "false".into());
        row.as_dict_mut()
            .unwrap()
            .get_mut("rumble_effects")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("brake_abs_lock", 9.into());
        state.profile.write().replace_native_tune(row.clone());
        let wire = Wire::default();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(wire.clone())));
        let mut telemetry = Telemetry::off();
        telemetry.abs_lock = 1.0;
        telemetry.gear = 2.0;
        let fans = Sim3dFans { left: 0, center: 0, right: 0 };
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        let maxima = Sim3dMaxima { surge_acceleration: 5.0, heave: 2.0, traction_loss: 5.0 };
        {
            let profile = state.profile.read();
            assert!(!profile.tune.rumble_switch.brake_abs_lock);
            assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
            sim.send_stream(
                &telemetry,
                fans,
                &[PortRole::Brake; 4],
                &switches,
                &native_sim3d_tuning(&profile),
                &maxima,
                &NoSleep,
            )
            .unwrap();
        }
        assert_eq!(&wire.take_written()[4..11], &[3, 86, 83, 114, 114, 114, 114]);
        row.as_dict_mut()
            .unwrap()
            .get_mut("rumble_effects")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("brake_abs_lock", "9.5".into());
        state.profile.write().replace_native_tune(row.clone());
        let error = sim
            .send_stream(
                &telemetry,
                fans,
                &[PortRole::Brake; 4],
                &switches,
                &native_sim3d_tuning(&state.profile.read()),
                &maxima,
                &NoSleep,
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "invalid literal for int() with base 10: '9.5'");
        assert!(wire.take_written().is_empty());
        row.as_dict_mut()
            .unwrap()
            .get_mut("rumble_effects")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("brake_abs_lock", 8.into());
        state.profile.write().replace_native_tune(row);
        sim.send_stream(
            &telemetry,
            fans,
            &[PortRole::Brake; 4],
            &switches,
            &native_sim3d_tuning(&state.profile.read()),
            &maxima,
            &NoSleep,
        )
        .unwrap();
        assert_eq!(&wire.take_written()[4..11], &[3, 86, 83, 102, 102, 102, 102]);
    }

    #[test]
    fn native_sim3d_events_feed_preserves_each_group_control() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut row = plain(state.profile.read().native_tune());
        row.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        for key in [
            "throttle_wheel_slip",
            "throttle_rumble_strips",
            "throttle_gear",
            "seat_wheel_slip",
            "seat_rumble_strips",
            "seat_gear",
            "clutch_gear",
        ] {
            row.as_dict_mut()
                .unwrap()
                .get_mut("rumble_switch")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "false".into());
            row.as_dict_mut()
                .unwrap()
                .get_mut("rumble_effects")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(key, "9.5".into());
        }
        state.profile.write().replace_native_tune(row.clone());
        feed.sim3d(&mut |inputs| {
            let native = inputs.tuning.native_event_controls.as_ref().unwrap();
            for control in native.throttle.iter().chain(native.seat.iter()).chain([&native.clutch]) {
                assert_eq!(control.enabled, Ok(true));
                assert_eq!(control.level.as_ref().unwrap_err().class, "ValueError");
            }
            None
        });
        row.as_dict_mut().unwrap().get_mut("rumble_switch").unwrap().as_dict_mut().unwrap().remove("clutch_gear");
        state.profile.write().replace_native_tune(row);
        let control = native_sim3d_tuning(&state.profile.read()).native_event_controls.unwrap().clutch;
        assert_eq!(control.enabled.unwrap_err().message, "'clutch_gear'");
        assert_eq!(state.profile.read().tune.gear_shift_dim_perc, 37.0);
    }

    #[test]
    fn native_sim3d_event_switch_text_false_remains_truthy() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.get_mut("rumble_switch").unwrap().as_dict_mut().unwrap().insert("throttle_wheel_slip", "false".into());
        fields.get_mut("rumble_effects").unwrap().as_dict_mut().unwrap().insert("throttle_wheel_slip", 9.into());
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert!(!profile.tune.rumble_switch.throttle_wheel_slip);
        assert_eq!(
            native_sim3d_tuning(&profile).native_event_controls.unwrap().throttle[0].enabled,
            Ok(true),
            "native nonempty switch text is true"
        );
    }

    #[test]
    fn native_haptic_event_feed_keeps_raw_switch_and_selected_level() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut base = plain(state.profile.read().native_tune());
        base.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        let keys = ["wheel_abs_lock", "wheel_wheel_slip", "wheel_rumble_strips", "wheel_gear"];
        for (switch, expected) in
            [(Some(LegacyValue::Str("false".into())), Some(true)), (Some(LegacyValue::None), Some(false)), (None, None)]
        {
            let mut row = base.clone();
            for key in keys {
                let fields = row.as_dict_mut().unwrap().get_mut("haptic_switch").unwrap().as_dict_mut().unwrap();
                match &switch {
                    Some(value) => {
                        fields.insert(key, value.clone());
                    }
                    None => {
                        fields.remove(key);
                    }
                }
                row.as_dict_mut()
                    .unwrap()
                    .get_mut("haptic_effects")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert(key, "9.5".into());
            }
            state.profile.write().replace_native_tune(row);
            feed.sim3d(&mut |inputs| {
                for control in inputs.haptic_tuning.native_event_controls.as_ref().unwrap() {
                    match expected {
                        Some(expected) => assert_eq!(control.enabled, Ok(expected)),
                        None => assert_eq!(control.enabled.as_ref().unwrap_err().class, "KeyError"),
                    }
                    assert_eq!(control.level.as_ref().unwrap_err().class, "ValueError");
                }
                None
            });
            assert_eq!(state.profile.read().tune.gear_shift_dim_perc, 37.0);
        }
    }

    #[test]
    fn native_haptic_event_switch_text_false_remains_truthy() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::hapticr::{wheel_level, Wheel, WheelInputs};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.get_mut("haptic_switch").unwrap().as_dict_mut().unwrap().insert("wheel_abs_lock", "false".into());
        fields.get_mut("haptic_effects").unwrap().as_dict_mut().unwrap().insert("wheel_abs_lock", 9.into());
        fields.insert("gear_shift_dim_perc", 37.into());
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert!(!profile.tune.haptic_switch.wheel_abs_lock);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let input = WheelInputs {
            abs_lock: 1.0,
            rumble_strips: 0.0,
            gear_change: false,
            surge: 0.0,
            heave: 0.0,
            wheel_slip: 0.0,
            rpm: 0.0,
            max_rpm: 8000.0,
            traction_loss: 0.0,
            wheel_slip_min_activation: 1.0,
        };
        let value = wheel_level(
            Wheel::FrontLeft,
            &input,
            &native_haptic_tuning(&profile),
            &HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
        )
        .unwrap();
        assert_eq!(value, 127.5);
    }

    #[test]
    fn native_engine_control_feed_keeps_truthy_switches_and_integer_failures() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let base = plain(state.profile.read().native_tune());
        for (switch, expected) in [
            (LegacyValue::Str("false".into()), true),
            (LegacyValue::Str(String::new()), false),
            (LegacyValue::Int(2), true),
            (LegacyValue::None, false),
        ] {
            let mut row = base.clone();
            for (section, key) in [
                ("haptic_switch", "wheel_engine"),
                ("rumble_switch", "throttle_engine"),
                ("rumble_switch", "seat_engine"),
            ] {
                row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap().insert(key, switch.clone());
            }
            for (section, key) in [
                ("haptic_effects", "wheel_engine"),
                ("rumble_effects", "throttle_engine"),
                ("rumble_effects", "seat_engine"),
            ] {
                row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap().insert(key, "9.5".into());
            }
            state.profile.write().replace_native_tune(row);
            feed.sim3d(&mut |inputs| {
                for control in [
                    inputs.haptic_tuning.native_engine_control.as_ref().unwrap(),
                    inputs.tuning.native_engine_control[0].as_ref().unwrap(),
                    inputs.tuning.native_engine_control[1].as_ref().unwrap(),
                ] {
                    assert_eq!(control.enabled, Ok(expected));
                    assert_eq!(control.level.as_ref().unwrap_err().class, "ValueError");
                }
                None
            });
        }
        let mut row = base;
        row.as_dict_mut().unwrap().get_mut("haptic_switch").unwrap().as_dict_mut().unwrap().remove("wheel_engine");
        state.profile.write().replace_native_tune(row);
        assert_eq!(
            native_haptic_tuning(&state.profile.read()).native_engine_control.unwrap().enabled.unwrap_err().message,
            "'wheel_engine'"
        );
    }

    #[test]
    fn native_engine_switch_text_false_remains_truthy() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::hapticr::{wheel_level, Wheel, WheelInputs};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.get_mut("haptic_switch").unwrap().as_dict_mut().unwrap().insert("wheel_engine", "false".into());
        let effects = fields.get_mut("haptic_effects").unwrap().as_dict_mut().unwrap();
        effects.insert("wheel_engine", 9.into());
        effects.insert("rpm_start_rumble", 0.into());
        effects.insert("rpm_start_fading", 70.into());
        effects.insert("rpm_stop_fading", 95.into());
        fields.insert("gear_shift_dim_perc", 37.into());
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert!(!profile.tune.haptic_switch.wheel_engine);
        let tune = native_haptic_tuning(&profile);
        let input = WheelInputs {
            abs_lock: 0.0,
            rumble_strips: 0.0,
            gear_change: false,
            surge: 0.0,
            heave: 0.0,
            wheel_slip: 0.0,
            rpm: 4000.0,
            max_rpm: 8000.0,
            traction_loss: 0.0,
            wheel_slip_min_activation: 1.0,
        };
        let result = wheel_level(
            Wheel::FrontLeft,
            &input,
            &tune,
            &HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
        )
        .unwrap();
        assert_eq!(result, 63.75);
    }

    #[test]
    fn native_engine_rpm_feed_preserves_integer_errors_and_sibling_settings() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::hapticr::NativeEngineError;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut base = plain(state.profile.read().native_tune());
        for section in ["rumble_effects", "haptic_effects"] {
            let effects = base.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap();
            effects.insert("rpm_start_fading", 73.into());
            effects.insert("rpm_stop_fading", 96.into());
            effects.insert("wheel_slip_min_activation", 17.into());
        }
        for (value, expected) in [
            (Some(LegacyValue::Int(31)), Ok(31)),
            (Some(LegacyValue::Float(31.5)), Ok(31)),
            (Some(LegacyValue::Bool(true)), Ok(1)),
            (Some(LegacyValue::Str("31".into())), Ok(31)),
            (
                Some(LegacyValue::Str("31.5".into())),
                Err(NativeEngineError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: '31.5'".into(),
                }),
            ),
            (None, Err(NativeEngineError { class: "KeyError", message: "'rpm_start_rumble'".into() })),
        ] {
            let mut row = base.clone();
            for section in ["rumble_effects", "haptic_effects"] {
                let fields = row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap();
                match &value {
                    Some(value) => {
                        fields.insert("rpm_start_rumble", value.clone());
                    }
                    None => {
                        fields.remove("rpm_start_rumble");
                    }
                }
            }
            state.profile.write().replace_native_tune(row);
            feed.sim3d(&mut |inputs| {
                for native in [
                    inputs.tuning.native_engine_rpm.as_ref().unwrap(),
                    inputs.haptic_tuning.native_engine_rpm.as_ref().unwrap(),
                ] {
                    assert_eq!(native.start, expected.clone().map(openrigcue_model::native_value::BigInt::from));
                    assert_eq!(native.fade_start, Ok(73.into()));
                    assert_eq!(native.fade_end, Ok(96.into()));
                }
                assert_eq!(
                    activation_of(inputs.tuning.native_profile.as_ref(), "rumble_effects", "wheel_slip_min_activation"),
                    17.0
                );
                assert_eq!(
                    activation_of(
                        inputs.haptic_tuning.native_profile.as_ref(),
                        "haptic_effects",
                        "wheel_slip_min_activation"
                    ),
                    17.0
                );
                None
            });
        }
    }

    #[test]
    fn native_engine_rpm_decimal_text_fails_at_reached_engine() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::hapticr::{wheel_level, Wheel, WheelInputs};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.insert("gear_shift_dim_perc", 37.into());
        fields.get_mut("haptic_effects").unwrap().as_dict_mut().unwrap().insert("rpm_start_rumble", "30.5".into());
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert_eq!(profile.tune.haptic_effects.rpm_start_rumble, 30.5);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let mut tune = native_haptic_tuning(&profile);
        tune.engine = EffectLevel { enabled: true, level: 9 };
        let input = WheelInputs {
            abs_lock: 0.0,
            rumble_strips: 0.0,
            gear_change: false,
            surge: 0.0,
            heave: 0.0,
            wheel_slip: 0.0,
            rpm: 4000.0,
            max_rpm: 8000.0,
            traction_loss: 0.0,
            wheel_slip_min_activation: 1.0,
        };
        let maxima = HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 };
        let error = wheel_level(Wheel::FrontLeft, &input, &tune, &maxima).unwrap_err();
        assert_eq!(error.to_string(), "invalid literal for int() with base 10: '30.5'");
    }

    #[test]
    fn native_activation_thresholds_change_fake_device_output_and_recover() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::{
            hapticr::HaptiCr,
            link::SerialLink,
            sim3d::{ShakeIt, Sim3d, Sim3dFans, Sim3dSwitches},
        };
        use openrigcue_model::native_value::LegacyValue;
        #[derive(Clone, Default)]
        struct Wire(Arc<Mutex<Vec<u8>>>);
        impl Wire {
            fn take_written(&self) -> Vec<u8> {
                std::mem::take(&mut *self.0.lock().unwrap())
            }
        }
        impl SerialLink for Wire {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend(bytes);
                Ok(bytes.len())
            }
            fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
                Ok(None)
            }
            fn read_available(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(vec![3, 0])
            }
            fn bytes_waiting(&mut self) -> std::io::Result<usize> {
                Ok(0)
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn SerialLink>> {
                Ok(Box::new(self.clone()))
            }
        }
        struct NoSleep;
        impl Clock for NoSleep {
            fn now(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
            fn sleep(&self, _: Duration) {
                panic!("immediate fake reply does not wait")
            }
        }
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        // The event controls also come from the profile row. Isolate wheel
        // slip there, including its level, rather than changing typed fallbacks.
        let switches = row.as_dict_mut().unwrap().get_mut("haptic_switch").unwrap().as_dict_mut().unwrap();
        for key in [
            "wheel_abs_lock",
            "wheel_gear",
            "wheel_engine",
            "wheel_rumble_strips",
            "wheel_surge_acceleration",
            "wheel_surge_deceleration",
            "wheel_heave",
            "wheel_traction_loss",
        ] {
            switches.insert(key, false.into());
        }
        row.as_dict_mut()
            .unwrap()
            .get_mut("haptic_effects")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("wheel_wheel_slip", 9.into());
        for section in ["rumble_effects", "haptic_effects"] {
            row.as_dict_mut()
                .unwrap()
                .get_mut(section)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("wheel_slip_min_activation", "30.5".into());
        }
        state.profile.write().replace_native_tune(row.clone());
        let sim_link = Wire::default();
        let haptic_link = Wire::default();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(sim_link.clone())));
        let mut haptic = HaptiCr::new(Box::new(haptic_link.clone()));
        let clock = NoSleep;
        let mut telemetry = Telemetry::off();
        telemetry.gear = 0.0;
        telemetry.wheel_slip_lf = 0.5;
        let disabled = EffectLevel { enabled: false, level: 0 };
        for expected in [127, 0] {
            if expected == 0 {
                for section in ["rumble_effects", "haptic_effects"] {
                    row.as_dict_mut()
                        .unwrap()
                        .get_mut(section)
                        .unwrap()
                        .as_dict_mut()
                        .unwrap()
                        .insert("wheel_slip_min_activation", 30.into());
                }
                state.profile.write().replace_native_tune(row.clone());
            }
            let mut sim_tune = native_sim3d_tuning(&state.profile.read());
            let seat = SeatEffects {
                wheel_slip: EffectLevel { enabled: true, level: 10 },
                rumble_strips: disabled,
                gear: disabled,
                engine: disabled,
                surge_acceleration: disabled,
                heave: disabled,
                traction_loss: disabled,
            };
            sim_tune.throttle = seat;
            sim_tune.seat = seat;
            sim_tune.clutch_gear = disabled;
            sim.send_stream(
                &telemetry,
                Sim3dFans { left: 0, center: 0, right: 0 },
                &[PortRole::Throttle; 4],
                &Sim3dSwitches { test_port: 0, on: true, game_running: true },
                &sim_tune,
                &Sim3dMaxima { surge_acceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
                &clock,
            )
            .unwrap();
            let frame = sim_link.take_written();
            assert_eq!(&frame[4..11], &[3, 86, 83, expected, expected, expected, expected]);
            let mut haptic_tune = native_haptic_tuning(&state.profile.read());
            haptic_tune.wheel_slip = EffectLevel { enabled: true, level: 9 };
            haptic_tune.abs_lock = disabled;
            haptic_tune.gear = disabled;
            haptic_tune.engine = disabled;
            haptic_tune.rumble_strips = disabled;
            haptic_tune.surge_acceleration = disabled;
            haptic_tune.surge_deceleration = disabled;
            haptic_tune.heave = disabled;
            haptic_tune.traction_loss = disabled;
            haptic
                .send_stream(
                    &telemetry,
                    &openrigcue_effects::accessories::hapticr::HapticSwitches {
                        test_mode: 0,
                        on: true,
                        game_running: true,
                    },
                    &haptic_tune,
                    &HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
                )
                .unwrap();
            let bytes: Vec<_> =
                haptic_link.take_written().into_iter().filter(|b| !matches!(*b, b',' | b';' | b'/')).collect();
            assert_eq!(bytes, vec![b'2', expected, 0, 0, 0, 0, 0, 0, 0, 0]);
        }
    }

    #[test]
    fn native_activation_threshold_feed_preserves_valid_values_and_catches_only_bad_fields() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let mut base = plain(state.profile.read().native_tune());
        for section in ["rumble_effects", "haptic_effects"] {
            base.as_dict_mut()
                .unwrap()
                .get_mut(section)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("rpm_start_fading", 73.into());
        }
        for (value, expected) in [
            (Some(LegacyValue::Int(-3)), -3),
            (Some(LegacyValue::Float(30.5)), 30),
            (Some(LegacyValue::Bool(true)), 1),
            (Some(LegacyValue::Str("30".into())), 30),
            (Some(LegacyValue::Bytes(b"30".to_vec())), 30),
            (Some(LegacyValue::Str("30.5".into())), 0),
            (Some(LegacyValue::Float(f64::NAN)), 0),
            (Some(LegacyValue::None), 0),
            (None, 0),
        ] {
            let mut row = base.clone();
            for section in ["rumble_effects", "haptic_effects"] {
                let fields = row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap();
                for key in ["wheel_slip_min_activation", "wheel_lock_min_activation", "traction_loss_min_activation"] {
                    match &value {
                        Some(value) => {
                            fields.insert(key, value.clone());
                        }
                        None => {
                            fields.remove(key);
                        }
                    }
                }
            }
            state.profile.write().replace_native_tune(row);
            feed.sim3d(&mut |inputs| {
                assert_eq!(
                    activation_of(inputs.tuning.native_profile.as_ref(), "rumble_effects", "wheel_slip_min_activation"),
                    expected as f64
                );
                assert_eq!(
                    activation_of(inputs.tuning.native_profile.as_ref(), "rumble_effects", "wheel_lock_min_activation"),
                    expected as f64
                );
                assert_eq!(
                    activation_of(
                        inputs.tuning.native_profile.as_ref(),
                        "rumble_effects",
                        "traction_loss_min_activation"
                    ),
                    expected as f64
                );
                assert_eq!(
                    activation_of(
                        inputs.haptic_tuning.native_profile.as_ref(),
                        "haptic_effects",
                        "wheel_slip_min_activation"
                    ),
                    expected as f64
                );
                assert_eq!(
                    activation_of(
                        inputs.haptic_tuning.native_profile.as_ref(),
                        "haptic_effects",
                        "traction_loss_min_activation"
                    ),
                    expected as f64
                );
                assert_eq!(inputs.tuning.rpm_start_fading, 73);
                assert_eq!(inputs.haptic_tuning.rpm_start_fading, 73);
                None
            });
        }
        let mut row = base;
        row.as_dict_mut().unwrap().insert("rumble_effects", LegacyValue::None);
        row.as_dict_mut().unwrap().remove("haptic_effects");
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert_eq!(
            activation_of(
                native_sim3d_tuning(&profile).native_profile.as_ref(),
                "rumble_effects",
                "wheel_slip_min_activation"
            ),
            0.0
        );
        assert_eq!(
            activation_of(
                native_haptic_tuning(&profile).native_profile.as_ref(),
                "haptic_effects",
                "traction_loss_min_activation"
            ),
            0.0
        );
    }

    #[test]
    fn native_activation_thresholds_use_zero_for_decimal_text() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.insert("gear_shift_dim_perc", 37.into());
        for section in ["rumble_effects", "haptic_effects"] {
            let effects = fields.get_mut(section).unwrap().as_dict_mut().unwrap();
            effects.insert("wheel_slip_min_activation", "30.5".into());
            effects.insert("traction_loss_min_activation", "40.5".into());
        }
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert_eq!(profile.tune.rumble_effects.wheel_slip_min_activation, 30.5);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        assert_eq!(
            activation_of(
                native_sim3d_tuning(&profile).native_profile.as_ref(),
                "rumble_effects",
                "wheel_slip_min_activation"
            ),
            0.0
        );
        assert_eq!(
            activation_of(
                native_haptic_tuning(&profile).native_profile.as_ref(),
                "haptic_effects",
                "traction_loss_min_activation"
            ),
            0.0
        );
    }

    #[test]
    fn native_wind_bounds_feed_keeps_raw_values_and_unrelated_projection() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        *state.telemetry_ready.write() = true;
        let mut base = plain(state.profile.read().native_tune());
        base.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        for value in [
            LegacyValue::Int(17),
            LegacyValue::Float(17.5),
            LegacyValue::Bool(true),
            LegacyValue::Str("17".into()),
            LegacyValue::None,
            LegacyValue::List(vec![1.into()]),
        ] {
            let mut row = base.clone();
            for key in [
                "power_wind_min_speed_perc",
                "power_wind_max_speed_perc",
                "power_center_wind_min_speed_perc",
                "power_center_wind_max_speed_perc",
            ] {
                row.as_dict_mut().unwrap().insert(key, value.clone());
            }
            state.profile.write().replace_native_tune(row);
            feed.intellibox(&mut |inputs, _| {
                let bounds = inputs.tuning.native_bounds.as_ref().unwrap();
                // Containers keep their object handles; compare their values.
                assert_eq!(bounds.minimum.resolved(), value);
                assert_eq!(bounds.maximum.resolved(), value);
                assert_eq!(bounds.center_minimum.resolved(), value);
                assert_eq!(bounds.center_maximum.resolved(), value);
                None
            });
            feed.sim3d(&mut |inputs| {
                assert_eq!(inputs.wind_tuning.native_bounds.as_ref().unwrap().maximum.resolved(), value);
                None
            });
            assert_eq!(state.profile.read().tune.gear_shift_dim_perc, 37.0);
        }
    }

    #[test]
    fn native_wind_maximum_text_is_not_coerced_before_multiplication() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::{WindInputs, WindSimulator};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.insert("power_wind_max_speed_perc", LegacyValue::Str("100".into()));
        fields.insert("power_wind_min_speed_perc", 17.into());
        fields.insert("speed_factor", 1.into());
        fields.insert("wind_curving", 0.into());
        fields.insert("gear_shift_dim_perc", 37.into());
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert_eq!(profile.tune.power_wind_max_speed_perc, 100.0);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let inputs = WindInputs {
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            steering_angle: 0.0,
            steering_max_angle: 450.0,
            dual_max_steering_percent: 50.0,
            left_right_mode: FanMode::Auto,
            center_mode: FanMode::Auto,
            ejection_requested: false,
        };
        let error = WindSimulator::new().fan_levels(2, 1023.0, &inputs, &native_wind_tuning(&profile)).unwrap_err();
        assert_eq!(error.to_string(), "can't multiply sequence by non-int of type 'float'");
    }

    #[test]
    fn native_wind_arguments_keep_lookup_order_and_do_not_convert_values() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::NativeWindError;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        *state.telemetry_ready.write() = true;
        let mut base = plain(state.profile.read().native_tune());
        base.as_dict_mut().unwrap().insert("gear_shift_dim_perc", 37.into());
        let keys = [
            "power_wind_min_speed_perc",
            "power_wind_max_speed_perc",
            "power_center_wind_min_speed_perc",
            "power_center_wind_max_speed_perc",
        ];
        for first_missing in 0..keys.len() {
            let mut row = base.clone();
            for key in &keys[first_missing..] {
                row.as_dict_mut().unwrap().remove(key);
            }
            state.profile.write().replace_native_tune(row);
            let expected =
                Some(Err(NativeWindError { class: "KeyError", message: format!("'{}'", keys[first_missing]) }));
            feed.intellibox(&mut |inputs, _| {
                assert_eq!(inputs.tuning.native_arguments, expected);
                None
            });
            feed.sim3d(&mut |inputs| {
                assert_eq!(inputs.wind_tuning.native_arguments, expected);
                None
            });
            assert_eq!(state.profile.read().tune.gear_shift_dim_perc, 37.0);
        }
        for value in [LegacyValue::Str("17".into()), LegacyValue::None, LegacyValue::List(vec![])] {
            let mut row = base.clone();
            for key in keys {
                row.as_dict_mut().unwrap().insert(key, value.clone());
            }
            state.profile.write().replace_native_tune(row);
            assert_eq!(native_wind_tuning(&state.profile.read()).native_arguments, Some(Ok(())));
        }
        state.profile.write().replace_native_tune(LegacyValue::None);
        assert_eq!(
            native_wind_tuning(&state.profile.read()).native_arguments,
            Some(Err(NativeWindError { class: "TypeError", message: "'NoneType' object is not subscriptable".into() }))
        );
    }

    #[test]
    fn native_wind_missing_argument_fails_before_ejection_and_factor() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::{WindError, WindInputs, WindSimulator};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.insert("enable_ejection_seat", 1.into());
        fields.insert("speed_factor", 1.into());
        fields.insert("wind_curving", 0.into());
        fields.insert("gear_shift_dim_perc", 37.into());
        state.profile.write().replace_native_tune(row.clone());
        row.as_dict_mut().unwrap().remove("power_wind_min_speed_perc");
        state.profile.write().replace_native_tune(row);
        let profile = state.profile.read();
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let inputs = WindInputs {
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            steering_angle: 0.0,
            steering_max_angle: 450.0,
            dual_max_steering_percent: 50.0,
            left_right_mode: FanMode::Off,
            center_mode: FanMode::Off,
            ejection_requested: true,
        };
        let error = WindSimulator::new().fan_levels(1, 1023.0, &inputs, &native_wind_tuning(&profile)).unwrap_err();
        assert_eq!(error.to_string(), "'power_wind_min_speed_perc'");
        assert!(!matches!(error, WindError::ZeroMaxSpeed));
    }

    #[test]
    fn native_wind_ejection_feed_preserves_str2bool_and_required_errors() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::NativeWindError;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        *state.telemetry_ready.write() = true;
        state.controls.write().canned.ejection_seat = Some(1);
        let mut base = plain(state.profile.read().native_tune());
        base.as_dict_mut().unwrap().insert("power_wind_min_speed_perc", 17.into());
        for (value, expected) in [
            (Some(LegacyValue::Bool(true)), Ok(true)),
            (Some(LegacyValue::Bool(false)), Ok(false)),
            (Some(LegacyValue::Int(1)), Ok(true)),
            (Some(LegacyValue::Int(0)), Ok(false)),
            (Some(LegacyValue::Str("YeS".into())), Ok(true)),
            (Some(LegacyValue::Str("no".into())), Ok(false)),
            (
                Some(LegacyValue::Float(1.0)),
                Err(NativeWindError {
                    class: "TypeError",
                    message: "can only concatenate str (not \"float\") to str".into(),
                }),
            ),
            (
                Some(LegacyValue::None),
                Err(NativeWindError {
                    class: "TypeError",
                    message: "can only concatenate str (not \"NoneType\") to str".into(),
                }),
            ),
            (
                Some(LegacyValue::Str("on".into())),
                Err(NativeWindError { class: "Exception", message: "Boolean value expected. Got on".into() }),
            ),
            (None, Err(NativeWindError { class: "KeyError", message: "'enable_ejection_seat'".into() })),
        ] {
            let mut row = base.clone();
            match value {
                Some(value) => {
                    row.as_dict_mut().unwrap().insert("enable_ejection_seat", value);
                }
                None => {
                    row.as_dict_mut().unwrap().remove("enable_ejection_seat");
                }
            }
            state.profile.write().replace_native_tune(row);
            feed.intellibox(&mut |inputs, _| {
                assert_eq!(inputs.tuning.native_ejection, Some(expected.clone()));
                assert_eq!(inputs.tuning.min_percent, 17.0);
                assert!(inputs.wind.ejection_requested);
                None
            });
            feed.sim3d(&mut |inputs| {
                assert_eq!(inputs.wind_tuning.native_ejection, Some(expected.clone()));
                assert_eq!(inputs.wind_tuning.min_percent, 17.0);
                assert!(inputs.wind.ejection_requested);
                None
            });
        }
        state.profile.write().replace_native_tune(LegacyValue::None);
        let expected =
            Some(Err(NativeWindError { class: "TypeError", message: "'NoneType' object is not subscriptable".into() }));
        feed.intellibox(&mut |inputs, _| {
            assert_eq!(inputs.tuning.native_ejection, expected);
            None
        });
        feed.sim3d(&mut |inputs| {
            assert_eq!(inputs.wind_tuning.native_ejection, expected);
            None
        });
        state.controls.write().canned.ejection_seat = Some(0);
        feed.intellibox(&mut |inputs, _| {
            assert!(!inputs.wind.ejection_requested);
            None
        });
    }

    #[test]
    fn native_wind_ejection_profile_float_fails_before_starting_blast() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::{WindInputs, WindSimulator};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.insert("enable_ejection_seat", LegacyValue::Float(1.0));
        fields.insert("speed_factor", 1.into());
        fields.insert("wind_curving", 0.into());
        fields.insert("power_wind_min_speed_perc", 17.into());
        fields.insert("power_wind_max_speed_perc", 100.into());
        fields.insert("gear_shift_dim_perc", 37.into());
        state.config.write().update(|config| config.profiles.set_native_game("ac", row));
        let profile = crate::profile::ActiveProfile::load_native(&state.config.read(), "ac", &0.into(), 1).unwrap();
        assert!(profile.tune.enable_ejection_seat);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let inputs = WindInputs {
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            steering_angle: 0.0,
            steering_max_angle: 450.0,
            dual_max_steering_percent: 50.0,
            left_right_mode: FanMode::Auto,
            center_mode: FanMode::Auto,
            ejection_requested: true,
        };
        let mut wind = WindSimulator::new();
        let tuning = native_wind_tuning(&profile);
        let error = wind.fan_levels(2, 1023.0, &inputs, &tuning).unwrap_err();
        assert_eq!(error.to_string(), "can only concatenate str (not \"float\") to str");
        let after = wind.fan_levels(2, 1023.0, &WindInputs { ejection_requested: false, ..inputs }, &tuning).unwrap();
        assert_eq!(after.left, 598, "failed request did not arm the blast");
    }

    #[test]
    fn native_wind_curving_feed_preserves_equality_and_required_errors() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::{NativeCurving, NativeSpeedFactor, NativeWindError};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        *state.telemetry_ready.write() = true;
        let mut base = plain(state.profile.read().native_tune());
        base.as_dict_mut().unwrap().insert("power_wind_min_speed_perc", 17.into());
        for (gate, expected) in [
            (Some(LegacyValue::Int(1)), Ok(true)),
            (Some(LegacyValue::Float(1.0)), Ok(true)),
            (Some(LegacyValue::Bool(true)), Ok(true)),
            (Some(LegacyValue::Str("1".into())), Ok(false)),
            (Some(LegacyValue::None), Ok(false)),
            (Some(LegacyValue::List(vec![1.into()])), Ok(false)),
            (None, Err(NativeWindError { class: "KeyError", message: "'wind_curving'".into() })),
        ] {
            for (factor, expected_factor) in [
                (Some(LegacyValue::Int(10)), NativeSpeedFactor::Number(10.0)),
                (Some(LegacyValue::Float(3.5)), NativeSpeedFactor::Number(3.5)),
                (Some(LegacyValue::Bool(true)), NativeSpeedFactor::Number(1.0)),
                (Some(LegacyValue::Str("10".into())), NativeSpeedFactor::Failed),
                (Some(LegacyValue::None), NativeSpeedFactor::Failed),
                (None, NativeSpeedFactor::Failed),
            ] {
                let mut row = base.clone();
                for (key, value) in [("wind_curving", gate.clone()), ("curving_effect", factor)] {
                    match value {
                        Some(value) => {
                            row.as_dict_mut().unwrap().insert(key, value);
                        }
                        None => {
                            row.as_dict_mut().unwrap().remove(key);
                        }
                    }
                }
                state.profile.write().replace_native_tune(row);
                let expected = Some(NativeCurving { enabled: expected.clone(), factor: expected_factor });
                feed.intellibox(&mut |inputs, _| {
                    assert_eq!(inputs.tuning.native_curving, expected);
                    assert_eq!(inputs.tuning.min_percent, 17.0);
                    None
                });
                feed.sim3d(&mut |inputs| {
                    assert_eq!(inputs.wind_tuning.native_curving, expected);
                    assert_eq!(inputs.wind_tuning.min_percent, 17.0);
                    None
                });
            }
        }
        state.profile.write().replace_native_tune(LegacyValue::None);
        let expected = Some(NativeCurving {
            enabled: Err(NativeWindError {
                class: "TypeError",
                message: "'NoneType' object is not subscriptable".into(),
            }),
            factor: NativeSpeedFactor::Failed,
        });
        feed.intellibox(&mut |inputs, _| {
            assert_eq!(inputs.tuning.native_curving, expected);
            None
        });
        feed.sim3d(&mut |inputs| {
            assert_eq!(inputs.wind_tuning.native_curving, expected);
            None
        });
    }

    #[test]
    fn native_wind_curving_profile_uses_raw_gate_and_local_factor() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::{WindInputs, WindSimulator};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        for (gate, factor, expected) in [
            (LegacyValue::Str("1".into()), LegacyValue::Int(10), 598),
            (LegacyValue::Int(1), LegacyValue::Str("10".into()), 324),
            (LegacyValue::Int(1), LegacyValue::Int(10), 226),
        ] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            let fields = row.as_dict_mut().unwrap();
            fields.insert("speed_factor", 1.into());
            fields.insert("power_wind_min_speed_perc", 17.into());
            fields.insert("power_wind_max_speed_perc", 100.into());
            fields.insert("power_center_wind_min_speed_perc", 17.into());
            fields.insert("power_center_wind_max_speed_perc", 100.into());
            fields.insert("wind_curving", gate.clone());
            fields.insert("curving_effect", factor.clone());
            fields.insert("gear_shift_dim_perc", 37.into());
            state.config.write().update(|config| config.profiles.set_native_game("ac", row));
            let profile = crate::profile::ActiveProfile::load_native(&state.config.read(), "ac", &0.into(), 1).unwrap();
            assert_eq!(profile.tune.wind_curving, 1);
            assert_eq!(profile.tune.curving_effect, 10.0);
            assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
            let inputs = WindInputs {
                speed_kmh: 75.0,
                max_speed_kmh: 150.0,
                steering_angle: 112.5,
                steering_max_angle: 450.0,
                dual_max_steering_percent: 50.0,
                left_right_mode: FanMode::Auto,
                center_mode: FanMode::Auto,
                ejection_requested: false,
            };
            let result = WindSimulator::new().fan_levels(2, 1023.0, &inputs, &native_wind_tuning(&profile)).unwrap();
            assert_eq!(result.left, expected, "gate {gate:?}, factor {factor:?}");
            assert_eq!((result.right, result.center), (598, 598));
        }
    }

    #[test]
    fn native_wind_factor_profile_text_uses_caught_linear_exponent() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::wind::{WindInputs, WindSimulator};
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let fields = row.as_dict_mut().unwrap();
        fields.insert("speed_factor", LegacyValue::Str("3".into()));
        fields.insert("power_wind_min_speed_perc", 0.into());
        fields.insert("power_wind_max_speed_perc", 100.into());
        fields.insert("power_center_wind_min_speed_perc", 0.into());
        fields.insert("power_center_wind_max_speed_perc", 100.into());
        fields.insert("wind_curving", 0.into());
        fields.insert("gear_shift_dim_perc", 37.into());
        state.config.write().update(|config| config.profiles.set_native_game("ac", row));
        let profile = crate::profile::ActiveProfile::load_native(&state.config.read(), "ac", &0.into(), 1).unwrap();
        assert_eq!(profile.tune.speed_factor, 3.0);
        assert_eq!(profile.tune.gear_shift_dim_perc, 37.0);
        let inputs = WindInputs {
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            steering_angle: 0.0,
            steering_max_angle: 450.0,
            dual_max_steering_percent: 50.0,
            left_right_mode: FanMode::Auto,
            center_mode: FanMode::Auto,
            ejection_requested: false,
        };
        let result = WindSimulator::new().fan_levels(2, 1023.0, &inputs, &native_wind_tuning(&profile)).unwrap();
        assert_eq!(result.left, 511, "native map rejects text and local catch uses exponent one");
    }

    #[test]
    fn native_led_always_on_color_preserves_lookup_conversion_and_model_order() {
        use openrigcue_model::native_value::{Dict, Key, LegacyValue};
        let base = Settings::default().apply_ui_patch(&serde_json::json!({"leds":{"brightness":73}})).unwrap();
        let set = |value: LegacyValue, model: Option<LegacyValue>| {
            let mut settings = base.clone();
            settings.edit_native(|root| {
                let led = root.get_mut("LED").unwrap().as_dict_mut().unwrap();
                led.insert("segment1_color", value);
                if let Some(model) = model {
                    led.insert("model", model);
                } else {
                    led.remove("model");
                }
            });
            settings
        };
        for (value, expected) in [
            (LegacyValue::from(serde_json::json!(["255", "128", "0"])), 0xff8000),
            (LegacyValue::from(serde_json::json!([255.9, 128.9, 0.0])), 0xff8000),
            (LegacyValue::Bytes(vec![255, 128, 0]), 0xff8000),
            (LegacyValue::Str("123".into()), 0x010203),
            (LegacyValue::Tuple(vec![255.into(), 128.into(), 0.into()]), 0xff8000),
            (
                LegacyValue::Dict(Dict::from_iter([
                    (Key::Int(0), 255.into()),
                    (Key::Int(1), 128.into()),
                    (Key::Int(2), 0.into()),
                ])),
                0xff8000,
            ),
        ] {
            assert_eq!(led_color_of(&set(value, None), 0), Ok(Some(expected.into())));
        }
        let color = LegacyValue::from(serde_json::json!([255, 128, 0]));
        assert_eq!(led_color_of(&set(color.clone(), Some("GRB".into())), 0), Ok(Some(0x80ff00.into())));
        for model in [LegacyValue::Str("X".into()), LegacyValue::Bytes(b"RGB".to_vec()), LegacyValue::None] {
            assert_eq!(led_color_of(&set(color.clone(), Some(model)), 0), Ok(None));
        }
        for (value, class, message) in [
            (LegacyValue::from(serde_json::json!([255, 0])), "IndexError", "list index out of range"),
            (LegacyValue::None, "TypeError", "'NoneType' object is not subscriptable"),
            (LegacyValue::Dict(Dict::new()), "KeyError", "0"),
            (
                LegacyValue::from(serde_json::json!(["bad"])),
                "ValueError",
                "invalid literal for int() with base 10: 'bad'",
            ),
            (
                LegacyValue::from(serde_json::json!([-1, 0, 0])),
                "ValueError",
                "invalid literal for int() with base 16: '0x-10000'",
            ),
        ] {
            assert_eq!(
                led_color_of(&set(value, Some("RGB".into())), 0),
                Err(NativeLedError { class, message: message.into() })
            );
        }
        assert_eq!(led_color_of(&set(LegacyValue::from(serde_json::json!([-1, 0, 0])), Some("X".into())), 0), Ok(None));
        assert_eq!(
            led_color_of(&set(LegacyValue::from(serde_json::json!(["bad"])), Some("X".into())), 0).unwrap_err().class,
            "ValueError"
        );
        let invalid = base.apply_ui_patch(&serde_json::json!({"leds":{"segments":[{"color":["bad",0,0]}]}})).unwrap();
        assert_eq!(native_led_settings(&invalid).always_on[0].as_ref().unwrap_err().class, "ValueError");
        let wide = set(LegacyValue::from(serde_json::json!([u32::MAX, 0, 0])), None);
        assert!(led_color_of(&wide, 0).is_ok(), "valid wide word retains the bounded projection");
        let mut missing = base;
        missing.edit_native(|root| {
            root.get_mut("LED").unwrap().as_dict_mut().unwrap().remove("segment1_color");
        });
        assert_eq!(
            led_color_of(&missing, 0),
            Err(NativeLedError { class: "KeyError", message: "'segment1_color'".into() })
        );
    }

    #[test]
    fn native_led_always_on_color_keeps_values_beyond_typed_bytes() {
        use openrigcue_effects::accessories::led::{LedInputs, LedStrip};
        let settings = Settings::default()
            .apply_ui_patch(&serde_json::json!({
                "leds":{"brightness":73, "segments":[{"color":[256,0,0]}]},
                "wind":{"refresh_rate":17}
            }))
            .unwrap();
        let config = led_config(&settings.leds, Some(LedByteOrder::Rgb));
        let native = native_led_settings(&settings);
        let input = LedInputs {
            rpm: 4000.0,
            max_rpm: 8000.0,
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            shift_light: false,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            mode: LedMode::AlwaysOn,
            tach_test: false,
        };
        let result = LedStrip::new().segment_frame_native(1, &config, &input, &native).unwrap();
        assert_eq!(
            result.colors,
            Some((Some(0x01000000.into()), Some(0x01000000.into()))),
            "02X is a minimum width, not a byte cast"
        );
        assert_eq!((settings.leds.brightness, settings.wind.refresh_rate), (73, 17));
    }

    #[test]
    fn native_led_flag_count_uses_signed_settings_before_initialization() {
        use openrigcue_effects::accessories::led::{LedInputs, LedStrip};
        let settings = Settings::default()
            .apply_ui_patch(&serde_json::json!({
                "leds": {"nbr_leds": -1, "brightness":73}, "wind":{"refresh_rate":17}
            }))
            .unwrap();
        let config = led_config(&settings.leds, Some(LedByteOrder::Rgb));
        let native = native_led_settings(&settings);
        let inputs = LedInputs {
            rpm: 4000.0,
            max_rpm: 8000.0,
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            shift_light: false,
            flag: 1.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            mode: LedMode::Auto,
            tach_test: false,
        };
        let frame = LedStrip::new().segment_frame_native(3, &config, &inputs, &native).unwrap();
        assert_eq!(
            frame.lit,
            openrigcue_model::native_value::BigInt::from(-1),
            "Flag count is int(native count), before the initializer's local catch"
        );
        assert_eq!(settings.leds.brightness, 73);
        assert_eq!(settings.wind.refresh_rate, 17);
    }

    #[test]
    fn native_led_flag_count_errors_follow_actual_settings_patch_and_gates() {
        use openrigcue_effects::accessories::led::{LedInputs, LedStrip};
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({
                "leds":{"brightness":73}, "wind":{"refresh_rate":17}
            }))
            .unwrap();
        let input = LedInputs {
            rpm: 4000.0,
            max_rpm: 8000.0,
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            shift_light: false,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            mode: LedMode::Auto,
            tach_test: false,
        };
        for (value, expected) in [
            (serde_json::json!(" 78 "), Ok(78)),
            (serde_json::json!(42.5), Err("ValueError")),
            (serde_json::json!(true), Err("ValueError")),
            (serde_json::json!("42.5"), Err("ValueError")),
            (serde_json::Value::Null, Err("ValueError")),
        ] {
            let settings = base.apply_ui_patch(&serde_json::json!({"leds":{"nbr_leds":value}})).unwrap();
            let native = native_led_settings(&settings);
            let config = led_config(&settings.leds, Some(LedByteOrder::Rgb));
            assert_eq!(
                native.led_count.clone().map_err(|error| error.class),
                expected.map(openrigcue_model::native_value::BigInt::from)
            );
            let result = LedStrip::new().segment_frame_native(3, &config, &input, &native);
            match expected {
                Ok(count) => assert_eq!(result.unwrap().lit, openrigcue_model::native_value::BigInt::from(count)),
                Err(class) => assert!(matches!(result,
                    Err(openrigcue_effects::accessories::led::LedError::Native(error)) if error.class == class)),
            }
            assert_eq!((settings.leds.brightness, settings.wind.refresh_rate), (73, 17));
        }
        for (value, expected) in [(LegacyValue::Float(42.5), 42), (LegacyValue::Bool(true), 1)] {
            let mut settings = base.clone();
            settings.edit_native(|root| {
                root.get_mut("LED").unwrap().as_dict_mut().unwrap().insert("nbr_leds", value);
            });
            assert_eq!(
                native_led_settings(&settings).led_count,
                Ok(openrigcue_model::native_value::BigInt::from(expected))
            );
        }
        let mut missing = base;
        missing.edit_native(|root| {
            root.get_mut("LED").unwrap().as_dict_mut().unwrap().remove("nbr_leds");
        });
        let native = native_led_settings(&missing);
        assert_eq!(native.led_count, Err(NativeLedError { class: "KeyError", message: "'nbr_leds'".into() }));
        let config = led_config(&missing.leds, Some(LedByteOrder::Rgb));
        assert!(LedStrip::new().segment_frame_native(1, &config, &input, &native).is_ok());
        assert!(LedStrip::new().segment_frame_native(3, &config, &input, &native).is_err());
        let wide = missing.apply_ui_patch(&serde_json::json!({"leds":{"nbr_leds":"9223372036854775808"}})).unwrap();
        assert!(native_led_settings(&wide).led_count.is_ok(), "valid wide integer retains bounded projection");
        assert_eq!(wide.native_snapshot().get("LED").unwrap().item("brightness"), Ok(&LegacyValue::Str("73".into())));
    }

    #[test]
    fn native_led_negative_count_hides_every_segment() {
        use openrigcue_effects::accessories::led::{LedInputs, LedStrip};
        let settings = Settings::default().apply_ui_patch(&serde_json::json!({"leds": {"nbr_segments": -1}})).unwrap();
        let config = led_config(&settings.leds, Some(LedByteOrder::Rgb));
        let inputs = LedInputs {
            rpm: 4000.0,
            max_rpm: 8000.0,
            shift_light: false,
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            mode: LedMode::Auto,
            tach_test: false,
        };
        let native = native_led_settings(&settings);
        let frame = LedStrip::new().segment_frame_native(1, &config, &inputs, &native).unwrap();
        assert_eq!(
            (frame.start, frame.end, frame.lit),
            (151.into(), 152.into(), 0.into()),
            "native int(-1) hides every segment"
        );
    }

    #[test]
    fn native_installation_reader_refreshes_required_values_without_losing_siblings() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "height": 321, "roll_factor": 82
                }}))
                .unwrap();
        });
        let reader = native_installation_reader(&state);
        for (value, expected) in
            [(serde_json::json!("H"), "H"), (serde_json::json!("V"), "V"), (serde_json::Value::Null, "None")]
        {
            state.config.write().update(|config| {
                config.settings = config
                    .settings
                    .apply_ui_patch(&serde_json::json!({"compensation": {
                        "installation": value
                    }}))
                    .unwrap();
            });
            assert_eq!(reader(), Ok(LegacyValue::from(expected)));
            assert_eq!(state.config.read().settings().compensation.height, 321.0);
            assert_eq!(state.config.read().settings().compensation.roll_factor, 82.0);
        }
        state.config.write().update(|config| {
            config.settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("installation");
            })
        });
        assert_eq!(reader(), Err(NativeCompensationError { class: "KeyError", message: "'installation'".into() }));
    }

    #[test]
    fn native_installation_bytes_h_is_not_horizontal() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("installation", LegacyValue::Bytes(b"H".to_vec()));
            })
        });
        let read = native_installation_reader(&state);
        assert_eq!(
            read(),
            Ok(LegacyValue::Bytes(b"H".to_vec())),
            "native reader preserves bytes instead of projecting them to string H"
        );
    }

    #[test]
    fn native_compensation_type_uses_exact_builtin_comparisons_and_required_lookup() {
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {
                "height": 321, "roll_factor": 82
            }}))
            .unwrap();
        for (value, expected) in [
            (LegacyValue::from("W"), CompensationKind::Sensor),
            (LegacyValue::from("V"), CompensationKind::Virtual),
            (LegacyValue::from("w"), CompensationKind::Off),
            (LegacyValue::from("virtual"), CompensationKind::Off),
            (LegacyValue::Bytes(b"W".to_vec()), CompensationKind::Off),
            (LegacyValue::None, CompensationKind::Off),
            (LegacyValue::Int(1), CompensationKind::Off),
            (LegacyValue::List(vec![LegacyValue::from("W")]), CompensationKind::Off),
        ] {
            let mut settings = base.clone();
            settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().insert("type", value);
            });
            assert_eq!(native_compensation_kind(&settings.native_snapshot()), Ok(expected));
            assert_eq!(compensation_settings(&settings).height, 321.0);
            assert_eq!(compensation_settings(&settings).roll_factor, 82);
        }
        let mut settings = base;
        settings.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("type");
        });
        assert_eq!(
            native_compensation_kind(&settings.native_snapshot()),
            Err(NativeCompensationError { class: "KeyError", message: "'type'".into() })
        );
    }

    #[test]
    fn native_compensation_unsupported_type_idles_without_replacing_last_export() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::compensation::Compensation;
        use openrigcue_model::motion_sensor::MotionSensorValues;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.motion.model = "5 Actuators [2 Front + 2 Rear + Surge]".into();
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "height": 321, "roll_factor": 82, "left_right_distance_mm": 750
                }}))
                .unwrap();
        });
        state.controls.write().switches.compensation = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut capture = Compensation::new();
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
        })
        .unwrap();
        let old = capture.pose();
        for value in [serde_json::json!("X"), serde_json::Value::Null] {
            state.config.write().update(|config| {
                config.settings = config
                    .settings
                    .apply_ui_patch(&serde_json::json!({"compensation": {
                        "sensor": value
                    }}))
                    .unwrap();
            });
            let result = feed.with_compensation_inputs(|input| {
                assert_eq!(input.settings.height, 321.0);
                assert_eq!(input.settings.roll_factor, 82);
                capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
            });
            assert_eq!(result, Ok(Duration::from_secs(1)), "unsupported raw type does not enter virtual capture");
            assert_eq!(capture.sensor_values().snapshot(), MotionSensorValues::default().snapshot());
            assert_eq!(capture.pose(), old);
        }
        state.config.write().update(|config| {
            config.settings =
                config.settings.apply_ui_patch(&serde_json::json!({"compensation": {"sensor": "V"}})).unwrap();
        });
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
        })
        .unwrap();
        let before = capture.sensor_values().snapshot();
        state.config.write().update(|config| {
            config.settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("type");
            })
        });
        assert_eq!(
            feed.with_compensation_inputs(|input| capture.cycle(
                None,
                &CompensationInputs { actuators: &positions, ..*input },
                &clock
            )),
            Err(CompensationError::NativeScalar(NativeCompensationError {
                class: "KeyError",
                message: "'type'".into()
            }))
        );
        assert_eq!(capture.sensor_values().snapshot(), before);
        assert_eq!(capture.pose(), old);
    }

    #[test]
    fn native_compensation_spacing_preserves_kinds_and_required_key() {
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {"height": 321, "roll_factor": 82}}))
            .unwrap();
        for (value, expected) in [
            (LegacyValue::Float(42.5), Ok(42)),
            (LegacyValue::Bool(true), Ok(1)),
            (LegacyValue::Bytes(b" 750 ".to_vec()), Ok(750)),
            (LegacyValue::from("-750"), Ok(-750_i128)),
            // Valid wider integers stay exact (no bounded typed projection).
            (LegacyValue::from("9223372036854775808"), Ok(i128::from(i64::MAX) + 1)),
            (
                LegacyValue::from("42.5"),
                Err(NativeCompensationError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: '42.5'".into(),
                }),
            ),
            (
                LegacyValue::None,
                Err(NativeCompensationError {
                    class: "TypeError",
                    message: "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                        .into(),
                }),
            ),
        ] {
            let mut settings = base.clone();
            settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("mm_distance_left_right", value.clone());
            });
            let raw = settings.native_snapshot();
            assert_eq!(
                native_axis_controls(&settings, &raw).actuator_spacing_mm,
                expected.clone().map(openrigcue_model::native_value::BigInt::from),
                "{value:?}"
            );
            assert_eq!(native_displacement_settings(&settings, &raw).height, Ok(321.0));
            assert_eq!(compensation_settings_from_native(&settings, &raw).roll_factor, 82);
        }
        let mut missing = base;
        missing.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("mm_distance_left_right");
        });
        assert_eq!(
            native_axis_controls(&missing, &missing.native_snapshot()).actuator_spacing_mm,
            Err(NativeCompensationError { class: "KeyError", message: "'mm_distance_left_right'".into() })
        );
    }

    #[test]
    fn native_compensation_spacing_patch_failure_preserves_last_capture_and_recovers() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::compensation::Compensation;
        use openrigcue_effects::accessories::rig_geometry::Corners;

        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.motion.model = "5 Actuators [2 Front + 2 Rear + Surge]".into();
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "left_right_distance_mm": 750, "height": 321, "pitch_factor": 71, "roll_factor": 82
                }}))
                .unwrap();
        });
        state.controls.write().switches.compensation = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let positions = ActuatorPositions {
            corners: Corners { front_left: 200.0, front_right: 100.0, rear_left: 0.0, rear_right: 0.0 },
            motors: None,
        };
        let changed = ActuatorPositions {
            corners: Corners { front_left: 400.0, front_right: 300.0, ..positions.corners },
            ..positions
        };
        // Virtual capture returns its pause; it never calls this clock's sleep.
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut capture = Compensation::new();
        feed.with_compensation_inputs(|input| {
            assert_eq!(input.rig.dof_reality, Some(false));
            assert_eq!(input.settings.height, 321.0);
            capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
        })
        .unwrap();
        let old_pose = capture.pose();
        let old_values = capture.sensor_values().snapshot();
        for (value, class, message) in [
            (serde_json::json!("bad"), "ValueError", "invalid literal for int() with base 10: 'bad'"),
            (serde_json::json!("42.5"), "ValueError", "invalid literal for int() with base 10: '42.5'"),
            (serde_json::json!(0), "ZeroDivisionError", "float division by zero"),
        ] {
            state.config.write().update(|config| {
                config.settings = config
                    .settings
                    .apply_ui_patch(&serde_json::json!({"compensation": {"left_right_distance_mm": value}}))
                    .unwrap();
            });
            let error = feed
                .with_compensation_inputs(|input| {
                    capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
                })
                .expect_err("native spacing failure precedes the first current-axis write");
            assert_eq!(
                error,
                CompensationError::NativeScalar(NativeCompensationError { class, message: message.into() })
            );
            assert_eq!(capture.pose(), old_pose);
            assert_eq!(capture.sensor_values().snapshot(), old_values);
        }
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {"left_right_distance_mm": 750}}))
                .unwrap();
        });
        feed.with_compensation_inputs(|input| {
            assert_eq!(input.settings.height, 321.0);
            capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
        })
        .unwrap();
        assert_ne!(capture.pose().pitch, old_pose.pitch);
    }

    #[test]
    fn native_compensation_controls_keep_native_kinds_and_missing_field_errors() {
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {"height": 321, "roll_factor": 82}}))
            .unwrap();
        for (patch, field, message) in [
            (serde_json::json!({"yaw": {"reverse": "maybe"}}), "reverse", "Boolean value expected. Got maybe"),
            (serde_json::json!({"swap_roll_pitch": "maybe"}), "swap", "Boolean value expected. Got maybe"),
            (serde_json::json!({"pitch": {"offset": "bad"}}), "offset", "could not convert string to float: 'bad'"),
        ] {
            let settings = base.apply_ui_patch(&serde_json::json!({"compensation": patch})).unwrap();
            let raw = settings.native_snapshot();
            let controls = native_axis_controls(&settings, &raw);
            let error = match field {
                "reverse" => controls.reverse[4].clone().unwrap_err(),
                "swap" => controls.swap_roll_pitch.unwrap_err(),
                _ => controls.offsets[0].clone().unwrap_err(),
            };
            assert_eq!(error.message, message);
            assert_eq!(native_displacement_settings(&settings, &raw).height, Ok(321.0));
            assert_eq!(compensation_settings_from_native(&settings, &raw).roll_factor, 82);
        }
        let mut settings = base;
        settings.edit_native(|root| {
            let c = root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap();
            c.insert("swap_roll_pitch", LegacyValue::Bool(true));
            let p = c.get_mut("pitch").unwrap().as_dict_mut().unwrap();
            p.insert("reverse", LegacyValue::Bool(false));
            p.insert("offset", LegacyValue::Bytes(b"12.5".to_vec()));
        });
        let controls = native_axis_controls(&settings, &settings.native_snapshot());
        assert_eq!(controls.reverse[0], Ok(false));
        assert_eq!(controls.swap_roll_pitch, Ok(true));
        assert_eq!(controls.offsets[0], Ok(12.5));
        settings.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .get_mut("pitch")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .remove("reverse");
        });
        assert_eq!(
            native_axis_controls(&settings, &settings.native_snapshot()).reverse[0],
            Err(NativeCompensationError { class: "KeyError", message: "'reverse'".into() })
        );
    }

    #[test]
    fn native_compensation_reverse_failure_is_required_even_for_yaw() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::compensation::Compensation;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.motion.model = "P6".into();
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "height": 321, "pitch_factor": 71, "roll_factor": 82, "yaw_factor": 93,
                    "yaw": {"enabled": true}
                }}))
                .unwrap();
        });
        state.controls.write().switches.compensation = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let positions = ActuatorPositions { motors: Some([600, 420, 700, 350, 510, 730]), ..Default::default() };
        let mut capture = Compensation::new();
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
        })
        .unwrap();
        let old = capture.pose();
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {"yaw": {"reverse": "maybe"}}}))
                .unwrap();
        });
        let changed = ActuatorPositions { motors: Some([630, 470, 680, 320, 560, 690]), ..positions };
        let result = feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
        });
        assert_eq!(
            result.expect_err("original reads yaw reverse even though yaw estimate does not use it").to_string(),
            "Boolean value expected. Got maybe"
        );
        assert_eq!(capture.pose(), old);
        assert_eq!(capture.sensor().yaw.value, old.yaw);
        assert_eq!(capture.sensor().yaw.calculated, old.yaw);
        assert_ne!(capture.sensor().surge.value, old.surge);
        state.controls.write().switches.compensation = false;
        assert!(
            feed.with_compensation_inputs(|input| capture.cycle(
                None,
                &CompensationInputs { actuators: &changed, ..*input },
                &clock
            ))
            .is_ok(),
            "switch off skips native reverse reads"
        );
        state.controls.write().switches.compensation = true;
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "yaw": {"reverse": false}, "swap_roll_pitch": "maybe"
                }}))
                .unwrap();
        });
        let before_swap = capture.sensor_values().snapshot();
        assert!(feed
            .with_compensation_inputs(|input| capture.cycle(
                None,
                &CompensationInputs { actuators: &changed, ..*input },
                &clock
            ))
            .is_err());
        assert_eq!(
            capture.sensor_values().snapshot(),
            before_swap,
            "bad swap stops before first current-axis publication"
        );
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "swap_roll_pitch": false, "pitch": {"offset": "bad"}
                }}))
                .unwrap();
        });
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
        })
        .unwrap();
        assert_ne!(capture.pose().yaw, old.yaw, "virtual estimate ignores bad sensor-only offset and recovers");
    }

    #[test]
    fn native_compensation_enabled_kinds_and_required_lookup_preserve_sibling_values() {
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {"height": 321, "roll_factor": 82}}))
            .unwrap();
        for (value, expected) in [
            (serde_json::json!("yes"), Ok(true)),
            (serde_json::json!(false), Ok(false)),
            (
                serde_json::json!("maybe"),
                Err(NativeCompensationError {
                    class: "Exception",
                    message: "Boolean value expected. Got maybe".into(),
                }),
            ),
        ] {
            let settings =
                base.apply_ui_patch(&serde_json::json!({"compensation": {"surge": {"enabled": value}}})).unwrap();
            let raw = settings.native_snapshot();
            assert_eq!(native_axis_enabled(&raw)[3], expected);
            assert_eq!(native_displacement_settings(&settings, &raw).height, Ok(321.0));
            assert_eq!(compensation_settings_from_native(&settings, &raw).roll_factor, 82);
        }
        for (value, class, message) in [
            (LegacyValue::None, "TypeError", "can only concatenate str (not \"NoneType\") to str"),
            (LegacyValue::Dict(Default::default()), "TypeError", "can only concatenate str (not \"dict\") to str"),
        ] {
            let mut settings = base.clone();
            settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .get_mut("surge")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("enabled", value);
            });
            assert_eq!(
                native_axis_enabled(&settings.native_snapshot())[3],
                Err(NativeCompensationError { class, message: message.into() })
            );
        }
        let mut settings = base.clone();
        settings.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .get_mut("surge")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .remove("enabled");
        });
        assert_eq!(
            native_axis_enabled(&settings.native_snapshot())[3],
            Err(NativeCompensationError { class: "KeyError", message: "'enabled'".into() })
        );
        settings.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().insert("surge", LegacyValue::None);
        });
        assert_eq!(
            native_axis_enabled(&settings.native_snapshot())[3],
            Err(NativeCompensationError {
                class: "TypeError",
                message: "'NoneType' object is not subscriptable".into()
            })
        );
    }

    #[test]
    fn native_compensation_enabled_error_follows_current_calculation_and_off_zero() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::compensation::Compensation;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.motion.model = "P6".into();
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "height": 321, "roll_factor": 82, "pitch_factor": 71, "yaw_factor": 93,
                    "yaw": {"enabled": true}, "heave": {"enabled": true}
                }}))
                .unwrap();
        });
        state.controls.write().switches.compensation = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut capture = Compensation::new();
        let positions = ActuatorPositions { motors: Some([600, 420, 700, 350, 510, 730]), ..Default::default() };
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
        })
        .unwrap();
        let old = capture.pose();
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {"surge": {"enabled": "maybe"}}}))
                .unwrap();
        });
        let changed = ActuatorPositions { motors: Some([630, 470, 680, 320, 560, 690]), ..positions };
        let result = feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
        });
        assert_eq!(
            result.expect_err("native enabled conversion cannot use the typed bool default").to_string(),
            "Boolean value expected. Got maybe"
        );
        assert_eq!(capture.pose(), old);
        assert_eq!(capture.sensor().surge.value, old.surge);
        assert_ne!(capture.sensor().surge.calculated, old.surge);
        state.controls.write().switches.compensation = false;
        assert!(feed
            .with_compensation_inputs(|input| capture.cycle(
                None,
                &CompensationInputs { actuators: &changed, ..*input },
                &clock
            ))
            .is_err());
        assert_eq!(
            capture.sensor_values().get("surge"),
            Some(openrigcue_model::native_value::LegacyValue::Int(0)),
            "off writes current zero before enabled fails"
        );
        assert_eq!(capture.sensor().yaw.value, old.yaw, "later axis is not reached");
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {"surge": {"enabled": false}}}))
                .unwrap();
        });
        state.controls.write().switches.compensation = true;
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
        })
        .unwrap();
        assert_eq!(capture.sensor_values().get("surge"), Some(openrigcue_model::native_value::LegacyValue::Int(0)));
        assert_ne!(capture.sensor().surge.calculated, 0.0);
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {"surge": {"enabled": true}}}))
                .unwrap();
        });
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
        })
        .unwrap();
        assert_eq!(capture.pose().surge, capture.sensor().surge.calculated);
    }

    #[test]
    fn native_compensation_horizontal_patch_values_keep_kinds_and_siblings() {
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {
                "height": 321, "roll_factor": 82, "surge_horizontal_stroke": 123,
                "surge_horizontal_reverse": true, "surge_horizontal_factor": 71
            }}))
            .unwrap();
        for (key, value, expected) in [
            (
                "surge_horizontal_stroke",
                serde_json::json!("12.5"),
                NativeCompensationError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: '12.5'".into(),
                },
            ),
            (
                "surge_horizontal_reverse",
                serde_json::json!("maybe"),
                NativeCompensationError { class: "Exception", message: "Boolean value expected. Got maybe".into() },
            ),
            (
                "surge_horizontal_factor",
                serde_json::json!("bad"),
                NativeCompensationError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: 'bad'".into(),
                },
            ),
        ] {
            let patch = serde_json::json!({"compensation": {key: value}});
            let settings = base.apply_ui_patch(&patch).unwrap();
            let raw = settings.native_snapshot();
            let native = native_horizontal_surge_settings(&settings, &raw);
            let error = match key {
                "surge_horizontal_stroke" => native.stroke_mm.unwrap_err(),
                "surge_horizontal_reverse" => native.reverse.unwrap_err(),
                _ => native.factor_percent.unwrap_err(),
            };
            assert_eq!(error, expected);
            assert_eq!(native_displacement_settings(&settings, &raw).height, Ok(321.0));
            assert_eq!(compensation_settings_from_native(&settings, &raw).roll_factor, 82);
        }
        let mut native_settings = base;
        native_settings.edit_native(|root| {
            use openrigcue_model::native_value::LegacyValue;
            let fields = root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap();
            fields.insert("surge_horizontal_stroke", LegacyValue::Float(42.5));
            fields.insert("surge_horizontal_factor", LegacyValue::Bytes(b"71".to_vec()));
            fields.insert("surge_horizontal_reverse", LegacyValue::Bool(false));
        });
        assert_eq!(
            native_horizontal_surge_settings(&native_settings, &native_settings.native_snapshot()),
            NativeHorizontalSurgeSettings {
                stroke_mm: Ok(42.into()),
                reverse: Ok(false),
                factor_percent: Ok(71.into())
            }
        );
        native_settings.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("surge_horizontal_reverse");
        });
        assert_eq!(
            native_horizontal_surge_settings(&native_settings, &native_settings.native_snapshot()).reverse,
            Err(NativeCompensationError { class: "KeyError", message: "'surge_horizontal_reverse'".into() })
        );
    }

    #[test]
    fn native_compensation_horizontal_stroke_fails_before_missing_stream_lookup() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::compensation::Compensation;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.motion.model = "5 Actuators [2 Front + 2 Rear + Surge]".into();
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "height": 321, "sway_factor": 61, "roll_factor": 82, "surge_horizontal_stroke": 123
                }}))
                .unwrap();
        });
        state.controls.write().switches.compensation = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut capture = Compensation::new();
        feed.with_compensation_inputs(|input| capture.cycle(None, input, &clock)).unwrap();
        let old_pose = capture.pose();
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "surge_horizontal_stroke": "bad"
                }}))
                .unwrap();
        });
        let result = feed.with_compensation_inputs(|input| {
            assert_eq!(input.rig.horizontal_surge.clone().unwrap().position, None);
            assert_eq!(input.settings.sway_factor, 61);
            capture.cycle(None, input, &clock)
        });
        assert_eq!(
            result.expect_err("native stroke int() precedes missing stream fallback").to_string(),
            "invalid literal for int() with base 10: 'bad'"
        );
        assert_eq!(capture.pose(), old_pose, "refresh/export follows failed horizontal branch");
        let mut stream: openrigcue_cueing::Positions = [0; 12];
        stream[4] = 49_152;
        state.motion.write().stream = Some(stream);
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "surge_horizontal_stroke": 123, "surge_horizontal_reverse": true, "surge_horizontal_factor": 71
                }}))
                .unwrap();
        });
        feed.with_compensation_inputs(|input| capture.cycle(None, input, &clock)).unwrap();
        let good_horizontal = capture.pose();
        assert_eq!(good_horizontal.surge, -(123.0 / 4.0) * 71.0 / 100.0);
        for (key, value, class, message) in [
            ("surge_horizontal_reverse", "maybe", "Exception", "Boolean value expected. Got maybe"),
            ("surge_horizontal_factor", "bad", "ValueError", "invalid literal for int() with base 10: 'bad'"),
        ] {
            state.config.write().update(|config| {
                config.settings = config
                    .settings
                    .apply_ui_patch(&serde_json::json!({"compensation": {
                        "surge_horizontal_stroke": 123, "surge_horizontal_reverse": true, "surge_horizontal_factor": 71
                    }}))
                    .unwrap()
                    .apply_ui_patch(&serde_json::json!({"compensation": {key: value}}))
                    .unwrap();
            });
            let result = feed.with_compensation_inputs(|input| capture.cycle(None, input, &clock));
            assert_eq!(result, Err(NativeCompensationError { class, message: message.into() }.into()));
            assert_eq!(capture.pose(), good_horizontal);
            state.motion.write().stream = None;
            assert!(
                feed.with_compensation_inputs(|input| capture.cycle(None, input, &clock)).is_ok(),
                "missing position skips {key}"
            );
            state.motion.write().stream = Some(stream);
        }
    }

    #[test]
    fn native_compensation_refresh_uses_required_raw_patch_values() {
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {
                "height": 321, "roll_factor": 82, "refresh_rate": 73
            }}))
            .unwrap();
        for (value, expected) in [
            (serde_json::json!(90), Ok(90)),
            (
                serde_json::json!(90.5),
                Err(NativeCompensationError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: '90.5'".into(),
                }),
            ),
            (serde_json::json!("٩_٠"), Ok(90)),
            (
                serde_json::json!(true),
                Err(NativeCompensationError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: 'yes'".into(),
                }),
            ),
            (serde_json::json!(0), Ok(0)),
            (serde_json::json!(-5), Ok(-5)),
            (
                serde_json::json!("90.5"),
                Err(NativeCompensationError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: '90.5'".into(),
                }),
            ),
            (
                serde_json::Value::Null,
                Err(NativeCompensationError {
                    class: "ValueError",
                    message: "invalid literal for int() with base 10: 'None'".into(),
                }),
            ),
        ] {
            let changed = base.apply_ui_patch(&serde_json::json!({"compensation": {"refresh_rate": value}})).unwrap();
            let raw = changed.native_snapshot();
            assert_eq!(
                native_compensation_integer(&raw, "refresh_rate"),
                expected.clone().map(openrigcue_model::native_value::BigInt::from),
                "{value}"
            );
            assert_eq!(native_displacement_settings(&changed, &raw).height, Ok(321.0));
            assert_eq!(compensation_settings_from_native(&changed, &raw).roll_factor, 82);
        }
        for (value, expected) in [
            (LegacyValue::Float(90.5), openrigcue_model::native_value::BigInt::from(90)),
            (LegacyValue::Bool(true), openrigcue_model::native_value::BigInt::from(1)),
            (LegacyValue::Bytes(b"90".to_vec()), openrigcue_model::native_value::BigInt::from(90)),
            // Valid wide integers stay exact, not a typed fallback.
            (LegacyValue::from("9223372036854775808"), openrigcue_model::native_value::BigInt::from(1u8) << 63usize),
            (LegacyValue::Float(1e100), LegacyValue::Float(1e100).py_integer().unwrap()),
        ] {
            let mut changed = base.clone();
            changed.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("refresh_rate", value.clone());
            });
            assert_eq!(
                native_compensation_integer(&changed.native_snapshot(), "refresh_rate"),
                Ok(expected),
                "{value:?}"
            );
        }
    }

    #[test]
    fn native_compensation_refresh_required_lookup_retains_class_and_detail() {
        use openrigcue_model::native_value::LegacyValue;
        let mut missing = Settings::default();
        missing.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("refresh_rate");
        });
        assert_eq!(
            native_compensation_integer(&missing.native_snapshot(), "refresh_rate"),
            Err(NativeCompensationError { class: "KeyError", message: "'refresh_rate'".into() })
        );
        missing.edit_native(|root| {
            root.remove("MOTION_COMPENSATION");
        });
        assert_eq!(
            native_compensation_integer(&missing.native_snapshot(), "refresh_rate"),
            Err(NativeCompensationError { class: "KeyError", message: "'MOTION_COMPENSATION'".into() })
        );
        missing.edit_native(|root| {
            root.insert("MOTION_COMPENSATION", LegacyValue::None);
        });
        assert_eq!(
            native_compensation_integer(&missing.native_snapshot(), "refresh_rate"),
            Err(NativeCompensationError {
                class: "TypeError",
                message: "'NoneType' object is not subscriptable".into()
            })
        );
        missing.edit_native(|root| {
            let mut fields = openrigcue_model::native_value::Dict::new();
            fields.insert("refresh_rate", LegacyValue::None);
            root.insert("MOTION_COMPENSATION", LegacyValue::Dict(fields));
        });
        assert_eq!(
            native_compensation_integer(&missing.native_snapshot(), "refresh_rate"),
            Err(NativeCompensationError {
                class: "TypeError",
                message: "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".into()
            })
        );
    }

    #[test]
    fn native_compensation_refresh_failure_follows_complete_capture_and_export() {
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::compensation::Compensation;

        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.motion.model = "P6".into();
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "pitch_factor": 71, "roll_factor": 82, "yaw_factor": 93,
                    "yaw": {"enabled": true}, "height": 321, "refresh_rate": 90
                }}))
                .unwrap();
        });
        state.controls.write().switches.compensation = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut capture = Compensation::new();
        let positions = ActuatorPositions { motors: Some([600, 420, 700, 350, 510, 730]), ..Default::default() };
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
        })
        .unwrap();
        let before = capture.pose();
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"compensation": {
                    "refresh_rate": "90.5"
                }}))
                .unwrap();
        });
        let changed = ActuatorPositions { motors: Some([630, 470, 680, 320, 560, 690]), ..Default::default() };
        let result = feed.with_compensation_inputs(|input| {
            assert_eq!(input.settings.roll_factor, 82, "invalid rate must not discard unrelated settings");
            capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
        });
        assert_eq!(
            result.expect_err("native int('90.5') must fail after the capture").to_string(),
            "invalid literal for int() with base 10: '90.5'"
        );
        assert_ne!(capture.pose(), before, "the new pose is exported before the refresh error");
        assert_eq!(capture.pose().pitch, capture.sensor().pitch.value);
        assert_eq!(capture.pose().roll, capture.sensor().roll.value);
        assert_eq!(capture.pose().yaw, capture.sensor().yaw.value);
        assert_eq!(capture.pose().heave, capture.sensor().heave.value);
        for axis in ["pitch", "roll", "sway", "surge", "yaw", "heave"] {
            assert!(capture.sensor_values().get(axis).is_some(), "completed axis {axis}");
        }
        for (rate, class, message) in [
            (0, "ZeroDivisionError", "float division by zero"),
            (-5, "ValueError", "sleep length must be non-negative"),
        ] {
            state.config.write().update(|config| {
                config.settings = config
                    .settings
                    .apply_ui_patch(&serde_json::json!({"compensation": {"refresh_rate": rate}}))
                    .unwrap();
            });
            let result = feed.with_compensation_inputs(|input| {
                capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
            });
            assert_eq!(result, Err(NativeCompensationError { class, message: message.into() }.into()));
        }
        state.config.write().update(|config| {
            config.settings =
                config.settings.apply_ui_patch(&serde_json::json!({"compensation": {"refresh_rate": 90}})).unwrap();
        });
        let recovered = feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &changed, ..*input }, &clock)
        });
        let nanos = if cfg!(target_os = "windows") { 11_111_200 } else { 11_111_112 };
        assert_eq!(recovered, Ok(Duration::from_nanos(nanos)));
    }

    #[test]
    fn native_compensation_height_failure_reaches_service_feed_after_prior_axes() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_effects::accessories::compensation::Compensation;
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|config| {
            config.settings.motion.model = "P6".into();
            config.settings.compensation.height = 321.0;
            config.settings.compensation.sway_factor = 50.0;
        });
        state.controls.write().switches.compensation = true;
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let feed = ServiceFeed::new(
            state.clone(),
            motion,
            Arc::new(Accessories::new(std::time::Instant::now())),
            PortCache::in_memory(),
            None,
        );
        let positions = ActuatorPositions { motors: Some([600, 420, 700, 350, 510, 730]), ..Default::default() };
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut capture = Compensation::new();
        feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
        })
        .unwrap();
        let old_pose = capture.pose();
        state.config.write().update(|config| {
            config.settings =
                config.settings.apply_ui_patch(&serde_json::json!({"compensation": {"height": "bad"}})).unwrap();
        });
        let result = feed.with_compensation_inputs(|input| {
            capture.cycle(None, &CompensationInputs { actuators: &positions, ..*input }, &clock)
        });
        assert_eq!(
            result.expect_err("native float('bad') cannot use typed height fallback").to_string(),
            "could not convert string to float: 'bad'"
        );
        assert_eq!(capture.pose(), old_pose, "export is later than the failed displacement");
    }

    #[test]
    fn native_compensation_displacement_reads_preserve_raw_kinds_and_sibling_values() {
        use super::*;
        use openrigcue_model::native_value::LegacyValue;
        let base = Settings::default()
            .apply_ui_patch(&serde_json::json!({"compensation": {
                "height": 321.0, "sway_factor": 51, "surge_factor": 73, "roll_factor": 82
            }}))
            .unwrap();
        for (value, expected, class) in [
            (LegacyValue::Int(42), Some(42), None),
            (LegacyValue::Float(42.5), Some(42), None),
            (LegacyValue::from("٤_٢"), Some(42), None),
            (LegacyValue::Bytes(b"42".to_vec()), Some(42), None),
            (LegacyValue::from("42.5"), None, Some("ValueError")),
            (LegacyValue::None, None, Some("TypeError")),
        ] {
            let mut settings = base.clone();
            settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("sway_factor", value.clone());
            });
            let raw = settings.native_snapshot();
            let native = native_displacement_settings(&settings, &raw);
            if let Some(expected) = expected {
                assert_eq!(native.sway_factor, Ok(openrigcue_model::native_value::BigInt::from(expected)), "{value:?}");
            } else {
                assert_eq!(native.sway_factor.unwrap_err().class, class.unwrap(), "{value:?}");
            }
            assert_eq!(native.height, Ok(321.0));
            assert_eq!(native.surge_factor, Ok(73.into()));
            assert_eq!(compensation_settings_from_native(&settings, &raw).roll_factor, 82);
        }
        let mut missing = base.clone();
        missing.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("height");
        });
        let native = native_displacement_settings(&missing, &missing.native_snapshot());
        assert_eq!(
            native.height.unwrap_err(),
            NativeCompensationError { class: "KeyError", message: "'height'".into() }
        );
    }

    /// The iFlag flag switches by name, the SIM3D/HaptiCR integer levels.
    #[test]
    fn tuning_conversions() {
        let tune = FactoryTunes::builtin().get("dirtrally2.0").unwrap();
        let iflag = iflag_settings(&Settings::default(), tune);
        assert_eq!(iflag.flags.get("green"), Some(&tune.iflag.green));
        assert_eq!(iflag.flags.len(), 26);
        let sim3d = sim3d_tuning(tune);
        assert_eq!(sim3d.rpm_start_rumble, py_int(tune.rumble_effects.rpm_start_rumble));
        let wind = wind_tuning(tune);
        assert_eq!(wind.curving, tune.wind_curving == 1);
    }

    /// LED settings: segment codes; the named colours keep the order `load_hex_colors` loaded, the "Always On"
    /// colour follows `LED.model` as it is now.
    #[test]
    fn led_configuration() {
        let mut leds = LedSettings::default();
        let config = led_config(&leds, Some(LedByteOrder::Rgb));
        assert_eq!(config.segments[0].kind, SegmentType::Rpm);
        assert_eq!(config.led_count, i64::from(leds.nbr_leds));
        assert_eq!(config.byte_order, Some(LedByteOrder::Rgb));
        assert_eq!(config.always_on_order, Some(LedByteOrder::Rgb));
        leds.model = "GRB".into();
        let accessories = Accessories::new(std::time::Instant::now());
        let config = led_config(&leds, accessories.hex_colors());
        assert_eq!((config.byte_order, config.always_on_order), (Some(LedByteOrder::Rgb), Some(LedByteOrder::Grb)));
        accessories.load_hex_colors(&leds.model);
        assert_eq!(led_config(&leds, accessories.hex_colors()).byte_order, Some(LedByteOrder::Grb));
        leds.model = "BGR".into();
        assert_eq!(led_config(&leds, None).always_on_order, None);
    }

    /// The SIM3D and HaptiCR streams draw only while the running listener is not 'none', 'emulator'
    /// or 'test'.
    #[test]
    fn game_listeners_for_sim3d_and_hapticr() {
        assert!(!game_listener_running(None));
        assert!(!game_listener_running(Some("emulator")));
        assert!(!game_listener_running(Some("test")));
        assert!(game_listener_running(Some("dirtrally2.0")));
    }

    /// The ping reply publishes the firmware version as a float, or integer zero if the conversion fails.
    #[test]
    fn iflag_version_starts_at_zero() {
        let accessories = Accessories::new(std::time::Instant::now());
        assert_eq!(accessory_status(&accessories).iflag_firmware, serde_json::json!(0));
        accessories.iflag.lock().unwrap().set_firmware(Some("1.4".into()));
        assert_eq!(accessory_status(&accessories).iflag_firmware, serde_json::json!(1.4));
        accessories.iflag.lock().unwrap().set_firmware(Some("invalid".into()));
        assert_eq!(accessory_status(&accessories).iflag_firmware, serde_json::json!(0));
    }

    /// The port cache: the `com_port` entry of each device and the Intellibox `com_port`, stored at once and recalled
    /// after a reload of the cache file.
    #[test]
    fn port_cache_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.db");
        let mut cache = PortCache::open(&path, None).unwrap();
        assert_eq!(cache.recall(Device::Belt), None);
        cache.remember(Device::Belt, "/dev/ttyACM3");
        cache.remember(Device::MotionCompensation, "/dev/ttyUSB0");
        cache.remember(Device::Intellibox, "/dev/ttyUSB1");
        use openrigcue_model::native_value::LegacyValue;
        let table = openrigcue_config::import::sqlitedict::read_table(&path, "cache").unwrap();
        let ports = table.get("com_port").unwrap().as_dict().unwrap();
        assert_eq!(ports.get("belt"), Some(&LegacyValue::Str("/dev/ttyACM3".into())));
        assert_eq!(ports.get("mc"), Some(&LegacyValue::Str("/dev/ttyUSB0".into())));
        assert_eq!(
            table.get("intellibox").unwrap().as_dict().unwrap().get("com_port"),
            Some(&LegacyValue::Str("/dev/ttyUSB1".into()))
        );
        assert!(!tmp.path().join(PORT_CACHE_FILE).exists());
        let reloaded = PortCache::open(&path, None).unwrap();
        assert_eq!(reloaded.recall(Device::Belt).as_deref(), Some("/dev/ttyACM3"));
        assert_eq!(reloaded.recall(Device::Intellibox).as_deref(), Some("/dev/ttyUSB1"));
        assert_eq!(reloaded.recall(Device::Sim3d), None);
        assert_eq!(reloaded.documents().as_deref(), Some(tmp.path()));
    }

    #[test]
    fn port_cache_migrates_missing_rows_and_preserves_native_opaque_values() {
        use openrigcue_config::import::sqlitedict::{self, AutocommitTable};
        use openrigcue_devices::motion::thanos::ComPortCache;
        use openrigcue_model::native_value::{Dict, Key, LegacyValue};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.db");
        let legacy = tmp.path().join(PORT_CACHE_FILE);
        let old = "[com_port]\nthanos = '/legacy/thanos'\nbelt = '/legacy/belt'\n[intellibox]\ncom_port = '/legacy/intellibox'\n";
        std::fs::write(&legacy, old).unwrap();
        let mut ports = Dict::new();
        ports.insert("belt", LegacyValue::Str("/native/belt".into()));
        ports.insert("unknown", LegacyValue::Bytes(vec![0, 255]));
        ports.insert(Key::Int(7), LegacyValue::None);
        sqlitedict::set_item(&path, "cache", "com_port", &LegacyValue::Dict(ports)).unwrap();
        sqlitedict::set_item(&path, "cache", "other", &LegacyValue::Tuple(vec![LegacyValue::Bool(true)])).unwrap();
        sqlitedict::set_item(&path, "joystick", "other", &LegacyValue::Bytes(vec![1, 255])).unwrap();
        let mut cache = PortCache::open(&path, Some(&legacy)).unwrap();
        assert_eq!(cache.recall(Device::Belt).as_deref(), Some("/native/belt"));
        assert_eq!(ComPortCache::get(&cache, "thanos"), None, "an existing native row wins as a whole");
        assert_eq!(cache.recall(Device::Intellibox).as_deref(), Some("/legacy/intellibox"));
        cache.remember(Device::IFlag, "/new/iflag");
        ComPortCache::set(&cache.clone(), "thanos", "/new/thanos");
        assert_eq!(cache.recall(Device::IFlag).as_deref(), Some("/new/iflag"));
        let table = AutocommitTable::open(&path, "cache").unwrap();
        let row = table.get("com_port").unwrap().unwrap();
        assert_eq!(row.as_dict().unwrap().get("unknown"), Some(&LegacyValue::Bytes(vec![0, 255])));
        assert_eq!(row.as_dict().unwrap().get_key(&Key::Int(7)), Some(&LegacyValue::None));
        assert_eq!(table.get("other").unwrap(), Some(LegacyValue::Tuple(vec![LegacyValue::Bool(true)])));
        assert_eq!(
            sqlitedict::read_table(&path, "joystick").unwrap().get("other"),
            Some(&LegacyValue::Bytes(vec![1, 255]))
        );
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), old, "migration and later writes never alter ports.toml");
        let fresh = PortCache::open(&tmp.path().join("fresh.db"), Some(&legacy)).unwrap();
        assert_eq!(fresh.python_str(), "{'thanos': '/legacy/thanos', 'belt': '/legacy/belt'}");
        assert!(cache.python_str().contains("'unknown': b'\\x00\\xff', 7: None"));
    }

    #[test]
    fn invalid_native_com_port_row_is_retained_when_a_scan_attempts_to_remember() {
        use openrigcue_config::import::sqlitedict::{self, AutocommitTable};
        use openrigcue_model::native_value::LegacyValue;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.db");
        sqlitedict::set_item(&path, "cache", "com_port", &LegacyValue::None).unwrap();
        let mut cache = PortCache::open(&path, None).unwrap();
        assert!(matches!(cache.lock().set_com_port("belt", "/new"), Err(PortCacheError::InvalidPorts)));
        cache.remember(Device::Belt, "/new");
        assert_eq!(cache.recall(Device::Belt), None);
        assert_eq!(AutocommitTable::open(&path, "cache").unwrap().get("com_port").unwrap(), Some(LegacyValue::None));
    }

    /// The Thanos driver and the accessories share the cache's `com_port` entry: insertion order is kept across a
    /// reload.
    #[test]
    fn port_cache_is_shared_with_thanos_in_insertion_order() {
        use openrigcue_devices::motion::thanos::ComPortCache;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.db");
        let mut cache = PortCache::open(&path, None).unwrap();
        ComPortCache::set(&cache, "thanos", "/dev/ttyUSB0");
        cache.remember(Device::Belt, "/dev/ttyACM3");
        ComPortCache::set(&cache, "thanos2", "/dev/ttyUSB1");
        let expected = "{'thanos': '/dev/ttyUSB0', 'belt': '/dev/ttyACM3', 'thanos2': '/dev/ttyUSB1'}";
        assert_eq!(cache.python_str(), expected);
        let reloaded = PortCache::open(&path, None).unwrap();
        assert_eq!(reloaded.python_str(), expected);
        assert_eq!(ComPortCache::get(&reloaded, "thanos2").as_deref(), Some("/dev/ttyUSB1"));
    }

    /// Hardware test: while a test runs every test button is disabled, so a second press does nothing and
    /// the first test restores the switches it found.
    #[test]
    fn hardware_tests_do_not_overlap() {
        let controls = Shared::new(crate::runtime::Controls::default());
        let before = controls.read().switches.led;
        let first = {
            let controls = controls.clone();
            std::thread::spawn(move || hardware_test(&controls, HardwareTest::Led))
        };
        let start = std::time::Instant::now();
        while running_hardware_test().is_none() && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::yield_now();
        }
        assert_eq!(running_hardware_test(), Some(HardwareTest::Led));
        assert!(!hardware_test(&controls, HardwareTest::Led), "the buttons are disabled");
        assert!(first.join().unwrap());
        assert_eq!(controls.read().switches.led, before);
        assert_eq!(running_hardware_test(), None);
    }

    /// Connection scan and SIM3D loop: the HaptiCR only with `APP.enable_hapticr`, which no configuration
    /// holds yet (a configuration migration sets it to 'no').
    #[test]
    fn hapticr_needs_enable_hapticr() {
        let settings = Settings::default();
        assert!(!enable_hapticr(&settings));
        assert!(!scan_config(&settings).hapticr_enabled);
    }

    /// The pose file keeps its inode and holds exactly the latest bytes.
    #[test]
    fn pose_file_is_written_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(POSE_EXPORT_FILE);
        write_in_place(&path, &[1; 48]).unwrap();
        use std::os::unix::fs::MetadataExt;
        let inode = std::fs::metadata(&path).unwrap().ino();
        write_in_place(&path, &[2; 8]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![2; 8]);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    }
}

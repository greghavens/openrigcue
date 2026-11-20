//! Accessories: wind fans, LED strip and tachometer (Intellibox), shift beep and voice prompts, belt
//! tensioners, HaptiCR, SIM3D, iFlag, the SymProjects gear indicator and the WitMotion IMU used for motion
//! compensation, the OBD2HUD Bluetooth LE head-up display, plus the dispatcher that connects and refreshes them.

pub mod alerts;
pub mod belt;
pub mod cmd_messenger;
pub mod cog_belt;
pub mod compensation;
pub mod dispatcher;
pub mod firmware;
pub mod gear_indicator;
pub mod hapticr;
pub mod hud;
pub mod iflag;
pub mod imu;
pub mod intellibox;
pub mod led;
pub mod link;
pub mod numeric;
pub mod open_gate;
pub mod port_scan;
pub mod pose_log;
pub mod rig_geometry;
pub mod serial_events;
pub mod sim3d;
pub mod status;
pub mod values;
pub mod wind;

/// The required normalization maximum; each haptic stream handles its absence locally.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeTractionMaximum {
    Missing,
    Value(openrigcue_model::native_value::LegacyValue),
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeMapMaxima {
    pub surge_acceleration: Result<f64, hapticr::NativeEngineError>,
    pub surge_deceleration: Result<f64, hapticr::NativeEngineError>,
    pub heave: Result<f64, hapticr::NativeEngineError>,
}

fn traction_ratio(
    telemetry: &openrigcue_model::Telemetry,
    legacy_maximum: f64,
    native: Option<&NativeTractionMaximum>,
    absolute_numerator: bool,
) -> f64 {
    use openrigcue_model::native_value::LegacyValue;
    let Some(native) = native else {
        let velocity = if absolute_numerator { telemetry.lateral_velocity.abs() } else { telemetry.lateral_velocity };
        return numeric::divide_or_zero(velocity, legacy_maximum);
    };
    let NativeTractionMaximum::Value(maximum) = native else { return 0.0 };
    let fallback = LegacyValue::Float(telemetry.lateral_velocity);
    let velocity = telemetry.native_value("lateral_velocity").unwrap_or(&fallback);
    let numerator = if absolute_numerator { velocity.py_abs() } else { Ok(velocity.clone()) };
    numerator.and_then(|value| value.py_truediv(maximum)).and_then(|value| value.py_as_double()).unwrap_or(0.0)
}

/// Both haptic stream senders copy these inputs and treat each missing lookup as integer zero.
/// This is a local calculation snapshot; their fallbacks never populate the shared telemetry dictionary.
fn haptic_stream_telemetry(
    telemetry: &openrigcue_model::Telemetry,
) -> std::borrow::Cow<'_, openrigcue_model::Telemetry> {
    const CHANNELS: [&str; 15] = [
        "gear",
        "wheel_slip_lf",
        "wheel_slip_rf",
        "wheel_slip_lr",
        "wheel_slip_rr",
        "rumble_pitch_lf",
        "rumble_pitch_rf",
        "rumble_pitch_lr",
        "rumble_pitch_rr",
        "rpm",
        "max_rpm",
        "abs_lock",
        "longitudinal_acceleration",
        "vertical_acceleration",
        "lateral_velocity",
    ];
    if CHANNELS.iter().all(|name| telemetry.get_assigned(name).is_some()) {
        return std::borrow::Cow::Borrowed(telemetry);
    }
    let mut local = telemetry.clone();
    for name in CHANNELS {
        if telemetry.get_assigned(name).is_none() {
            local.set_native(name, openrigcue_model::native_value::LegacyValue::Int(0));
        }
    }
    std::borrow::Cow::Owned(local)
}

fn stream_gear(telemetry: &openrigcue_model::Telemetry) -> openrigcue_model::native_value::LegacyValue {
    telemetry
        .native_value("gear")
        .cloned()
        .unwrap_or(openrigcue_model::native_value::LegacyValue::Float(telemetry.gear))
}

#[cfg(test)]
fn gear_comparison_samples() -> Vec<(openrigcue_model::native_value::LegacyValue, bool)> {
    use openrigcue_model::native_value::LegacyValue::{Bool, Bytes, Float, Int, None, Str};
    vec![
        (None, true),
        (None, false),
        (Str("3".into()), true),
        (Str("3".into()), false),
        (Bytes(b"3".to_vec()), true),
        (Bytes(b"3".to_vec()), false),
        (Bool(true), true),
        (Int(1), false),
        (Float(1.0), false),
        (Int(9_007_199_254_740_993), true),
        (Float(9_007_199_254_740_992.0), true),
        (Float(f64::NAN), true),
        (Float(f64::NAN), true),
    ]
}

#[cfg(test)]
mod traction_tests {
    use super::*;
    use openrigcue_model::{native_value::LegacyValue, Telemetry};

    #[test]
    fn native_traction_ratio_preserves_kinds_sign_and_local_catch() {
        let mut telemetry = Telemetry::off();
        telemetry.set_native("lateral_velocity", LegacyValue::Int(-5));
        for (maximum, expected) in [
            (NativeTractionMaximum::Value(LegacyValue::Int(5)), -1.0),
            (NativeTractionMaximum::Value(LegacyValue::Float(2.5)), -2.0),
            (NativeTractionMaximum::Value(LegacyValue::Bool(true)), -5.0),
            (NativeTractionMaximum::Value(LegacyValue::Int(-5)), 1.0),
            (NativeTractionMaximum::Value(LegacyValue::Int(0)), 0.0),
            (NativeTractionMaximum::Value(LegacyValue::Float(-0.0)), 0.0),
            (NativeTractionMaximum::Value(LegacyValue::Bool(false)), 0.0),
            (NativeTractionMaximum::Value(LegacyValue::from("5")), 0.0),
            (NativeTractionMaximum::Value(LegacyValue::None), 0.0),
            (NativeTractionMaximum::Value(LegacyValue::List(vec![])), 0.0),
            (NativeTractionMaximum::Missing, 0.0),
        ] {
            assert_eq!(traction_ratio(&telemetry, 99.0, Some(&maximum), false), expected);
            assert_eq!(traction_ratio(&telemetry, 99.0, Some(&maximum), true), -expected);
        }
        let maximum = NativeTractionMaximum::Value(LegacyValue::Int(5));
        telemetry.set_native("lateral_velocity", LegacyValue::from("5"));
        assert_eq!(traction_ratio(&telemetry, 99.0, Some(&maximum), false), 0.0);
        telemetry.set_native("lateral_velocity", LegacyValue::None);
        assert_eq!(traction_ratio(&telemetry, 99.0, Some(&maximum), true), 0.0);
        telemetry.set_native("lateral_velocity", LegacyValue::Float(f64::NAN));
        assert!(traction_ratio(&telemetry, 99.0, Some(&maximum), true).is_nan());
        telemetry.set_native("lateral_velocity", LegacyValue::Float(f64::INFINITY));
        assert_eq!(traction_ratio(&telemetry, 99.0, Some(&maximum), true), f64::INFINITY);
    }

    #[test]
    fn native_traction_integer_division_uses_retained_precision_and_abs_minimum() {
        let mut telemetry = Telemetry::off();
        let numerator = -9_007_199_254_740_993_i64;
        let denominator = 9_007_199_254_740_991_i64;
        telemetry.set_native("lateral_velocity", LegacyValue::Int(numerator));
        let maximum = NativeTractionMaximum::Value(LegacyValue::Int(denominator));
        let expected =
            openrigcue_model::python_numeric::int_true_divide(i128::from(numerator), i128::from(denominator)).unwrap();
        assert_eq!(traction_ratio(&telemetry, 99.0, Some(&maximum), false).to_bits(), expected.to_bits());
        assert_eq!(traction_ratio(&telemetry, 99.0, Some(&maximum), true).to_bits(), (-expected).to_bits());
        telemetry.set_native("lateral_velocity", LegacyValue::Int(i64::MIN));
        let maximum = NativeTractionMaximum::Value(LegacyValue::Int(-1));
        assert_eq!(traction_ratio(&telemetry, 99.0, Some(&maximum), true), i64::MIN as f64);
        telemetry.lateral_velocity = -2.5;
        assert_eq!(traction_ratio(&telemetry, 5.0, None, false), -0.5);
        assert_eq!(traction_ratio(&telemetry, 5.0, None, true), 0.5);
    }
}

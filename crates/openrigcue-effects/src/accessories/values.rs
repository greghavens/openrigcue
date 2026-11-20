//! Raw-value operations at accessory calculation and protocol conversion boundaries.

use openrigcue_model::{
    native_value::{BigInt, LegacyValue},
    Telemetry,
};

use super::hapticr::{EffectLevel, NativeEngineControl, NativeEngineError};

pub(crate) fn operation(error: String) -> NativeEngineError {
    let (class, message) = error.split_once(": ").unwrap_or((&error, ""));
    NativeEngineError { class: exception_class(class), message: message.to_owned() }
}

pub(crate) fn item(parent: &LegacyValue, key: &str) -> Result<LegacyValue, NativeEngineError> {
    parent.item_owned(key).map_err(|message| NativeEngineError {
        class: if parent.resolved().as_dict().is_some() { "KeyError" } else { "TypeError" },
        message,
    })
}

pub(crate) fn field(profile: &LegacyValue, section: &str, key: &str) -> Result<LegacyValue, NativeEngineError> {
    item(&item(profile, section)?, key)
}

pub(crate) fn channel(telemetry: &Telemetry, key: &str, projection: f64) -> LegacyValue {
    telemetry.native_value(key).cloned().unwrap_or(LegacyValue::Float(projection))
}

pub(crate) fn selected(
    profile: Option<&LegacyValue>,
    sections: (&str, &str),
    key: &str,
    control: Option<&NativeEngineControl>,
    effect: EffectLevel,
) -> Result<Option<LegacyValue>, NativeEngineError> {
    if let Some(profile) = profile {
        return if field(profile, sections.0, key)?.is_truthy() {
            field(profile, sections.1, key)?.py_integer().map(LegacyValue::from_integer).map(Some).map_err(operation)
        } else {
            Ok(None)
        };
    }
    control
        .map_or_else(
            || Ok(effect.enabled.then_some(effect.level).map(BigInt::from)),
            NativeEngineControl::selected_level,
        )
        .map(|level| level.map(LegacyValue::from_integer))
}

pub(crate) fn full(level: &LegacyValue, divider: i64) -> Result<f64, NativeEngineError> {
    LegacyValue::Int(255)
        .py_mul(level)
        .and_then(|v| v.py_truediv(&LegacyValue::Int(divider)))
        .and_then(|v| v.py_as_double())
        .map_err(operation)
}

pub(crate) fn scaled(value: f64, level: &LegacyValue, divider: i64) -> Result<f64, NativeEngineError> {
    LegacyValue::Float(value)
        .py_mul(level)
        .and_then(|v| v.py_truediv(&LegacyValue::Int(divider)))
        .and_then(|v| v.py_as_double())
        .map_err(operation)
}

pub fn activation(
    profile: Option<&LegacyValue>,
    section: &str,
    key: &str,
    fallback: i64,
    divisor: i64,
) -> Result<f64, NativeEngineError> {
    let value = profile
        .map_or_else(
            || Ok(LegacyValue::Int(fallback)),
            |p| field(p, section, key).and_then(|v| v.py_integer().map(LegacyValue::from_integer).map_err(operation)),
        )
        .unwrap_or(LegacyValue::Int(0));
    value.py_truediv(&LegacyValue::Int(divisor)).and_then(|v| v.py_as_double()).map_err(operation)
}

pub(crate) fn maximum(
    profile: Option<&LegacyValue>,
    key: &str,
    fallback: f64,
    legacy: Option<&Result<f64, NativeEngineError>>,
) -> Result<f64, NativeEngineError> {
    if let Some(profile) = profile {
        return field(profile, "max_telemetry_vars", key)?.py_as_double().map_err(operation);
    }
    legacy.cloned().unwrap_or(Ok(fallback))
}

/// Integer conversion followed by the firmware's PWM clipping. Finite values beyond a
/// machine integer still clip; NaN/infinity fail at the conversion, before clipping.
pub(crate) fn clipped_integer(value: f64) -> Option<i64> {
    value.is_finite().then(|| value.trunc().clamp(0.0, 255.0) as i64)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn engine(
    profile: Option<&LegacyValue>,
    section: &str,
    rpm: &LegacyValue,
    maximum: &LegacyValue,
    slider: &LegacyValue,
    divider: i64,
    defaults: [i64; 3],
    legacy: Option<&super::hapticr::NativeEngineRpm>,
) -> Result<Option<f64>, NativeEngineError> {
    let rpm = rpm.py_as_double().map_err(operation)?;
    let maximum = maximum.py_as_double().map_err(operation)?;
    if profile.is_none() && legacy.is_none() {
        return super::hapticr::engine_level_native(
            rpm,
            maximum,
            slider.py_as_double().map_err(operation)?,
            divider as f64,
            defaults[0],
            defaults[1],
            defaults[2],
        )
        .map_err(|error| match error {
            super::hapticr::EngineLevelError::Native(error) => error,
            super::hapticr::EngineLevelError::Math(_) => operation("ZeroDivisionError: float division by zero".into()),
        });
    }
    let percentage = LegacyValue::Float(super::numeric::map_clamped(rpm, 0.0, maximum, 0.0, 100.0));
    // The raw row's integer reads, or the same integer results carried without a row.
    let integer =
        |key, carried: Option<&Result<BigInt, NativeEngineError>>| -> Result<LegacyValue, NativeEngineError> {
            match (profile, carried) {
                (Some(profile), _) => {
                    field(profile, section, key)?.py_integer().map(LegacyValue::from_integer).map_err(operation)
                }
                (None, Some(carried)) => carried.clone().map(LegacyValue::from_integer),
                (None, None) => unreachable!("typed engine path returned above"),
            }
        };
    let start = integer("rpm_start_rumble", legacy.map(|n| &n.start))?;
    let fade_start = integer("rpm_start_fading", legacy.map(|n| &n.fade_start))?;
    let fade_end = integer("rpm_stop_fading", legacy.map(|n| &n.fade_end))?;
    let hundred = LegacyValue::Int(100);
    let numerator = hundred.py_sub(&fade_end).and_then(|v| LegacyValue::Int(2).py_mul(&v)).map_err(operation)?;
    let denominator = hundred.py_sub(&fade_start).map_err(operation)?;
    let floor = numerator.py_truediv(&denominator).and_then(|v| v.py_as_double()).map_err(operation)?;
    if percentage.py_lt(&start).map_err(operation)? {
        return Ok(Some(0.0));
    }
    if percentage.py_ge(&start).map_err(operation)? && percentage.py_le(&fade_start).map_err(operation)? {
        return scaled(
            super::numeric::map_clamped(percentage.py_as_double().map_err(operation)?, 0.0, 100.0, 0.0, 255.0),
            slider,
            divider,
        )
        .map(Some);
    }
    if percentage.py_gt(&fade_start).map_err(operation)? && percentage.py_le(&fade_end).map_err(operation)? {
        let value = super::numeric::map_clamped(
            percentage.py_as_double().map_err(operation)?,
            fade_end.py_as_double().map_err(operation)?,
            fade_start.py_as_double().map_err(operation)?,
            floor * 255.0,
            255.0,
        );
        return scaled(value, slider, divider).map(Some);
    }
    Ok(percentage.py_gt(&fade_end).map_err(operation)?.then_some(floor * 255.0))
}

/// The static name of the error class carried in a `Class: message` operation error.
pub fn exception_class(class: &str) -> &'static str {
    const CLASSES: [&str; 13] = [
        "TypeError",
        "OverflowError",
        "ZeroDivisionError",
        "MemoryError",
        "KeyError",
        "IndexError",
        "AttributeError",
        "RecursionError",
        "UnicodeEncodeError",
        "UnicodeDecodeError",
        "Exception",
        "OSError",
        "ValueError",
    ];
    CLASSES.into_iter().find(|candidate| *candidate == class).unwrap_or("ValueError")
}

/// Split a `Class: message` operation error into the static class name and the message.
pub fn split_exception(error: &str) -> (&'static str, String) {
    let (class, message) = error.split_once(": ").unwrap_or((error, ""));
    (exception_class(class), message.to_owned())
}

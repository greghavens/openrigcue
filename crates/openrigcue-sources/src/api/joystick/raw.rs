//! The stored-value lookups of the joystick mapping pass.
//! In particular, an unassigned joystick is None, and errors in unused fields must not end the thread.

use std::collections::BTreeMap;

use openrigcue_config::import::value::{Key, LegacyValue as Value};

use super::{AxisMode, ControllerState, Deflections, InputKind, JoystickAssignment, JoystickControl};
use crate::udp_flight::python::{parse_int_text_checked, IntTextError, PyInt};

fn field<'a>(entry: &'a Value, key: &str) -> Result<&'a Value, String> {
    let object = entry.as_dict().ok_or_else(|| format!("TypeError: joystick entry cannot be indexed by {key:?}"))?;
    object.get(key).ok_or_else(|| format!("KeyError: {key:?}"))
}

fn index(value: &Value) -> Result<Option<u32>, String> {
    match value {
        Value::Bool(value) => Ok(Some(u32::from(*value))),
        Value::Int(value) => Ok(u32::try_from(*value).ok()),
        Value::Float(value) => {
            if value.is_nan() {
                return Err("ValueError: cannot convert float NaN to integer".into());
            }
            if value.is_infinite() {
                return Err("OverflowError: cannot convert float infinity to integer".into());
            }
            let value = value.trunc();
            Ok((value >= 0.0 && value <= f64::from(u32::MAX)).then_some(value as u32))
        }
        Value::Str(value) => match parse_int_text_checked(value) {
            Ok(PyInt::Small(value)) => Ok(u32::try_from(value).ok()),
            Ok(PyInt::Large) => Ok(None),
            Err(IntTextError::Invalid) => Err(format!("ValueError: invalid literal for int(): {value:?}")),
            Err(error @ IntTextError::DigitLimit(_)) => Err(format!("ValueError: {error}")),
        },
        Value::Bytes(value) => {
            if !value.is_ascii() {
                return Err(format!("ValueError: invalid literal for int(): {value:?}"));
            }
            index(&Value::Str(String::from_utf8(value.clone()).expect("ASCII bytes")))
        }
        _ => Err("TypeError: int() argument must be a string, a bytes-like object or a real number".into()),
    }
}

pub(super) fn apply(
    name: &str,
    profile: Option<&Value>,
    pass: &BTreeMap<String, ControllerState>,
    deflections: &mut Deflections,
) -> Result<(), String> {
    let profile = profile.ok_or_else(|| format!("KeyError: {name:?}"))?;
    let entries =
        profile.as_dict().ok_or_else(|| "AttributeError: joystick profile has no attribute 'items'".to_owned())?;
    for (key, entry) in entries.iter() {
        let control = match key {
            Key::Str(key) => JoystickControl::from_profile_key(key),
            Key::Int(_) | Key::Bool(_) | Key::Float(_) | Key::None | Key::Extra(_) => None,
        };
        for (device, state) in pass {
            if field(entry, "joystick")?.as_str() != Some(device) {
                continue;
            }
            // Button events come before the axes. The trigger is converted inside each of these
            // loops, so a connected controller with no entries has not yet used trigger at all.
            for (&button, &pressed) in &state.button_events {
                if index(field(entry, "trigger")?)? == Some(button) {
                    let kind = field(entry, "type")?;
                    if kind.as_str() == Some("button") {
                        if let Some(control) = control {
                            deflections.apply_button(control, pressed);
                        }
                    }
                }
            }
            for (&axis, &value) in &state.axes {
                if index(field(entry, "trigger")?)? != Some(axis) {
                    continue;
                }
                let mode = field(entry, "mode")?;
                let (mode, kind, invert) = if mode.as_str() == Some("Single Axis") {
                    (AxisMode::Single, InputKind::Other, field(entry, "invert_axis").is_ok_and(Value::is_truthy))
                } else if mode.as_str() == Some("Dual Axis") {
                    let kind = field(entry, "type")?.as_str().map_or(InputKind::Other, InputKind::from_stored);
                    (AxisMode::Dual, kind, false)
                } else {
                    continue;
                };
                if let Some(control) = control {
                    deflections.apply_axis(
                        &JoystickAssignment { control, device: device.clone(), kind, index: axis, mode, invert },
                        value,
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn axis_pass() -> BTreeMap<String, ControllerState> {
        BTreeMap::from([("Wheel".into(), ControllerState { axes: BTreeMap::from([(0, 32767)]), ..Default::default() })])
    }

    #[test]
    fn unassigned_and_unused_fields_are_not_validated_early() {
        let mut d = Deflections::default();
        let profile = Value::from(json!({
            "front": {"joystick":null, "mode":null},
            "right": {"joystick":"Wheel", "trigger":0, "mode":"Single Axis"}
        }));
        apply("Default", Some(&profile), &axis_pass(), &mut d).unwrap();
        assert_eq!(d.roll(), 10.0, "single-axis mapping never reads type; missing invert is caught");

        let entry = Value::from(json!({"right":{}}));
        apply("Default", Some(&entry), &BTreeMap::new(), &mut d).unwrap();
        assert!(apply("Default", Some(&entry), &axis_pass(), &mut d).unwrap_err().contains("joystick"));

        let entry = Value::from(json!({"right":{"joystick":"Wheel"}}));
        let empty_controller = BTreeMap::from([("Wheel".into(), ControllerState::default())]);
        apply("Default", Some(&entry), &empty_controller, &mut d).unwrap();
        assert!(apply("Default", Some(&entry), &axis_pass(), &mut d).unwrap_err().contains("trigger"));
    }

    #[test]
    fn missing_profile_and_none_profile_raise_even_without_devices() {
        let mut d = Deflections::default();
        let pass = BTreeMap::new();
        assert!(apply("missing", None, &pass, &mut d).unwrap_err().starts_with("KeyError"));
        assert!(apply("none", Some(&Value::None), &pass, &mut d).unwrap_err().starts_with("AttributeError"));
    }

    #[test]
    fn python_int_and_truthiness_apply_without_string_or_bool_coercion() {
        let mut d = Deflections::default();
        assert_eq!(index(&Value::Str(format!("{}7", "０_".repeat(4299)))).unwrap(), Some(7));
        for value in [
            Value::Str(format!("{}7", "０_".repeat(4300))),
            Value::Bytes(format!("{}7", "0".repeat(4300)).into_bytes()),
        ] {
            assert_eq!(index(&value).unwrap_err(), "ValueError: Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit");
        }
        let profile = Value::from(json!({"right":{
            "joystick":"Wheel", "trigger":0.9, "mode":"Single Axis", "invert_axis":"False"
        }}));
        apply("Default", Some(&profile), &axis_pass(), &mut d).unwrap();
        assert!((d.roll() - 10.0 / 65535.0).abs() < 1e-12, "the nonempty string False inverts 32767 to -32767");
        for (value, expected) in [
            (Value::Bool(true), Some(1)),
            (Value::Float(-0.9), Some(0)),
            (Value::Float(-1.1), None),
            (Value::Bytes(b"  +0_1 ".to_vec()), Some(1)),
            (Value::from("١"), Some(1)),
            (Value::from("999999999999999999999999999999999999999999"), None),
        ] {
            assert_eq!(index(&value).unwrap(), expected, "{value:?}");
        }
        for value in
            [Value::None, Value::Float(f64::NAN), Value::Float(f64::INFINITY), Value::Bytes("١".as_bytes().to_vec())]
        {
            assert!(index(&value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn button_does_not_read_mode_or_invert_but_matching_axis_does() {
        let profile = Value::from(json!({"right":{"joystick":"Wheel", "trigger":true, "type":"button"}}));
        let mut pass = BTreeMap::from([(
            "Wheel".into(),
            ControllerState { button_events: BTreeMap::from([(1, true)]), ..Default::default() },
        )]);
        let mut d = Deflections::default();
        apply("Default", Some(&profile), &pass, &mut d).unwrap();
        assert_eq!(d.roll(), 32767.0);
        pass.get_mut("Wheel").unwrap().axes.insert(1, 0);
        assert!(apply("Default", Some(&profile), &pass, &mut d).unwrap_err().contains("mode"));
    }

    #[test]
    fn profile_order_and_partial_deflections_survive_a_later_exception() {
        let profile = Value::from(json!({
            "right":{"joystick":"Wheel", "trigger":0, "mode":"Single Axis"},
            "front":{"joystick":"Wheel", "trigger":null},
            "left":{"joystick":"Wheel", "trigger":0, "mode":"Single Axis"}
        }));
        let mut d = Deflections::default();
        assert!(apply("Default", Some(&profile), &axis_pass(), &mut d).is_err());
        assert_eq!(d.roll_right, 10.0, "the first stored entry ran");
        assert_eq!(d.roll_left, 0.0, "the third stored entry did not run");
    }
}

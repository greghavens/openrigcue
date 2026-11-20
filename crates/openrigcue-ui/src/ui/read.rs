//! Reading stored values for the controls and turning the browser's values into stored ones.

use openrigcue_model::native_value::LegacyValue;
use serde_json::Value;

use crate::app::MyApp;
use crate::spec::{Field, Store};

/// The stored value of a field, from the documents the main loop keeps current.
pub fn field(app: &MyApp, field: &Field) -> Value {
    let path: Vec<&str> = field.path.iter().map(String::as_str).collect();
    match field.store {
        Store::Tune => app.tune(&path).clone(),
        Store::Settings => app.setting(&path).clone(),
        Store::Controls => app.control(&path).clone(),
    }
}

/// Whether a stored value is on, read with the service's boolean grammar: true, the whole number 1 and the words
/// y, yes, t, true and 1 (any case) are on; anything else (another number, another text, nothing) is off.
pub fn truth(value: &Value) -> bool {
    crate::backend::native_bool(&LegacyValue::from(value.clone())).unwrap_or(false)
}

/// Whether a choice's value is the stored value (numbers compare by value, texts exactly).
pub fn same(choice: &Value, stored: &Value) -> bool {
    match (choice, stored) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Number(a), Value::String(b)) | (Value::String(b), Value::Number(a)) => {
            b.trim().parse::<f64>().ok() == a.as_f64()
        }
        (a, b) => a == b,
    }
}

/// The browser's text of a number as a whole number (sliders and steppers send their value as text).
pub fn int(value: &str) -> Result<i64, String> {
    let number: f64 = value.trim().parse().map_err(|_| format!("ValueError: not a number: {value:?}"))?;
    if !number.is_finite() {
        return Err(format!("ValueError: not a number: {value:?}"));
    }
    Ok(number.round() as i64)
}

/// The browser's text of a whole number, written as one (an optional sign, digits, single underscores between
/// digits, surrounding spaces); anything else, a fraction included, is refused.
pub fn exact_int(value: &str) -> Result<i64, String> {
    let refused = || format!("ValueError: invalid literal for int() with base 10: {value:?}");
    let trimmed = value.trim();
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let well_formed = !digits.is_empty()
        && digits.split('_').all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if !well_formed {
        return Err(refused());
    }
    trimmed.replace('_', "").parse().map_err(|_| refused())
}

/// The browser's text of a number as its whole part, the fraction dropped toward zero.
pub fn whole(value: &str) -> Result<i64, String> {
    let number: f64 = value.trim().parse().map_err(|_| format!("ValueError: not a number: {value:?}"))?;
    if !number.is_finite() {
        return Err(format!("ValueError: not a number: {value:?}"));
    }
    Ok(number.trunc() as i64)
}

/// The browser's text of a number as the stored kind: a whole number stays an integer.
pub fn number(value: &str) -> Result<LegacyValue, String> {
    let number: f64 = value.trim().parse().map_err(|_| format!("ValueError: not a number: {value:?}"))?;
    if !number.is_finite() {
        return Err(format!("ValueError: not a number: {value:?}"));
    }
    Ok(if number.fract() == 0.0 { LegacyValue::Int(number as i64) } else { LegacyValue::Float(number) })
}

/// The browser's text of a checkbox.
pub fn checked(value: &str) -> bool {
    matches!(value, "1" | "true" | "on")
}

/// A number box: its minimum, maximum and step, what the browser offers with them and what the box takes from
/// the browser. The browser sends the text as typed; [`NumberBox::take`] keeps it within the limits before
/// anything is stored, so every number box of the interface reads its value through it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NumberBox {
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

impl NumberBox {
    pub const fn new(min: f64, max: f64, step: f64) -> NumberBox {
        NumberBox { min, max, step }
    }

    /// The `min`, `max` and `step` attributes of the input, each with a leading space.
    pub fn attributes(&self) -> String {
        let num = super::html::num;
        format!(" min=\"{}\" max=\"{}\" step=\"{}\"", num(self.min), num(self.max), num(self.step))
    }

    /// Both limits as whole numbers, when they are.
    fn whole_limits(&self) -> Option<(i64, i64)> {
        let whole =
            |limit: f64| (limit.is_finite() && limit.fract() == 0.0 && limit.abs() < 9e15).then_some(limit as i64);
        Some((whole(self.min)?, whole(self.max)?))
    }

    /// The value `input` (the typed text, or a stored value put into the box) as the box takes it, within
    /// `min`…`max`. When the input reads as a whole number and both limits are whole, the value is that whole
    /// number brought within the limits; otherwise it is the input as a float brought within them (a NaN stays
    /// NaN). An input that is no number leaves the box at `shown`, the value it showed, which is then the value
    /// taken (not brought within the limits); when `shown` is no number either, nothing is taken.
    pub fn take(&self, input: &LegacyValue, shown: &LegacyValue) -> Result<LegacyValue, String> {
        let within = || -> Result<LegacyValue, String> {
            match (crate::backend::native_int(input), self.whole_limits()) {
                (Ok(value), Some((low, high))) => Ok(LegacyValue::Int(value.max(low).min(high))),
                _ => {
                    let value = float_of(input)?;
                    let value = if self.min > value { self.min } else { value };
                    Ok(LegacyValue::Float(if self.max < value { self.max } else { value }))
                }
            }
        };
        within().or_else(|_| match (crate::backend::native_int(shown), self.whole_limits()) {
            (Ok(value), Some(_)) => Ok(LegacyValue::Int(value)),
            _ => float_of(shown).map(LegacyValue::Float),
        })
    }

    /// [`NumberBox::take`] for the browser's text, the box showing `shown`: the text the box then shows.
    pub fn take_text(&self, typed: &str, shown: &str) -> Result<String, String> {
        self.take(&LegacyValue::Str(typed.to_owned()), &LegacyValue::Str(shown.to_owned()))
            .map(|value| box_text(&value))
    }

    /// [`NumberBox::take`] of a stored value put into the box, as the whole number a row of whole numbers stores
    /// (a fraction dropped toward zero).
    pub fn take_whole(&self, input: &LegacyValue, shown: &LegacyValue) -> Result<i64, String> {
        crate::backend::native_int(&self.take(input, shown)?)
    }
}

/// A value as a float, as the number box reads it.
fn float_of(value: &LegacyValue) -> Result<f64, String> {
    value.py_float().ok_or_else(|| match value {
        LegacyValue::Str(_) => "ValueError: invalid floating-point text".into(),
        _ => format!("TypeError: float() argument cannot be {}", value.type_name()),
    })
}

/// The text a number box shows for the value it took: a whole number as digits, a float with its fraction
/// (`12.0`, `0.5`, `nan`).
pub fn box_text(value: &LegacyValue) -> String {
    match value {
        LegacyValue::Int(value) => value.to_string(),
        LegacyValue::Float(value) => crate::remi::py_float_str(*value),
        other => openrigcue_model::python_repr::str_value(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stored_values_compare_across_number_kinds() {
        assert!(same(&json!(10), &json!(10.0)));
        assert!(same(&json!("angle"), &json!("angle")));
        assert!(same(&json!(30), &json!("30")));
        assert!(!same(&json!("rate"), &json!("angle")));
    }

    #[test]
    fn browser_values_become_stored_kinds() {
        assert_eq!(number("12").unwrap(), LegacyValue::Int(12));
        assert_eq!(number("1.5").unwrap(), LegacyValue::Float(1.5));
        assert_eq!(int("12.6").unwrap(), 13);
        assert_eq!(whole("12.6").unwrap(), 12);
        assert_eq!(whole("-12.6").unwrap(), -12);
        assert!(whole("inf").is_err());
        assert_eq!(exact_int(" 12 ").unwrap(), 12);
        assert_eq!(exact_int("-1_000").unwrap(), -1000);
        for refused in ["1.5", "2.0", "", "1__0", "_1", "x", "+"] {
            assert!(exact_int(refused).is_err(), "{refused:?}");
        }
        assert!(number("x").is_err());
        assert!(checked("1") && !checked("0"));
        assert!(truth(&json!(1)) && !truth(&json!(false)));
        for on in [json!(true), json!(1), json!("y"), json!("Yes"), json!("t"), json!("TRUE"), json!("1")] {
            assert!(truth(&on), "{on}");
        }
        for off in [json!(false), json!(0), json!(2), json!(1.0), json!("on"), json!("n"), json!(""), json!(null)] {
            assert!(!truth(&off), "{off}");
        }
    }

    /// A number box keeps a typed whole number within whole limits, a fraction within them as a float, keeps a
    /// NaN, and stays at its shown value for text that is no number.
    #[test]
    fn a_number_box_takes_typed_values_within_its_limits() {
        let port = NumberBox::new(1.0, 49152.0, 1.0);
        assert_eq!(port.attributes(), " min=\"1\" max=\"49152\" step=\"1\"");
        let take = |typed: &str| port.take_text(typed, "7408");
        assert_eq!(take("70000").unwrap(), "49152");
        assert_eq!(take("0").unwrap(), "1");
        assert_eq!(take("-5").unwrap(), "1");
        assert_eq!(take(" 2_000 ").unwrap(), "2000");
        assert_eq!(take("3.5").unwrap(), "3.5");
        assert_eq!(take("0.5").unwrap(), "1.0");
        assert_eq!(take("1e9").unwrap(), "49152.0");
        assert_eq!(take("inf").unwrap(), "49152.0");
        assert_eq!(take("-inf").unwrap(), "1.0");
        assert_eq!(take("nan").unwrap(), "nan");
        for unreadable in ["", "-", "1,5", "abc"] {
            assert_eq!(take(unreadable).unwrap(), "7408", "{unreadable:?}");
        }
        // The shown value is taken as it is, not brought within the limits.
        assert_eq!(port.take_text("", "60000").unwrap(), "60000");
        assert!(port.take_text("", "x").is_err());
        // A limit with a fraction makes every value a float.
        let fraction = NumberBox::new(0.0, 1.5, 0.1);
        assert_eq!(fraction.take_text("5", "0.5").unwrap(), "1.5");
        assert_eq!(fraction.take_text("-2", "0.5").unwrap(), "0.0");
        assert_eq!(fraction.take_text("", "0.5").unwrap(), "0.5");
        // Whole limits keep a typed whole number whole.
        assert_eq!(NumberBox::new(0.0, 1.0, 0.1).take_text("5", "0").unwrap(), "1");
        // A stored value put into the box, then stored as a whole number.
        let leds = NumberBox::new(1.0, 150.0, 1.0);
        let shown = LegacyValue::Str("1".into());
        assert_eq!(leds.take_whole(&LegacyValue::Int(500), &shown).unwrap(), 150);
        assert_eq!(leds.take_whole(&LegacyValue::Float(0.4), &shown).unwrap(), 1);
        assert_eq!(leds.take_whole(&LegacyValue::Float(37.9), &shown).unwrap(), 37);
        assert_eq!(leds.take_whole(&LegacyValue::Bool(true), &shown).unwrap(), 1);
        assert_eq!(leds.take_whole(&LegacyValue::Str("12.7".into()), &shown).unwrap(), 12);
        assert!(leds.take_whole(&LegacyValue::Str("nan".into()), &shown).is_err());
    }
}

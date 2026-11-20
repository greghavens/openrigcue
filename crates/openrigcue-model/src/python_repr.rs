//! Display text, quoted representation and telemetry cell formatting for the representable value tree.
use crate::native_value::{Key, LegacyValue};
mod nonprintable;

/// The decimal conversion digit limit applies to integer-to-text as well
/// as text-to-integer. Binary pickle integer storage has no such digit restriction.
pub fn integer_decimal(value: &crate::native_value::BigInt) -> Result<String, String> {
    const LIMIT: usize = crate::python_numeric::DEFAULT_INT_MAX_STR_DIGITS;
    let failure = || {
        format!("ValueError: Exceeds the limit ({LIMIT} digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit")
    };
    if value.bits() > (LIMIT as u64) * 4 {
        return Err(failure());
    }
    let text = value.to_string();
    if text.trim_start_matches('-').len() > LIMIT {
        return Err(failure());
    }
    Ok(text)
}

/// Quoted representation with conversion failures retained, including inside containers.
pub fn repr_checked(value: &LegacyValue) -> Result<String, String> {
    let _operation = crate::native_object::native_operation();
    fn validate(value: &LegacyValue, seen: &mut std::collections::BTreeSet<usize>) -> Result<(), String> {
        match value {
            LegacyValue::BigInt(value) => {
                integer_decimal(value)?;
            }
            LegacyValue::Object(value) => {
                if seen.insert(value.id()) {
                    validate(&value.resolved(), seen)?;
                }
            }
            LegacyValue::List(values)
            | LegacyValue::Tuple(values)
            | LegacyValue::Set(values)
            | LegacyValue::FrozenSet(values) => {
                for value in values {
                    validate(value, seen)?;
                }
            }
            LegacyValue::Dict(values) => {
                for (key, value) in values.iter_native() {
                    validate(&key, seen)?;
                    validate(value, seen)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    validate(value, &mut std::collections::BTreeSet::new())?;
    Ok(repr(value))
}

/// Display text before encoding, so a lone surrogate remains a character and a
/// wide integer's digit-limit exception occurs before the destination write.
pub fn str_native(value: &LegacyValue) -> Result<crate::native_value::PythonString, String> {
    if let Some(text) = value.python_string() {
        return Ok(text);
    }
    repr_checked(value)?;
    Ok(crate::native_value::PythonString::from(str_value(value).as_str()))
}

pub fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut result = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '\t' => result.push_str("\\t"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            c if c == quote => {
                result.push('\\');
                result.push(c);
            }
            c if nonprintable::NONPRINTABLE
                .binary_search_by(|&(low, high)| {
                    let cp = c as u32;
                    if high < cp {
                        std::cmp::Ordering::Less
                    } else if low > cp {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .is_ok() =>
            {
                let cp = c as u32;
                if cp <= 0xff {
                    result.push_str(&format!("\\x{cp:02x}"));
                } else if cp <= 0xffff {
                    result.push_str(&format!("\\u{cp:04x}"));
                } else {
                    result.push_str(&format!("\\U{cp:08x}"));
                }
            }
            c => result.push(c),
        }
    }
    result.push(quote);
    result
}

pub fn repr(value: &LegacyValue) -> String {
    let _operation = crate::native_object::native_operation();
    match value {
        LegacyValue::Object(object) => {
            if !REPRESENTING.with(|stack| stack.borrow_mut().insert(object.id())) {
                return match object.resolved() {
                    LegacyValue::List(_) => "[...]",
                    LegacyValue::Dict(_) => "{...}",
                    _ => "(...)",
                }
                .into();
            }
            let result = repr(&object.resolved());
            REPRESENTING.with(|stack| {
                stack.borrow_mut().remove(&object.id());
            });
            result
        }
        LegacyValue::Str(text) => repr_str(text),
        LegacyValue::Unicode(text) => text.repr(),
        LegacyValue::ByteArray(bytes) => format!("bytearray({})", repr(&LegacyValue::Bytes(bytes.clone()))),
        LegacyValue::MemoryView(view) => format!("<memory at {:p}>", std::sync::Arc::as_ptr(view)),
        LegacyValue::TimeDelta(value) => {
            let mut fields = Vec::new();
            if value.days != 0 {
                fields.push(format!("days={}", value.days));
            }
            if value.seconds != 0 {
                fields.push(format!("seconds={}", value.seconds));
            }
            if value.microseconds != 0 {
                fields.push(format!("microseconds={}", value.microseconds));
            }
            if fields.is_empty() {
                fields.push("0".into());
            }
            format!("datetime.timedelta({})", fields.join(", "))
        }
        LegacyValue::Global(value) => format!(
            "<class '{}{}{}'>",
            if value.module == "builtins" { "" } else { &value.module },
            if value.module == "builtins" { "" } else { "." },
            value.name
        ),
        LegacyValue::Bytes(bytes) => {
            let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') { '"' } else { '\'' };
            let mut result = format!("b{quote}");
            for &byte in bytes {
                match byte {
                    b'\n' => result.push_str("\\n"),
                    b'\r' => result.push_str("\\r"),
                    b'\t' => result.push_str("\\t"),
                    b'\\' => result.push_str("\\\\"),
                    byte if char::from(byte) == quote => {
                        result.push('\\');
                        result.push(quote);
                    }
                    32..=126 => result.push(char::from(byte)),
                    byte => result.push_str(&format!("\\x{byte:02x}")),
                }
            }
            result.push(quote);
            result
        }
        LegacyValue::List(values) => format!("[{}]", joined(values)),
        LegacyValue::Tuple(values) => format!("({}{})", joined(values), if values.len() == 1 { "," } else { "" }),
        LegacyValue::Set(values) if values.is_empty() => "set()".into(),
        LegacyValue::Set(values) => format!("{{{}}}", joined(values)),
        LegacyValue::FrozenSet(values) if values.is_empty() => "frozenset()".into(),
        LegacyValue::FrozenSet(values) => format!("frozenset({{{}}})", joined(values)),
        LegacyValue::Dict(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| {
                    let key = match key {
                        Key::Str(text) => repr_str(text),
                        Key::Int(value) => value.to_string(),
                        Key::Bool(_) | Key::Float(_) => key.to_text(),
                        Key::None => "None".into(),
                        Key::Extra(value) => repr(&value.to_value()),
                    };
                    format!("{key}: {}", repr(value))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        LegacyValue::DateTime(value) => {
            let mut fields = vec![
                u32::from(value.year),
                u32::from(value.month),
                u32::from(value.day),
                u32::from(value.hour),
                u32::from(value.minute),
            ];
            if value.second != 0 || value.microsecond != 0 {
                fields.push(u32::from(value.second));
            }
            if value.microsecond != 0 {
                fields.push(value.microsecond);
            }
            format!(
                "datetime.datetime({}{})",
                fields.iter().map(u32::to_string).collect::<Vec<_>>().join(", "),
                if value.fold { ", fold=1" } else { "" }
            )
        }
        value => value.py_str().expect("scalar value"),
    }
}
thread_local! {static REPRESENTING:std::cell::RefCell<std::collections::BTreeSet<usize>>=const{std::cell::RefCell::new(std::collections::BTreeSet::new())};}
fn joined(values: &[LegacyValue]) -> String {
    values.iter().map(repr).collect::<Vec<_>>().join(", ")
}

pub fn str_value(value: &LegacyValue) -> String {
    match value {
        LegacyValue::Object(value) => {
            let resolved = value.resolved();
            if matches!(resolved, LegacyValue::Str(_) | LegacyValue::DateTime(_) | LegacyValue::TimeDelta(_)) {
                str_value(&resolved)
            } else {
                repr(&LegacyValue::Object(value.clone()))
            }
        }
        LegacyValue::TimeDelta(value) => format!(
            "{}{}:{:02}:{:02}{}",
            if value.days != 0 {
                format!("{} day{}, ", value.days, if value.days.abs() == 1 { "" } else { "s" })
            } else {
                String::new()
            },
            value.seconds / 3600,
            (value.seconds / 60) % 60,
            value.seconds % 60,
            if value.microseconds != 0 { format!(".{:06}", value.microseconds) } else { String::new() }
        ),
        LegacyValue::Str(text) => text.clone(),
        LegacyValue::DateTime(value) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}{}",
            value.year,
            value.month,
            value.day,
            value.hour,
            value.minute,
            value.second,
            if value.microsecond == 0 { String::new() } else { format!(".{:06}", value.microsecond) }
        ),
        value => repr(value),
    }
}

/// Telemetry cell text: only floats get six decimal places.
pub fn format_value(value: &LegacyValue) -> String {
    match value {
        LegacyValue::Float(value) if value.is_nan() => "nan".into(),
        LegacyValue::Float(value) => format!("{value:.6}"),
        value => str_value(value),
    }
}

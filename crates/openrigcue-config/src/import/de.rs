//! Typed views of [`LegacyValue`] trees through serde.
//!
//! The target type drives the conversion of raw values: booleans
//! accept bools, ints (truthiness of 0/1) and yes/no-style text spellings; integers and floats
//! accept numbers and numeric strings (surrounding whitespace, a sign, and single underscores
//! between digits are accepted).

use num_traits::ToPrimitive;
use serde::de::{self, DeserializeOwned, IntoDeserializer, MapAccess, SeqAccess, Visitor};

use super::value::{py_float_repr, str2bool, Dict, LegacyValue};
use super::ImportWarning;

/// Conversion failure, with the dotted path of the offending value.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct ConvertError {
    pub path: String,
    pub message: String,
}

impl de::Error for ConvertError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        ConvertError { path: String::new(), message: msg.to_string() }
    }
}

/// Converts a legacy value tree into `T`.
pub fn from_legacy<T: DeserializeOwned>(value: &LegacyValue) -> Result<T, ConvertError> {
    T::deserialize(LegacyDeserializer { value, path: String::new() })
}

thread_local! {static CONVERTING:std::cell::RefCell<std::collections::BTreeSet<usize>>=const{std::cell::RefCell::new(std::collections::BTreeSet::new())};}
fn converting<T>(id: usize, run: impl FnOnce() -> Result<T, ConvertError>) -> Result<T, ConvertError> {
    if !CONVERTING.with(|stack| stack.borrow_mut().insert(id)) {
        return Err(ConvertError {
            path: String::new(),
            message: "RecursionError: maximum recursion depth exceeded while converting a native container".into(),
        });
    }
    let result = run();
    CONVERTING.with(|stack| {
        stack.borrow_mut().remove(&id);
    });
    result
}

/// Converts one section, keeping going when single entries are unusable: an entry that fails to
/// convert is reported and dropped, so that field keeps its default. A missing section yields the
/// defaults (reported).
pub fn convert_lenient<T: DeserializeOwned + Default>(
    section: Option<&Dict>,
    location: &str,
    warnings: &mut Vec<ImportWarning>,
) -> T {
    let Some(section) = section else {
        warnings.push(ImportWarning::new(location, "section missing; defaults used"));
        return T::default();
    };
    let mut working = section.clone();
    loop {
        match from_legacy::<T>(&LegacyValue::Dict(working.clone())) {
            Ok(value) => return value,
            Err(err) => {
                let key = err.path.split(['.', '[']).next().unwrap_or_default().to_owned();
                if key.is_empty() || working.remove(&key).is_none() {
                    warnings.push(ImportWarning::new(location, format!("{}; defaults used", err.message)));
                    return T::default();
                }
                warnings
                    .push(ImportWarning::new(format!("{location}.{key}"), format!("{}; default kept", err.message)));
            }
        }
    }
}

struct LegacyDeserializer<'a> {
    value: &'a LegacyValue,
    path: String,
}

impl<'a> LegacyDeserializer<'a> {
    fn fail(&self, message: impl Into<String>) -> ConvertError {
        ConvertError { path: self.path.clone(), message: message.into() }
    }

    fn mismatch(&self, wanted: &str) -> ConvertError {
        self.fail(format!("expected {wanted}, found {} `{}`", self.value.type_name(), self.value))
    }

    fn child(&self, segment: &str, value: &'a LegacyValue) -> LegacyDeserializer<'a> {
        let path = if self.path.is_empty() { segment.to_owned() } else { format!("{}.{segment}", self.path) };
        LegacyDeserializer { value, path }
    }

    fn as_bool(&self) -> Result<bool, ConvertError> {
        match self.value {
            LegacyValue::Object(object) => {
                LegacyDeserializer { value: &object.resolved(), path: self.path.clone() }.as_bool()
            }
            LegacyValue::Bool(b) => Ok(*b),
            LegacyValue::Int(i) => Ok(*i != 0),
            LegacyValue::BigInt(i) => Ok(i != &0.into()),
            LegacyValue::Float(f) => Ok(*f != 0.0),
            LegacyValue::Str(s) => str2bool(s).ok_or_else(|| self.mismatch("a boolean")),
            _ => Err(self.mismatch("a boolean")),
        }
    }

    fn as_float(&self) -> Result<f64, ConvertError> {
        self.value.py_float().ok_or_else(|| self.mismatch("a number"))
    }

    fn as_text(&self) -> Result<String, ConvertError> {
        self.value.py_str().ok_or_else(|| self.mismatch("a string"))
    }

    fn seq_items(&self) -> Option<&'a [LegacyValue]> {
        match self.value {
            LegacyValue::List(v) | LegacyValue::Tuple(v) | LegacyValue::Set(v) | LegacyValue::FrozenSet(v) => {
                Some(v.as_slice())
            }
            _ => None,
        }
    }
}

macro_rules! int_method {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
            let v = self.value.py_integer().map_err(|message| self.fail(message))?;
            let narrowed = <$ty>::try_from(v.clone()).map_err(|_| self.fail(format!("{v} is out of range")))?;
            visitor.$visit(narrowed)
        }
    };
}

impl<'de, 'a> de::Deserializer<'de> for LegacyDeserializer<'a> {
    type Error = ConvertError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        match self.value {
            LegacyValue::Object(object) => converting(object.id(), || {
                LegacyDeserializer { value: &object.resolved(), path: self.path }.deserialize_any(visitor)
            }),
            LegacyValue::None => visitor.visit_unit(),
            LegacyValue::Bool(b) => visitor.visit_bool(*b),
            LegacyValue::Int(i) => visitor.visit_i64(*i),
            LegacyValue::BigInt(i) => {
                if let Some(i) = i.to_i128() {
                    visitor.visit_i128(i)
                } else if let Some(i) = i.to_u128() {
                    visitor.visit_u128(i)
                } else {
                    Err(self.fail("integer is out of range for this typed view"))
                }
            }
            LegacyValue::Float(f) => visitor.visit_f64(*f),
            LegacyValue::Str(s) => visitor.visit_string(s.clone()),
            LegacyValue::Unicode(_) => Err(self.fail("UnicodeEncodeError: surrogates not allowed in a Rust text view")),
            LegacyValue::Bytes(b) | LegacyValue::ByteArray(b) => visitor.visit_byte_buf(b.clone()),
            LegacyValue::MemoryView(_) => Err(self.mismatch("a plain value")),
            LegacyValue::List(_) | LegacyValue::Tuple(_) | LegacyValue::Set(_) | LegacyValue::FrozenSet(_) => {
                self.deserialize_seq(visitor)
            }
            LegacyValue::Dict(_) => self.deserialize_map(visitor),
            LegacyValue::DateTime(_) | LegacyValue::TimeDelta(_) | LegacyValue::Global(_) => {
                Err(self.mismatch("a plain value"))
            }
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        visitor.visit_bool(self.as_bool()?)
    }

    int_method!(deserialize_i8, visit_i8, i8);
    int_method!(deserialize_i16, visit_i16, i16);
    int_method!(deserialize_i32, visit_i32, i32);
    int_method!(deserialize_i64, visit_i64, i64);
    int_method!(deserialize_u8, visit_u8, u8);
    int_method!(deserialize_u16, visit_u16, u16);
    int_method!(deserialize_u32, visit_u32, u32);
    int_method!(deserialize_u64, visit_u64, u64);

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        visitor.visit_f32(self.as_float()? as f32)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        visitor.visit_f64(self.as_float()?)
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        visitor.visit_string(self.as_text()?)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        match self.value {
            LegacyValue::Object(object) => converting(object.id(), || {
                LegacyDeserializer { value: &object.resolved(), path: self.path }.deserialize_bytes(visitor)
            }),
            LegacyValue::Bytes(b) => visitor.visit_byte_buf(b.clone()),
            _ => Err(self.mismatch("bytes")),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        match self.value {
            LegacyValue::Object(object) => converting(object.id(), || {
                LegacyDeserializer { value: &object.resolved(), path: self.path }.deserialize_option(visitor)
            }),
            LegacyValue::None => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ConvertError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ConvertError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        if let LegacyValue::Object(object) = self.value {
            return converting(object.id(), || {
                LegacyDeserializer { value: &object.resolved(), path: self.path }.deserialize_seq(visitor)
            });
        }
        match self.seq_items() {
            Some(items) => visitor.visit_seq(LegacySeq { items, index: 0, path: self.path }),
            // The INI reader yields a plain string for a one-element list written without the trailing comma.
            None if matches!(self.value, LegacyValue::Str(_)) => {
                visitor.visit_seq(LegacySeq { items: std::slice::from_ref(self.value), index: 0, path: self.path })
            }
            None => Err(self.mismatch("a list")),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, ConvertError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, ConvertError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        match self.value {
            LegacyValue::Object(object) => converting(object.id(), || {
                LegacyDeserializer { value: &object.resolved(), path: self.path }.deserialize_map(visitor)
            }),
            LegacyValue::Dict(dict) => visitor.visit_map(LegacyMap { dict, index: 0, path: self.path }),
            _ => Err(self.mismatch("a dict")),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ConvertError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ConvertError> {
        let text = self.as_text()?;
        visitor.visit_enum(text.into_deserializer()).map_err(|e: de::value::Error| self.fail(e.to_string()))
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConvertError> {
        visitor.visit_unit()
    }
}

struct LegacySeq<'a> {
    items: &'a [LegacyValue],
    index: usize,
    path: String,
}

impl<'de, 'a> SeqAccess<'de> for LegacySeq<'a> {
    type Error = ConvertError;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, ConvertError> {
        let Some(item) = self.items.get(self.index) else { return Ok(None) };
        let path = format!("{}[{}]", self.path, self.index);
        self.index += 1;
        seed.deserialize(LegacyDeserializer { value: item, path }).map(Some)
    }
}

struct LegacyMap<'a> {
    dict: &'a Dict,
    index: usize,
    path: String,
}

impl<'de, 'a> MapAccess<'de> for LegacyMap<'a> {
    type Error = ConvertError;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, ConvertError> {
        let Some((key, _)) = self.dict.iter().nth(self.index) else { return Ok(None) };
        let text = key.to_text();
        seed.deserialize(text.into_deserializer())
            .map(Some)
            .map_err(|e: de::value::Error| ConvertError { path: self.path.clone(), message: e.to_string() })
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, ConvertError> {
        let (key, value) = self.dict.iter().nth(self.index).expect("next_key_seed returned a key");
        self.index += 1;
        let parent = LegacyDeserializer { value, path: self.path.clone() };
        seed.deserialize(parent.child(&key.to_text(), value))
    }
}

/// JSON with four-space indent and sorted keys (ASCII-escaped, `", "`/`": "` separators),
/// the encoding of `.srs` profile files.
pub fn to_python_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_json(value, 0, &mut out);
    out
}

/// The profile JSON encoding over raw values, including int/float distinctions, NaN/Infinity,
/// tuples written as arrays and errors for pickle kinds JSON cannot hold.
pub fn native_to_python_json(value: &LegacyValue) -> Result<String, ConvertError> {
    let _operation = openrigcue_model::native_object::native_operation();
    let mut out = String::new();
    write_native_json(value, 0, &mut out, &mut std::collections::BTreeSet::new())?;
    Ok(out)
}

fn write_native_json(
    value: &LegacyValue,
    depth: usize,
    out: &mut String,
    seen: &mut std::collections::BTreeSet<usize>,
) -> Result<(), ConvertError> {
    match value {
        LegacyValue::Object(object) => {
            if !seen.insert(object.id()) {
                return Err(ConvertError { path: String::new(), message: "Circular reference detected".into() });
            }
            let result = write_native_json(&object.resolved(), depth, out, seen);
            seen.remove(&object.id());
            result?;
        }
        LegacyValue::None => out.push_str("null"),
        LegacyValue::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        LegacyValue::Int(value) => out.push_str(&value.to_string()),
        LegacyValue::BigInt(value) => {
            let value = value.to_string();
            let digits = value.trim_start_matches('-').len();
            if digits > openrigcue_model::python_numeric::DEFAULT_INT_MAX_STR_DIGITS {
                return Err(ConvertError{path:String::new(),message:"Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit".into()});
            }
            out.push_str(&value);
        }
        LegacyValue::Float(value) => out.push_str(&json_float(*value)),
        LegacyValue::Str(value) => write_json_string(value, out),
        LegacyValue::Unicode(value) => write_python_string(value, out),
        LegacyValue::List(values) | LegacyValue::Tuple(values) => {
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline_indent(depth + 1, out);
                write_native_json(value, depth + 1, out, seen)?;
            }
            if !values.is_empty() {
                newline_indent(depth, out);
            }
            out.push(']');
        }
        LegacyValue::Dict(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            openrigcue_model::native_key_sort::sort_by_key(&mut entries, |(key, _)| *key)
                .map_err(|message| ConvertError { path: String::new(), message })?;
            out.push('{');
            for (index, (key, value)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline_indent(depth + 1, out);
                match key.to_value().resolved() {
                    LegacyValue::Str(value) => write_json_string(&value, out),
                    LegacyValue::Unicode(value) => write_python_string(&value, out),
                    LegacyValue::Int(value) => write_json_string(&value.to_string(), out),
                    LegacyValue::BigInt(value) => write_json_string(
                        &openrigcue_model::python_repr::integer_decimal(&value)
                            .map_err(|message| ConvertError { path: String::new(), message })?,
                        out,
                    ),
                    LegacyValue::Float(value) => write_json_string(&json_float(value), out),
                    LegacyValue::Bool(value) => write_json_string(if value { "true" } else { "false" }, out),
                    LegacyValue::None => write_json_string("null", out),
                    value => {
                        return Err(ConvertError {
                            path: String::new(),
                            message: format!("keys must be str, int, float, bool or None, not {}", value.type_name()),
                        })
                    }
                }
                out.push_str(": ");
                write_native_json(value, depth + 1, out, seen)?;
            }
            if !entries.is_empty() {
                newline_indent(depth, out);
            }
            out.push('}');
        }
        value => {
            return Err(ConvertError {
                path: String::new(),
                message: format!("Object of type {} is not JSON serializable", value.type_name()),
            })
        }
    }
    Ok(())
}

fn write_python_string(value: &super::value::PythonString, out: &mut String) {
    out.push('"');
    for &cp in value.codepoints() {
        if let Some(c) = char::from_u32(cp) {
            let mut encoded = String::new();
            write_json_string(&c.to_string(), &mut encoded);
            out.push_str(&encoded[1..encoded.len() - 1]);
        } else {
            out.push_str(&format!("\\u{cp:04x}"));
        }
    }
    out.push('"');
}

fn write_json(value: &serde_json::Value, depth: usize, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => out.push_str(&i.to_string()),
            (None, Some(u)) => out.push_str(&u.to_string()),
            (None, None) => out.push_str(&json_float(n.as_f64().unwrap_or(f64::NAN))),
        },
        serde_json::Value::String(s) => write_json_string(s, out),
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline_indent(depth + 1, out);
                write_json(item, depth + 1, out);
            }
            newline_indent(depth, out);
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline_indent(depth + 1, out);
                write_json_string(key, out);
                out.push_str(": ");
                write_json(&map[key.as_str()], depth + 1, out);
            }
            newline_indent(depth, out);
            out.push('}');
        }
    }
}

fn json_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_owned()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        py_float_repr(f)
    }
}

fn newline_indent(depth: usize, out: &mut String) {
    out.push('\n');
    out.push_str(&" ".repeat(depth * 4));
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) >= 0x7f => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::value::Key;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Sample {
        flag: bool,
        count: u32,
        ratio: f64,
        name: String,
        #[serde(default)]
        missing: Option<i64>,
        colors: Vec<u8>,
    }

    fn dict(entries: Vec<(&str, LegacyValue)>) -> LegacyValue {
        LegacyValue::Dict(entries.into_iter().map(|(k, v)| (Key::from(k), v)).collect())
    }

    #[test]
    fn converts_configobj_strings_like_srs_consumers() {
        let value = dict(vec![
            ("flag", "yes".into()),
            ("count", "27".into()),
            ("ratio", "1.66".into()),
            ("name", "MP2".into()),
            ("colors", LegacyValue::List(vec!["255".into(), "0".into(), "0".into()])),
        ]);
        let s: Sample = from_legacy(&value).unwrap();
        assert_eq!(
            s,
            Sample { flag: true, count: 27, ratio: 1.66, name: "MP2".into(), missing: None, colors: vec![255, 0, 0] }
        );
    }

    #[test]
    fn converts_python_typed_values() {
        let value = dict(vec![
            ("flag", LegacyValue::Int(0)),
            ("count", LegacyValue::Float(27.0)),
            ("ratio", LegacyValue::Int(2)),
            ("name", LegacyValue::Float(15.0)),
            ("colors", LegacyValue::List(vec![LegacyValue::Int(1)])),
        ]);
        let s: Sample = from_legacy(&value).unwrap();
        assert!(!s.flag);
        assert_eq!(s.count, 27);
        assert_eq!(s.ratio, 2.0);
        assert_eq!(s.name, "15.0");
    }

    #[test]
    fn reports_path_of_bad_value() {
        let value = dict(vec![
            ("flag", "maybe".into()),
            ("count", "1".into()),
            ("ratio", "1".into()),
            ("name", "x".into()),
            ("colors", LegacyValue::List(vec![])),
        ]);
        let err = from_legacy::<Sample>(&value).unwrap_err();
        assert_eq!(err.path, "flag");
    }

    #[test]
    fn native_json_float_keys_keep_numeric_order_and_nonfinite_spelling() {
        let value = LegacyValue::Dict(
            [
                (Key::float(f64::INFINITY).unwrap(), 4.into()),
                (Key::float(1.5).unwrap(), 3.into()),
                (Key::float(-0.0).unwrap(), 2.into()),
                (Key::float(f64::NEG_INFINITY).unwrap(), 1.into()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            native_to_python_json(&value).unwrap(),
            "{\n    \"-Infinity\": 1,\n    \"-0.0\": 2,\n    \"1.5\": 3,\n    \"Infinity\": 4\n}"
        );
        for (keys, comparison) in [
            (vec![Key::float(1.5).unwrap(), Key::from("x")], "'str' and 'float'"),
            (vec![Key::from("x"), Key::float(1.5).unwrap()], "'float' and 'str'"),
        ] {
            let value = LegacyValue::Dict(keys.into_iter().map(|key| (key, 42.into())).collect());
            assert_eq!(
                native_to_python_json(&value).unwrap_err().message,
                format!("'<' not supported between instances of {comparison}")
            );
        }
    }

    #[test]
    fn native_json_boolean_keys_sort_with_integers_and_keep_json_names() {
        let value = LegacyValue::Dict(
            [
                (Key::Int(2), 20.into()),
                (Key::Bool(true), 10.into()),
                (Key::Int(-1), (-10).into()),
                (Key::Bool(false), 0.into()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            native_to_python_json(&value).unwrap(),
            "{\n    \"-1\": -10,\n    \"false\": 0,\n    \"true\": 10,\n    \"2\": 20\n}"
        );
        for (keys, comparison) in [
            (vec![Key::Bool(true), Key::from("x")], "'str' and 'bool'"),
            (vec![Key::from("x"), Key::Bool(false)], "'bool' and 'str'"),
            (vec![Key::None, Key::Bool(true)], "'bool' and 'NoneType'"),
            (vec![Key::Bool(false), Key::None], "'NoneType' and 'bool'"),
            (vec![Key::Int(0), Key::Bool(true), Key::from("x")], "'str' and 'bool'"),
            (vec![Key::Bool(true), Key::Int(0), Key::Int(2), Key::from("x")], "'str' and 'int'"),
        ] {
            let value = LegacyValue::Dict(keys.into_iter().map(|key| (key, 42.into())).collect());
            assert_eq!(
                native_to_python_json(&value).unwrap_err().message,
                format!("'<' not supported between instances of {comparison}")
            );
        }
    }

    #[test]
    fn native_json_null_key_text_and_mixed_key_order_errors() {
        let value = LegacyValue::Dict([(Key::None, 42.into())].into_iter().collect());
        assert_eq!(native_to_python_json(&value).unwrap(), "{\n    \"null\": 42\n}");
        for (keys, comparison) in [
            (vec![Key::None, Key::from("x")], "'str' and 'NoneType'"),
            (vec![Key::from("x"), Key::None], "'NoneType' and 'str'"),
            (vec![Key::None, Key::Int(1)], "'int' and 'NoneType'"),
            (vec![Key::Int(1), Key::None], "'NoneType' and 'int'"),
            (vec![Key::Int(1), Key::from("x")], "'str' and 'int'"),
            (vec![Key::from("x"), Key::Int(1)], "'int' and 'str'"),
            (vec![Key::from("x"), Key::Int(1), Key::None], "'int' and 'str'"),
        ] {
            let value = LegacyValue::Dict(keys.into_iter().map(|key| (key, 42.into())).collect());
            assert_eq!(
                native_to_python_json(&value).unwrap_err().message,
                format!("'<' not supported between instances of {comparison}")
            );
        }
    }

    #[test]
    fn python_json_matches_json_dumps_indent4_sort_keys() {
        let v: serde_json::Value = serde_json::json!({"b": 1, "a": {"x": [1, 2.5], "e": {}}, "s": "é\"", "f": 10.0});
        let expected = "{\n    \"a\": {\n        \"e\": {},\n        \"x\": [\n            1,\n            2.5\n        ]\n    },\n    \"b\": 1,\n    \"f\": 10.0,\n    \"s\": \"\\u00e9\\\"\"\n}";
        assert_eq!(to_python_json(&v), expected);
    }

    /// ASCII-escaped output writes only ' '..'~' raw: DEL and C0 controls become \u escapes,
    /// non-BMP characters surrogate pairs.
    #[test]
    fn python_json_escapes_like_ensure_ascii() {
        let v = serde_json::Value::String("\u{7f}~ \u{1f}\u{1F600}\u{08}".into());
        assert_eq!(to_python_json(&v), r#""\u007f~ \u001f\ud83d\ude00\b""#);
    }

    #[test]
    #[ignore = "requires python3 for the independent json.dumps comparison"]
    fn native_profile_json_matches_python() {
        use std::process::Command;
        let values = LegacyValue::Dict(
            [
                (Key::from("z"), LegacyValue::Tuple(vec![LegacyValue::Int(10), LegacyValue::Float(10.0)])),
                (Key::from("text"), "é\u{7f}😀".into()),
                (
                    Key::from("f"),
                    LegacyValue::List(vec![
                        LegacyValue::Float(-0.0),
                        LegacyValue::Float(f64::NAN),
                        LegacyValue::Float(f64::INFINITY),
                        LegacyValue::Float(f64::NEG_INFINITY),
                    ]),
                ),
                (Key::from("a"), LegacyValue::Dict(Dict::new())),
            ]
            .into_iter()
            .collect(),
        );
        let output = Command::new("python3").args(["-c", "import json,sys; sys.stdout.write(json.dumps({'z':(10,10.0),'text':'é\\x7f😀','f':[-0.0,float('nan'),float('inf'),float('-inf')],'a':{}},indent=4,sort_keys=True))"]).output().unwrap();
        assert!(output.status.success());
        assert_eq!(native_to_python_json(&values).unwrap(), String::from_utf8(output.stdout).unwrap());
        let output = Command::new("python3").args(["-c", "import json;\ntry: json.dumps({'x':b'1'},indent=4,sort_keys=True)\nexcept TypeError as error: print(error,end='')"]).output().unwrap();
        let invalid = LegacyValue::Dict([(Key::from("x"), LegacyValue::Bytes(b"1".to_vec()))].into_iter().collect());
        assert_eq!(native_to_python_json(&invalid).unwrap_err().message, String::from_utf8(output.stdout).unwrap());
    }
}

//! Dynamically typed values as the game configuration (a JSON document) and the configuration (INI strings)
//! hold them, with the operations the motion code applies to them: integer, float and boolean
//! conversion, arithmetic, comparisons, absolute value and item access, each failing with a typed
//! error and its text.

use std::cmp::Ordering;

use crate::error::{functions, CueError, DivisionKind};
use openrigcue_model::native_value::{Key, LegacyValue};

/// A value of the kinds a JSON document or a config value can be.
#[derive(Debug, Clone, PartialEq)]
pub enum PyValue {
    None,
    Bool(bool),
    /// A small integer. Wider integers are held exactly in [`PyValue::Native`].
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<PyValue>),
    /// A dictionary with text keys, in insertion order.
    Dict(Vec<(String, PyValue)>),
    /// An imported dictionary containing non-text keys. Values stay recursively represented
    /// so string lookups can borrow them without dropping unrelated entries.
    KeyedDict(Vec<(Key, PyValue)>),
    /// Imported value kinds outside JSON (bytes, tuple, set and datetime). They keep their type rather than
    /// coercing them into strings/lists before the motion code applies an operation.
    Native(LegacyValue),
}

impl From<&LegacyValue> for PyValue {
    fn from(value: &LegacyValue) -> Self {
        match value {
            LegacyValue::None => Self::None,
            LegacyValue::Bool(value) => Self::Bool(*value),
            LegacyValue::Int(value) => Self::Int(i128::from(*value)),
            LegacyValue::Float(value) => Self::Float(*value),
            LegacyValue::BigInt(value) => {
                let bytes = value.to_signed_bytes_le();
                if bytes.len() <= 16 {
                    let mut bits = [if bytes.last().is_some_and(|v| v & 0x80 != 0) { 255 } else { 0 }; 16];
                    bits[..bytes.len()].copy_from_slice(&bytes);
                    Self::Int(i128::from_le_bytes(bits))
                } else {
                    Self::Native(LegacyValue::BigInt(value.clone()))
                }
            }
            LegacyValue::Str(value) => Self::Str(value.clone()),
            LegacyValue::List(values) => Self::List(values.iter().map(Self::from).collect()),
            LegacyValue::Dict(values) if values.keys().all(|key| matches!(key, Key::Str(_))) => {
                Self::Dict(values.iter().map(|(key, value)| (key.to_text(), Self::from(value))).collect())
            }
            LegacyValue::Dict(values) => {
                Self::KeyedDict(values.iter().map(|(key, value)| (key.clone(), Self::from(value))).collect())
            }
            value => Self::Native(value.clone()),
        }
    }
}

impl From<f64> for PyValue {
    fn from(v: f64) -> Self {
        PyValue::Float(v)
    }
}

impl From<i64> for PyValue {
    fn from(v: i64) -> Self {
        PyValue::Int(i128::from(v))
    }
}

impl From<bool> for PyValue {
    fn from(v: bool) -> Self {
        PyValue::Bool(v)
    }
}

impl From<&str> for PyValue {
    fn from(v: &str) -> Self {
        PyValue::Str(v.to_owned())
    }
}

/// A number: a boolean counts as an integer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Number {
    Int(i128),
    Float(f64),
}

impl Number {
    pub fn to_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }

    pub fn value(self) -> PyValue {
        match self {
            Number::Int(i) => PyValue::Int(i),
            Number::Float(f) => PyValue::Float(f),
        }
    }
}

/// A binary arithmetic operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmp {
    Lt,
    Gt,
}

pub(crate) fn native_error(error: String) -> CueError {
    use crate::error::Exception;
    let (class, message) = error.split_once(": ").unwrap_or((&error, ""));
    let exception = match class {
        "TypeError" => Exception::TypeError,
        "ValueError" => Exception::ValueError,
        "OverflowError" => Exception::OverflowError,
        "ZeroDivisionError" => Exception::ZeroDivisionError,
        "KeyError" => Exception::KeyError,
        "IndexError" => Exception::IndexError,
        "MemoryError" => Exception::Named("MemoryError"),
        "RecursionError" => Exception::Named("RecursionError"),
        _ => Exception::Exception,
    };
    CueError::new(exception, if message.is_empty() && class != "MemoryError" { error.as_str() } else { message })
}

const TWO_53: i128 = 1 << 53;

fn int_overflow() -> CueError {
    CueError::overflow("integer result beyond 128 bits")
}

impl PyValue {
    /// The value in the shared model's representation; object handles are kept, not copied.
    pub fn to_native(&self) -> LegacyValue {
        match self {
            Self::None => LegacyValue::None,
            Self::Bool(v) => LegacyValue::Bool(*v),
            Self::Int(v) => LegacyValue::from_integer((*v).into()),
            Self::Float(v) => LegacyValue::Float(*v),
            Self::Str(v) => LegacyValue::Str(v.clone()),
            Self::List(v) => LegacyValue::List(v.iter().map(Self::to_native).collect()),
            Self::Dict(v) => {
                let mut d = openrigcue_model::native_value::Dict::new();
                for (k, v) in v {
                    d.insert(k.clone(), v.to_native());
                }
                LegacyValue::Dict(d)
            }
            Self::KeyedDict(v) => {
                let mut d = openrigcue_model::native_value::Dict::new();
                for (k, v) in v {
                    d.insert(k.clone(), v.to_native());
                }
                LegacyValue::Dict(d)
            }
            Self::Native(v) => v.clone(),
        }
    }
    /// A lookup in a live mutable dictionary returns its actual item handle.
    pub fn item_owned(&self, key: &str) -> Result<PyValue, CueError> {
        if let Self::Native(value) = self {
            return value.item_owned(key).map(|v| Self::from(&v)).map_err(|message| {
                if value.type_name() == "dict" {
                    CueError::new(crate::error::Exception::KeyError, message)
                } else {
                    CueError::type_error(message)
                }
            });
        }
        self.item(key).cloned()
    }
    /// The value as an integer of any size, kept exact for later arithmetic.
    pub fn to_integer_value(&self) -> Result<PyValue, CueError> {
        self.to_native().py_integer().map(LegacyValue::from_integer).map(|v| Self::from(&v)).map_err(native_error)
    }
    /// The type name used in error texts.
    pub fn type_name(&self) -> &'static str {
        match self {
            PyValue::None => "NoneType",
            PyValue::Bool(_) => "bool",
            PyValue::Int(_) => "int",
            PyValue::Float(_) => "float",
            PyValue::Str(_) => "str",
            PyValue::List(_) => "list",
            PyValue::Dict(_) | PyValue::KeyedDict(_) => "dict",
            PyValue::Native(value) => value.type_name(),
        }
    }

    pub(crate) fn number(&self) -> Option<Number> {
        match *self {
            PyValue::Bool(b) => Some(Number::Int(i128::from(b))),
            PyValue::Int(i) => Some(Number::Int(i)),
            PyValue::Float(f) => Some(Number::Float(f)),
            _ => None,
        }
    }

    /// The item under a text key.
    pub fn item(&self, key: &str) -> Result<&PyValue, CueError> {
        match self {
            PyValue::Dict(entries) => {
                entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v).ok_or_else(|| CueError::key(key))
            }
            PyValue::KeyedDict(entries) => entries
                .iter()
                .rev()
                .find(|(k, _)| matches!(k, Key::Str(value) if value == key))
                .map(|(_, v)| v)
                .ok_or_else(|| CueError::key(key)),
            PyValue::List(_) => Err(CueError::type_error("list indices must be integers or slices, not str")),
            PyValue::Str(_) => Err(CueError::type_error("string indices must be integers, not 'str'")),
            PyValue::Native(LegacyValue::Tuple(_)) => {
                Err(CueError::type_error("tuple indices must be integers or slices, not str"))
            }
            PyValue::Native(LegacyValue::Bytes(_)) => {
                Err(CueError::type_error("byte indices must be integers or slices, not str"))
            }
            other => Err(CueError::type_error(format!("'{}' object is not subscriptable", other.type_name()))),
        }
    }

    /// Equality; a comparison nested too deeply is an error.
    pub fn equals_checked(&self, other: &Self) -> Result<bool, CueError> {
        openrigcue_model::python_compare::native_equal_checked(&self.to_native(), &other.to_native())
            .map_err(native_error)
    }

    /// Equality (false when the comparison nests too deeply).
    pub fn equals(&self, other: &PyValue) -> bool {
        if matches!(self, Self::Native(_)) || matches!(other, Self::Native(_)) {
            return openrigcue_model::python_compare::native_equal(&self.to_native(), &other.to_native());
        }
        if let (Some(left), Some(right)) = (self.dictionary_len(), other.dictionary_len()) {
            if left != right {
                return false;
            }
            return match self {
                PyValue::Dict(entries) => {
                    entries.iter().all(|(key, value)| other.item(key).is_ok_and(|other| value.equals(other)))
                }
                PyValue::KeyedDict(entries) => {
                    entries.iter().all(|(key, value)| other.keyed_item(key).is_some_and(|other| value.equals(other)))
                }
                _ => unreachable!("dictionary length is only defined for dictionaries"),
            };
        }
        match (self.number(), other.number()) {
            (Some(a), Some(b)) => compare_numbers(a, b) == Some(Ordering::Equal),
            _ => match (self, other) {
                (PyValue::Str(a), PyValue::Str(b)) => a == b,
                (PyValue::None, PyValue::None) => true,
                (PyValue::Native(a), PyValue::Native(b)) => openrigcue_model::python_compare::native_equal(a, b),
                (PyValue::List(a), PyValue::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.equals(y)),
                _ => false,
            },
        }
    }

    fn dictionary_len(&self) -> Option<usize> {
        match self {
            PyValue::Dict(entries) => Some(entries.len()),
            PyValue::KeyedDict(entries) => Some(entries.len()),
            _ => None,
        }
    }

    fn keyed_item(&self, key: &Key) -> Option<&PyValue> {
        match (self, key) {
            (PyValue::Dict(_), Key::Str(key)) => self.item(key).ok(),
            (PyValue::KeyedDict(entries), _) => entries
                .iter()
                .rev()
                .find(|(other, _)| openrigcue_model::python_compare::native_equal(&other.to_value(), &key.to_value()))
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Equality with a text.
    pub fn equals_str(&self, text: &str) -> bool {
        self.equals(&PyValue::Str(text.to_owned()))
    }

    /// An ordering comparison with `other`.
    pub(crate) fn compare(&self, op: Cmp, other: &PyValue) -> Result<bool, CueError> {
        let left = self.to_native();
        let right = other.to_native();
        match op {
            Cmp::Lt => left.py_lt(&right),
            Cmp::Gt => left.py_gt(&right),
        }
        .map_err(native_error)
    }

    /// An arithmetic operation with `other`.
    pub(crate) fn binop(&self, op: Op, other: &PyValue) -> Result<PyValue, CueError> {
        if let (Some(a), Some(b)) = (self.number(), other.number()) {
            match arithmetic(a, op, b) {
                Ok(value) => return Ok(value.value()),
                Err(error) if error.message() == "integer result beyond 128 bits" => {}
                Err(error) => return Err(error),
            }
        }
        let left = self.to_native();
        let right = other.to_native();
        match op {
            Op::Add => left.py_add(&right),
            Op::Sub => left.py_sub(&right),
            Op::Mul => left.py_mul(&right),
            Op::Div => left.py_truediv(&right),
        }
        .map(|v| PyValue::from(&v))
        .map_err(native_error)
    }

    /// The absolute value.
    pub fn abs(&self) -> Result<PyValue, CueError> {
        self.to_native().py_abs().map(|v| Self::from(&v)).map_err(native_error)
    }

    /// The value as an integer.
    pub fn to_int(&self) -> Result<i128, CueError> {
        match self {
            PyValue::Bool(b) => Ok(i128::from(*b)),
            PyValue::Int(i) => Ok(*i),
            PyValue::Float(f) => float_to_int(*f),
            PyValue::Str(s) => parse_int(s).ok_or_else(|| invalid_int_literal(repr_str(s))),
            PyValue::Native(LegacyValue::Bytes(bytes)) => std::str::from_utf8(bytes)
                .ok()
                .filter(|_| bytes.is_ascii())
                .and_then(parse_int)
                .ok_or_else(|| invalid_int_literal(self.repr())),
            PyValue::Native(value) => {
                let integer = value.py_integer().map_err(native_error)?;
                let parsed = PyValue::from(&LegacyValue::from_integer(integer));
                match parsed {
                    PyValue::Int(value) => Ok(value),
                    _ => Err(int_overflow()),
                }
            }
            other => Err(CueError::type_error(format!(
                "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                other.type_name()
            ))),
        }
    }

    /// The value as a float.
    pub fn to_float(&self) -> Result<f64, CueError> {
        match self {
            PyValue::Bool(b) => Ok(f64::from(u8::from(*b))),
            PyValue::Int(i) => Ok(*i as f64),
            PyValue::Float(f) => Ok(*f),
            PyValue::Str(s) => parse_float(s)
                .ok_or_else(|| CueError::value_error(format!("could not convert string to float: {}", repr_str(s)))),
            PyValue::Native(LegacyValue::Bytes(bytes)) => std::str::from_utf8(bytes)
                .ok()
                .filter(|_| bytes.is_ascii())
                .and_then(parse_float)
                .ok_or_else(|| CueError::value_error(format!("could not convert string to float: {}", self.repr()))),
            PyValue::Native(value) => value.py_float_native().map_err(native_error),
            other => Err(CueError::type_error(format!(
                "float() argument must be a string or a real number, not '{}'",
                other.type_name()
            ))),
        }
    }

    /// The value as a double for a numeric parameter; a value that is not a real number is a type error.
    pub fn as_double(&self) -> Result<f64, CueError> {
        self.to_native().py_as_double().map_err(native_error)
    }

    /// The value as text.
    pub fn to_str(&self) -> String {
        match self {
            PyValue::Str(s) => s.clone(),
            PyValue::Native(value) => value.py_str().unwrap_or_else(|| native_repr(value)),
            other => other.repr(),
        }
    }

    /// The value as text, with the limit on the digits of a converted integer.
    /// `to_str` is the unchecked form for display.
    pub fn to_str_checked(&self) -> Result<String, CueError> {
        // These scalar representations cannot exceed the decimal digit
        // limit: Int is i128, floats use the shortest round-trip form, and text is exempt.
        if matches!(self, Self::Str(_) | Self::Int(_) | Self::Float(_) | Self::Bool(_)) {
            return Ok(self.to_str());
        }
        let text = openrigcue_model::python_repr::str_native(&self.to_native()).map_err(native_error)?;
        Ok(text.scalar_string().unwrap_or_else(|| self.to_str()))
    }

    /// The value's literal representation (text in quotes).
    pub fn repr(&self) -> String {
        match self {
            PyValue::None => "None".to_owned(),
            PyValue::Bool(true) => "True".to_owned(),
            PyValue::Bool(false) => "False".to_owned(),
            PyValue::Int(i) => i.to_string(),
            PyValue::Float(f) => float_repr(*f),
            PyValue::Str(s) => repr_str(s),
            PyValue::List(items) => format!("[{}]", items.iter().map(PyValue::repr).collect::<Vec<_>>().join(", ")),
            PyValue::Dict(entries) => format!(
                "{{{}}}",
                entries.iter().map(|(k, v)| format!("{}: {}", repr_str(k), v.repr())).collect::<Vec<_>>().join(", ")
            ),
            PyValue::KeyedDict(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, value)| format!("{}: {}", native_repr(&key.to_value()), value.repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            PyValue::Native(value) => native_repr(value),
        }
    }
}

fn native_repr(value: &LegacyValue) -> String {
    openrigcue_model::python_repr::repr(value)
}

/// Exact comparison of two numbers (`None` when a NaN is involved).
fn compare_numbers(a: Number, b: Number) -> Option<Ordering> {
    match (a, b) {
        (Number::Int(x), Number::Int(y)) => Some(x.cmp(&y)),
        (Number::Float(x), Number::Float(y)) => x.partial_cmp(&y),
        (Number::Int(i), Number::Float(f)) => compare_int_float(i, f),
        (Number::Float(f), Number::Int(i)) => compare_int_float(i, f).map(Ordering::reverse),
    }
}

/// An integer compared with a float, exactly.
fn compare_int_float(i: i128, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 { Ordering::Less } else { Ordering::Greater });
    }
    if (-TWO_53..=TWO_53).contains(&i) {
        return (i as f64).partial_cmp(&f);
    }
    // |i| > 2^53: compare the integer part of f, then its fraction.
    if f < i128::MIN as f64 {
        return Some(Ordering::Greater);
    }
    if f >= -(i128::MIN as f64) {
        return Some(Ordering::Less);
    }
    let whole = f.trunc();
    match i.cmp(&(whole as i128)) {
        Ordering::Equal => 0.0f64.partial_cmp(&(f - whole)),
        other => Some(other),
    }
}

/// Arithmetic on two numbers.
fn arithmetic(a: Number, op: Op, b: Number) -> Result<Number, CueError> {
    match (a, b) {
        (Number::Int(x), Number::Int(y)) => match op {
            Op::Add => x.checked_add(y).map(Number::Int).ok_or_else(int_overflow),
            Op::Sub => x.checked_sub(y).map(Number::Int).ok_or_else(int_overflow),
            Op::Mul => x.checked_mul(y).map(Number::Int).ok_or_else(int_overflow),
            Op::Div => {
                if y == 0 {
                    return Err(CueError::zero_division(DivisionKind::Int));
                }
                Ok(Number::Float(int_true_divide(x, y)))
            }
        },
        _ => {
            let (x, y) = (a.to_f64(), b.to_f64());
            Ok(Number::Float(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => {
                    if y == 0.0 {
                        return Err(CueError::zero_division(DivisionKind::Float));
                    }
                    x / y
                }
            }))
        }
    }
}

/// `int / int`, rounded once from the exact bounded integer ratio.
fn int_true_divide(x: i128, y: i128) -> f64 {
    openrigcue_model::python_numeric::int_true_divide(x, y).expect("division by zero checked before numeric division")
}

/// The value error for a text that is not an integer literal.
fn invalid_int_literal(repr: String) -> CueError {
    // The message shows at most 200 characters of the text, counted as Unicode characters, including when
    // truncation removes the representation's closing quote.
    let shown: String = repr.chars().take(200).collect();
    CueError::value_error(format!("invalid literal for int() with base 10: {shown}"))
}

pub(crate) fn float_to_int(f: f64) -> Result<i128, CueError> {
    if f.is_nan() {
        return Err(CueError::value_error("cannot convert float NaN to integer"));
    }
    if f.is_infinite() {
        return Err(CueError::overflow("cannot convert float infinity to integer"));
    }
    let t = f.trunc();
    if t < i128::MIN as f64 || t >= -(i128::MIN as f64) {
        return Err(int_overflow());
    }
    Ok(t as i128)
}

/// Numeric strings use the settings/profile grammar, including Unicode decimal digits.
fn parse_int(text: &str) -> Option<i128> {
    openrigcue_model::native_value::parse_py_int128(text)
}

fn parse_float(text: &str) -> Option<f64> {
    openrigcue_model::native_value::parse_py_float(text)
}

/// A text in quotes: single quotes unless the text holds a single quote and no double quote; backslash,
/// the quote, `\t`, `\n`, `\r` and other control characters escaped.
pub fn repr_str(s: &str) -> String {
    openrigcue_model::python_repr::repr_str(s)
}

/// A float as text: shortest round-trip digits, positional for exponents in [-4, 16), scientific
/// otherwise, and always a fractional part when positional.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0.0" } else { "0.0" }.to_owned();
    }
    let sci = format!("{value:e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if value < 0.0 { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            let (int_part, frac_part) = digits.split_at(point as usize);
            format!("{int_part}.{frac_part}")
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() { first.to_owned() } else { format!("{first}.{rest}") };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{exp_sign}{:02}", exponent.abs())
    }
}

/// The lenient boolean parse: a bool is returned as is; otherwise the lower-cased text in
/// `yes/true/t/y/1` is true, in `no/false/f/n/0` false, anything else is an error
/// "Boolean value expected. Got <text>" (a type error for a value that is not text).
pub fn str2bool(v: &PyValue) -> Result<bool, CueError> {
    if let PyValue::Native(value) = v {
        let resolved = value.resolved();
        if matches!(resolved, LegacyValue::Object(_)) {
            unreachable!("arena resolves one object layer");
        }
        if matches!(value, LegacyValue::Object(_)) {
            return str2bool(&PyValue::from(&resolved));
        }
        if let Some(text) = resolved.python_string() {
            if let Some(text) = text.scalar_string() {
                return str2bool(&PyValue::Str(text));
            }
            let text = text.repr();
            let text = &text[1..text.len() - 1];
            return Err(CueError::new(
                crate::error::Exception::Exception,
                format!("Boolean value expected. Got {text}"),
            )
            .at(functions("str2bool", 261)));
        }
    }
    if let PyValue::Bool(b) = v {
        return Ok(*b);
    }
    let lower = v.to_str_checked().map_err(|error| error.at(functions("str2bool", 256)))?.to_lowercase();
    if matches!(lower.as_str(), "yes" | "true" | "t" | "y" | "1") {
        return Ok(true);
    }
    if matches!(lower.as_str(), "no" | "false" | "f" | "n" | "0") {
        return Ok(false);
    }
    let raised = match v {
        PyValue::Str(s) => {
            CueError::new(crate::error::Exception::Exception, format!("Boolean value expected. Got {s}"))
        }
        other => CueError::type_error(format!("can only concatenate str (not \"{}\") to str", other.type_name())),
    };
    Err(raised.at(functions("str2bool", 261)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> PyValue {
        PyValue::Str(v.to_owned())
    }

    #[test]
    fn minimum_represented_integer_compares_exactly_with_floats() {
        let value = s("-170141183460469231731687303715884105728").to_int().unwrap();
        assert_eq!(value, i128::MIN);
        let integer = PyValue::Int(value);
        let equal = PyValue::Float(i128::MIN as f64);
        assert!(integer.equals(&equal));
        assert!(!integer.compare(Cmp::Lt, &equal).unwrap());
        assert!(!integer.compare(Cmp::Gt, &equal).unwrap());
        assert!(integer.compare(Cmp::Lt, &PyValue::Float((i128::MIN as f64).next_up())).unwrap());
        assert!(integer.compare(Cmp::Gt, &PyValue::Float((i128::MIN as f64).next_down())).unwrap());
        assert!(!integer.equals(&PyValue::Float(0.0)));
        assert!(PyValue::Int(i128::MAX).compare(Cmp::Lt, &PyValue::Float(-(i128::MIN as f64))).unwrap());
    }

    #[test]
    fn numeric_comparisons_keep_fractions_subnormals_and_unordered_nans() {
        for (integer, float, expected) in [
            (0, -0.0, Ordering::Equal),
            (0, f64::from_bits(1), Ordering::Less),
            (0, -f64::from_bits(1), Ordering::Greater),
            (-1, -1.5, Ordering::Greater),
            (-2, -1.5, Ordering::Less),
            (9_007_199_254_740_993, 9_007_199_254_740_992.0, Ordering::Greater),
            (9_007_199_254_740_993, 9_007_199_254_740_994.0, Ordering::Less),
            (-9_007_199_254_740_993, -9_007_199_254_740_992.0, Ordering::Less),
            (-9_007_199_254_740_993, -9_007_199_254_740_994.0, Ordering::Greater),
            (i128::MAX, f64::INFINITY, Ordering::Less),
            (i128::MIN, f64::NEG_INFINITY, Ordering::Greater),
        ] {
            assert_eq!(compare_numbers(Number::Int(integer), Number::Float(float)), Some(expected));
            assert_eq!(compare_numbers(Number::Float(float), Number::Int(integer)), Some(expected.reverse()));
        }
        let nan = PyValue::Float(f64::NAN);
        for other in [PyValue::Int(0), PyValue::Int(i128::MAX), PyValue::Float(f64::INFINITY), nan.clone()] {
            assert!(!nan.equals(&other));
            assert!(!nan.compare(Cmp::Lt, &other).unwrap());
            assert!(!nan.compare(Cmp::Gt, &other).unwrap());
            assert!(!other.compare(Cmp::Lt, &nan).unwrap());
            assert!(!other.compare(Cmp::Gt, &nan).unwrap());
        }
    }

    #[test]
    fn int_and_float_conversions_like_cpython() {
        assert_eq!(s(" 1_000 ").to_int(), Ok(1000));
        assert_eq!(s("-7").to_int(), Ok(-7));
        assert_eq!(s("15.0").to_int().unwrap_err().to_string(), "invalid literal for int() with base 10: '15.0'");
        assert_eq!(s("1__0").to_int().unwrap_err().exception(), crate::error::Exception::ValueError);
        assert_eq!(PyValue::Float(-2.9).to_int(), Ok(-2));
        assert_eq!(PyValue::Float(i128::MIN as f64).to_int(), Ok(i128::MIN));
        assert_eq!(PyValue::Float(f64::NAN).to_int().unwrap_err().to_string(), "cannot convert float NaN to integer");
        assert_eq!(
            PyValue::Float(f64::INFINITY).to_int().unwrap_err().to_string(),
            "cannot convert float infinity to integer"
        );
        assert_eq!(
            PyValue::None.to_int().unwrap_err().to_string(),
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        );
        assert_eq!(s("1.66").to_float(), Ok(1.66));
        assert_eq!(s(".5e1").to_float(), Ok(5.0));
        assert_eq!(s("1_0.5").to_float(), Ok(10.5));
        assert_eq!(s("-Infinity").to_float(), Ok(f64::NEG_INFINITY));
        assert!(s("nan").to_float().unwrap().is_nan());
        assert_eq!(s("abc").to_float().unwrap_err().to_string(), "could not convert string to float: 'abc'");
        assert_eq!(s("1e").to_float().unwrap_err().exception(), crate::error::Exception::ValueError);
    }

    #[test]
    fn native_pickle_kinds_keep_their_type_at_motion_operations() {
        let bytes = PyValue::from(&LegacyValue::Bytes(b"  +1_2 ".to_vec()));
        assert_eq!(bytes.type_name(), "bytes");
        assert_eq!(bytes.to_int(), Ok(12));
        assert_eq!(bytes.to_float(), Ok(12.0));
        let invalid = PyValue::from(&LegacyValue::Bytes(vec![0xff]));
        assert_eq!(invalid.repr(), "b'\\xff'");
        assert_eq!(invalid.to_int().unwrap_err().to_string(), "invalid literal for int() with base 10: b'\\xff'");
        let tuple = PyValue::from(&LegacyValue::Tuple(vec![1.into()]));
        assert_eq!(tuple.repr(), "(1,)");
        assert_eq!(
            tuple.to_float().unwrap_err().to_string(),
            "float() argument must be a string or a real number, not 'tuple'"
        );
        assert_eq!(tuple.item("pitch").unwrap_err().to_string(), "tuple indices must be integers or slices, not str");
    }

    #[test]
    fn arithmetic_and_comparison_errors_like_cpython() {
        let n20 = PyValue::Int(20);
        assert_eq!(
            s("a").binop(Op::Div, &n20).unwrap_err().to_string(),
            "unsupported operand type(s) for /: 'str' and 'int'"
        );
        assert_eq!(s("a").binop(Op::Mul, &PyValue::Int(1)), Ok(s("a")));
        assert_eq!(
            s("a").binop(Op::Mul, &PyValue::Float(1.0)).unwrap_err().to_string(),
            "can't multiply sequence by non-int of type 'float'"
        );
        assert_eq!(
            PyValue::Dict(vec![]).binop(Op::Mul, &PyValue::Int(1)).unwrap_err().to_string(),
            "unsupported operand type(s) for *: 'dict' and 'int'"
        );
        assert_eq!(
            PyValue::Float(1.0).compare(Cmp::Lt, &s("x")).unwrap_err().to_string(),
            "'<' not supported between instances of 'float' and 'str'"
        );
        assert_eq!(PyValue::Int(1).binop(Op::Div, &PyValue::Int(0)).unwrap_err().to_string(), "division by zero");
        assert_eq!(
            PyValue::Float(1.0).binop(Op::Div, &PyValue::Int(0)).unwrap_err().to_string(),
            "float division by zero"
        );
        assert_eq!(PyValue::Int(3).binop(Op::Div, &PyValue::Int(2)), Ok(PyValue::Float(1.5)));
        assert_eq!(PyValue::Bool(true).binop(Op::Mul, &PyValue::Int(7)), Ok(PyValue::Int(7)));
        assert!(PyValue::Int((1 << 60) + 1).compare(Cmp::Gt, &PyValue::Float((1u64 << 60) as f64)).unwrap());
        assert!(!PyValue::Float(f64::NAN).compare(Cmp::Lt, &PyValue::Int(1)).unwrap());
        assert_eq!(s("x").abs().unwrap_err().to_string(), "bad operand type for abs(): 'str'");
    }

    #[test]
    fn sequence_count_and_length_errors_precede_later_numeric_operations() {
        for sequence in [s(""), s("a"), PyValue::List(vec![])] {
            for count in [i128::from(i64::MAX) + 1, i128::from(i64::MIN) - 1] {
                let error = sequence.binop(Op::Mul, &PyValue::Int(count)).unwrap_err();
                assert_eq!(error.exception(), crate::Exception::OverflowError);
                assert_eq!(error.to_string(), "cannot fit 'int' into an index-sized integer");
            }
        }
        assert_eq!(
            s("ab").binop(Op::Mul, &PyValue::Int(i64::MAX.into())).unwrap_err().to_string(),
            "repeated string is too long"
        );
        let error = PyValue::List(vec![PyValue::None; 2]).binop(Op::Mul, &PyValue::Int(i64::MAX.into())).unwrap_err();
        assert_eq!(error.exception().name(), "MemoryError");
        assert_eq!(error.to_string(), "");
        assert_eq!(s("").binop(Op::Mul, &PyValue::Int(i64::MAX.into())), Ok(s("")));
        assert_eq!(s("ab").binop(Op::Mul, &PyValue::Int(i64::MIN.into())), Ok(s("")));
    }

    #[test]
    fn item_access_errors_like_cpython() {
        let d = PyValue::Dict(vec![("pitch".into(), PyValue::Int(1))]);
        assert_eq!(d.item("pitch"), Ok(&PyValue::Int(1)));
        assert_eq!(d.item("roll").unwrap_err().to_string(), "'roll'");
        assert_eq!(PyValue::Int(3).item("x").unwrap_err().to_string(), "'int' object is not subscriptable");
        assert_eq!(
            PyValue::List(vec![]).item("x").unwrap_err().to_string(),
            "list indices must be integers or slices, not str"
        );
    }

    #[test]
    fn imported_dictionaries_retain_non_string_entries() {
        use openrigcue_model::native_value::Dict;
        let mut entries = Dict::new();
        entries.insert(Key::Int(1), 7.into());
        entries.insert("pitch", 8.into());
        entries.insert(Key::None, 9.into());
        let value = PyValue::from(&LegacyValue::Dict(entries.clone()));
        assert_eq!(value.type_name(), "dict");
        assert_eq!(value.repr(), "{1: 7, 'pitch': 8, None: 9}");
        assert_eq!(value.item("pitch"), Ok(&PyValue::Int(8)));
        assert_eq!(value.item("1").unwrap_err().message(), "'1'");
        entries.insert(Key::Bool(true), 10.into());
        let changed = PyValue::from(&LegacyValue::Dict(entries));
        assert!(!value.equals(&changed), "an update to a numeric key must survive the motion boundary");
        assert!(!value.equals(&PyValue::Dict(vec![("pitch".into(), PyValue::Int(8))])));
    }

    #[test]
    fn imported_dictionary_equality_keeps_numeric_key_aliases_and_nested_values() {
        use openrigcue_model::native_value::Dict;
        let mut left = Dict::new();
        left.insert(Key::Int(1), LegacyValue::Float(3.0));
        left.insert(Key::None, 4.into());
        let mut right = Dict::new();
        right.insert(Key::None, 4.into());
        right.insert(Key::Bool(true), 3.into());
        let left = PyValue::from(&LegacyValue::List(vec![LegacyValue::Dict(left)]));
        let right = PyValue::from(&LegacyValue::List(vec![LegacyValue::Dict(right)]));
        assert!(left.equals(&right));
        assert_eq!(left.repr(), "[{1: 3.0, None: 4}]");
    }

    #[test]
    fn str2bool_like_backend_functions() {
        assert_eq!(str2bool(&s("Yes")), Ok(true));
        assert_eq!(str2bool(&s("0")), Ok(false));
        assert_eq!(str2bool(&PyValue::Bool(false)), Ok(false));
        let e = str2bool(&s("maybe")).unwrap_err();
        assert_eq!(e.to_string(), "Boolean value expected. Got maybe");
        assert!(e.traceback().contains("File \"backend/functions.py\", line 261, in backend.functions.str2bool"));
    }

    #[test]
    fn str2bool_retains_decimal_output_failure_before_its_invalid_value_branch() {
        let huge = LegacyValue::from_integer(num_bigint::BigInt::from(10u8).pow(4300));
        for value in [PyValue::from(&huge), PyValue::List(vec![PyValue::from(&huge)])] {
            let error = str2bool(&value).unwrap_err();
            assert_eq!(error.exception(), crate::Exception::ValueError);
            assert!(error.message().starts_with("Exceeds the limit (4300 digits)"));
            assert_eq!(error.frames().map(|frame| frame.line).collect::<Vec<_>>(), [256]);
        }
        let accepted = PyValue::from(&LegacyValue::from_integer(num_bigint::BigInt::from(10u8).pow(4299)));
        assert_eq!(accepted.to_str_checked().unwrap().len(), 4300);
        assert_eq!(str2bool(&accepted).unwrap_err().exception(), crate::Exception::TypeError);
        let hexadecimal = PyValue::from(format!("0x{}", "f".repeat(5000)).as_str());
        assert_eq!(hexadecimal.to_str_checked().unwrap().len(), 5002);
    }

    #[test]
    fn reprs_like_cpython() {
        assert_eq!(repr_str("it's"), "\"it's\"");
        assert_eq!(repr_str("a\nb"), "'a\\nb'");
        assert_eq!(float_repr(0.1), "0.1");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(5.0), "5.0");
        assert_eq!(PyValue::List(vec![PyValue::Int(1), s("a")]).repr(), "[1, 'a']");
    }

    #[test]
    fn native_pickle_comparison_and_repr_keep_builtin_semantics() {
        use openrigcue_model::native_value::DateTimeParts;
        let base = DateTimeParts {
            year: 2024,
            month: 11,
            day: 3,
            hour: 1,
            minute: 30,
            second: 0,
            microsecond: 0,
            fold: false,
        };
        let normal = PyValue::from(&LegacyValue::DateTime(base));
        let folded = PyValue::from(&LegacyValue::DateTime(DateTimeParts { fold: true, ..base }));
        assert!(normal.equals(&folded), "naive datetime equality ignores fold");
        assert_eq!(folded.repr(), "datetime.datetime(2024, 11, 3, 1, 30, fold=1)");
        let integer_tuple = PyValue::from(&LegacyValue::Tuple(vec![LegacyValue::Int(1)]));
        let float_tuple = PyValue::from(&LegacyValue::Tuple(vec![LegacyValue::Float(1.0)]));
        assert!(integer_tuple.equals(&float_tuple), "tuple equality compares numeric values");
        let first = PyValue::from(&LegacyValue::Set(vec![1.into(), 2.into()]));
        let reordered = PyValue::from(&LegacyValue::Set(vec![2.into(), 1.into()]));
        assert!(first.equals(&reordered), "set equality ignores insertion order");
    }
}

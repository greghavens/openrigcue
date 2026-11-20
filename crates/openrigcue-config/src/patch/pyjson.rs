//! JSON reading, comparison and writing for game settings files: a lenient reader
//! (`NaN`, `Infinity`, `-Infinity`, integers of any size up to a 4300-digit limit, floats
//! that overflow to infinity, lone surrogate escapes kept, duplicate keys keep their first position and last
//! value, a leading byte-order mark rejected), loose equality, integer and text conversion of the parsed
//! values, and an indented writer that emits its output as a sequence of chunks.

use super::pyunicode::{self, PyInt, MAX_STR_DIGITS};

/// Containers nested deeper than this fail to parse.
const MAX_DEPTH: usize = 3000;

/// A JSON string as UTF-16 code units: JSON escapes can produce lone surrogates, which a Rust `String` cannot
/// hold; JSON text never yields a high surrogate followed by a low one that are not a pair.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PyStr(Vec<u16>);

impl PyStr {
    pub fn new(text: &str) -> Self {
        PyStr(text.encode_utf16().collect())
    }

    /// Equality with a text.
    pub fn is(&self, text: &str) -> bool {
        self.0.iter().copied().eq(text.encode_utf16())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The text, `None` when it holds a lone surrogate (not encodable in any codec the modules write).
    pub fn to_text(&self) -> Option<String> {
        String::from_utf16(&self.0).ok()
    }

    /// The code points (a lone surrogate is its own code point).
    pub fn code_points(&self) -> Vec<u32> {
        char::decode_utf16(self.0.iter().copied())
            .map(|unit| unit.map(u32::from).unwrap_or_else(|error| u32::from(error.unpaired_surrogate())))
            .collect()
    }

    fn push_str(&mut self, text: &str) {
        self.0.extend(text.encode_utf16());
    }
}

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum PyValue {
    Null,
    Bool(bool),
    Int(PyInt),
    Float(f64),
    Str(PyStr),
    List(Vec<PyValue>),
    Dict(Dict),
}

impl PyValue {
    pub fn str(text: &str) -> Self {
        PyValue::Str(PyStr::new(text))
    }

    pub fn int(value: i128) -> Self {
        PyValue::Int(PyInt::Small(value))
    }

    /// The entry `key` of an object.
    pub fn get(&self, key: &str) -> Option<&PyValue> {
        match self {
            PyValue::Dict(dict) => dict.get(key),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            PyValue::Dict(dict) => Some(dict),
            _ => None,
        }
    }

    pub fn as_dict_mut(&mut self) -> Option<&mut Dict> {
        match self {
            PyValue::Dict(dict) => Some(dict),
            _ => None,
        }
    }
}

/// An object with string keys, in insertion order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Dict(Vec<(PyStr, PyValue)>);

impl Dict {
    pub fn new() -> Self {
        Dict(Vec::new())
    }

    /// A dict of the pairs in order (later duplicates replace earlier values in place).
    pub fn of<'a>(pairs: impl IntoIterator<Item = (&'a str, PyValue)>) -> Self {
        let mut dict = Dict::new();
        for (key, value) in pairs {
            dict.insert(key, value);
        }
        dict
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&PyValue> {
        self.0.iter().find(|(k, _)| k.is(key)).map(|(_, value)| value)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut PyValue> {
        self.0.iter_mut().find(|(k, _)| k.is(key)).map(|(_, value)| value)
    }

    /// Sets `key`: replaces in place, or appends.
    pub fn insert(&mut self, key: &str, value: PyValue) {
        self.insert_key(PyStr::new(key), value);
    }

    fn insert_key(&mut self, key: PyStr, value: PyValue) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some((_, existing)) => *existing = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PyStr, &PyValue)> {
        self.0.iter().map(|(key, value)| (key, value))
    }

    /// Sets every entry of `other`, in its order.
    pub fn update(&mut self, other: Dict) {
        for (key, value) in other.0 {
            self.insert_key(key, value);
        }
    }
}

/// Why a text did not parse (syntax, the digit limit, or the nesting limit).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at character {position}")]
pub struct JsonError {
    pub message: &'static str,
    pub position: usize,
}

/// Parses a JSON text.
pub fn parse(text: &str) -> Result<PyValue, JsonError> {
    let chars: Vec<char> = text.chars().collect();
    let scanner = Scanner { s: &chars };
    if chars.first() == Some(&'\u{feff}') {
        return Err(JsonError { message: "Unexpected UTF-8 BOM (decode using utf-8-sig)", position: 0 });
    }
    let start = scanner.skip_whitespace(0);
    let (value, end) = scanner.scan(start).map_err(|error| match error {
        ScanError::Stop(position) => JsonError { message: "Expecting value", position },
        ScanError::Raise(error) => error,
    })?;
    let end = scanner.skip_whitespace(end);
    if end != chars.len() {
        return Err(JsonError { message: "Extra data", position: end });
    }
    Ok(value)
}

/// No value at this index, or an error.
enum ScanError {
    Stop(usize),
    Raise(JsonError),
}

fn raise<T>(message: &'static str, position: usize) -> Result<T, ScanError> {
    Err(ScanError::Raise(JsonError { message, position }))
}

/// The scanner: one value at a position, and its helpers.
struct Scanner<'a> {
    s: &'a [char],
}

impl Scanner<'_> {
    fn at(&self, index: usize) -> Option<char> {
        self.s.get(index).copied()
    }

    fn skip_whitespace(&self, mut index: usize) -> usize {
        while matches!(self.at(index), Some(' ' | '\t' | '\n' | '\r')) {
            index += 1;
        }
        index
    }

    fn literal(&self, index: usize, word: &str) -> bool {
        word.chars().enumerate().all(|(offset, c)| self.at(index + offset) == Some(c))
    }

    /// Scans one value, with the containers kept on an explicit stack instead of recursion.
    fn scan(&self, start: usize) -> Result<(PyValue, usize), ScanError> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut index = start;
        loop {
            let Some(c) = self.at(index) else {
                return Err(ScanError::Stop(index));
            };
            let (mut value, mut end) = match c {
                '{' | '[' => {
                    if stack.len() >= MAX_DEPTH {
                        return raise("maximum recursion depth exceeded while decoding a JSON object", index);
                    }
                    let inner = self.skip_whitespace(index + 1);
                    if c == '[' {
                        if self.at(inner) == Some(']') {
                            (PyValue::List(Vec::new()), inner + 1)
                        } else {
                            stack.push(Frame::List(Vec::new()));
                            index = inner;
                            continue;
                        }
                    } else if self.at(inner) == Some('}') {
                        (PyValue::Dict(Dict::new()), inner + 1)
                    } else {
                        let (key, next) = self.key(inner)?;
                        stack.push(Frame::Dict(Dict::new(), key));
                        index = next;
                        continue;
                    }
                }
                _ => self.scalar(index)?,
            };
            // Attach the value to the enclosing containers, closing those that end here.
            loop {
                let Some(frame) = stack.last_mut() else {
                    return Ok((value, end));
                };
                let after = self.skip_whitespace(end);
                let closing = match frame {
                    Frame::List(items) => {
                        items.push(value);
                        ']'
                    }
                    Frame::Dict(dict, key) => {
                        dict.insert_key(std::mem::take(key), value);
                        '}'
                    }
                };
                match self.at(after) {
                    Some(c) if c == closing => {
                        value = match stack.pop() {
                            Some(Frame::List(items)) => PyValue::List(items),
                            Some(Frame::Dict(dict, _)) => PyValue::Dict(dict),
                            None => unreachable!("a frame was just used"),
                        };
                        end = after + 1;
                    }
                    Some(',') => {
                        let next = self.skip_whitespace(after + 1);
                        if self.at(next) == Some(closing) {
                            return if closing == ']' {
                                raise("Illegal trailing comma before end of array", next)
                            } else {
                                raise("Illegal trailing comma before end of object", next)
                            };
                        }
                        index = match stack.last_mut() {
                            Some(Frame::Dict(_, key)) => {
                                let (next_key, value_start) = self.key(next)?;
                                *key = next_key;
                                value_start
                            }
                            _ => next,
                        };
                        break;
                    }
                    _ => return raise("Expecting ',' delimiter", after),
                }
            }
        }
    }

    /// A property name and its `:`; returns the key and where its value starts.
    fn key(&self, index: usize) -> Result<(PyStr, usize), ScanError> {
        if self.at(index) != Some('"') {
            return raise("Expecting property name enclosed in double quotes", index);
        }
        let (key, end) = self.string(index + 1)?;
        let colon = self.skip_whitespace(end);
        if self.at(colon) != Some(':') {
            return raise("Expecting ':' delimiter", colon);
        }
        Ok((key, self.skip_whitespace(colon + 1)))
    }

    fn scalar(&self, index: usize) -> Result<(PyValue, usize), ScanError> {
        match self.at(index) {
            Some('"') => self.string(index + 1).map(|(text, end)| (PyValue::Str(text), end)),
            Some('n') if self.literal(index, "null") => Ok((PyValue::Null, index + 4)),
            Some('t') if self.literal(index, "true") => Ok((PyValue::Bool(true), index + 4)),
            Some('f') if self.literal(index, "false") => Ok((PyValue::Bool(false), index + 5)),
            Some('N') if self.literal(index, "NaN") => Ok((PyValue::Float(f64::NAN), index + 3)),
            Some('I') if self.literal(index, "Infinity") => Ok((PyValue::Float(f64::INFINITY), index + 8)),
            Some('-') if self.literal(index, "-Infinity") => Ok((PyValue::Float(f64::NEG_INFINITY), index + 9)),
            _ => self.number(index),
        }
    }

    /// A number: ASCII digits, `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?`.
    fn number(&self, start: usize) -> Result<(PyValue, usize), ScanError> {
        let digit = |index: usize| self.at(index).is_some_and(|c| c.is_ascii_digit());
        let mut index = start;
        if self.at(index) == Some('-') {
            index += 1;
        }
        match self.at(index) {
            Some('1'..='9') => {
                index += 1;
                while digit(index) {
                    index += 1;
                }
            }
            Some('0') => index += 1,
            _ => return Err(ScanError::Stop(start)),
        }
        let mut is_float = false;
        if self.at(index) == Some('.') && digit(index + 1) {
            is_float = true;
            index += 2;
            while digit(index) {
                index += 1;
            }
        }
        if matches!(self.at(index), Some('e' | 'E')) {
            let exponent_start = index;
            index += 1;
            if matches!(self.at(index), Some('-' | '+')) {
                index += 1;
            }
            let digits_start = index;
            while digit(index) {
                index += 1;
            }
            if index > digits_start {
                is_float = true;
            } else {
                index = exponent_start;
            }
        }
        let text: String = self.s[start..index].iter().collect();
        if is_float {
            // Correctly rounded; overflow gives infinity (Rust's parser does both).
            let value = text.parse::<f64>().map_err(|_| ScanError::Stop(start))?;
            return Ok((PyValue::Float(value), index));
        }
        let (negative, digits) = match text.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, text.as_str()),
        };
        if digits.len() > MAX_STR_DIGITS {
            return raise("Exceeds the limit (4300 digits) for integer string conversion", start);
        }
        Ok((PyValue::Int(PyInt::from_digits(negative, digits)), index))
    }

    /// A string, control characters refused; `index` is just after the opening quote.
    fn string(&self, index: usize) -> Result<(PyStr, usize), ScanError> {
        let begin = index - 1;
        let mut out = PyStr::default();
        let mut index = index;
        loop {
            let Some(c) = self.at(index) else {
                return raise("Unterminated string starting at", begin);
            };
            match c {
                '"' => return Ok((out, index + 1)),
                '\\' => {
                    let Some(escape) = self.at(index + 1) else {
                        return raise("Unterminated string starting at", begin);
                    };
                    let simple = match escape {
                        '"' => Some('"'),
                        '\\' => Some('\\'),
                        '/' => Some('/'),
                        'b' => Some('\u{8}'),
                        'f' => Some('\u{c}'),
                        'n' => Some('\n'),
                        'r' => Some('\r'),
                        't' => Some('\t'),
                        'u' => None,
                        _ => return raise("Invalid \\escape", index),
                    };
                    if let Some(simple) = simple {
                        out.push_str(simple.encode_utf8(&mut [0; 4]));
                        index += 2;
                        continue;
                    }
                    let unit = self.hex4(index + 2)?;
                    index += 6;
                    if (0xd800..0xdc00).contains(&unit)
                        && index + 6 < self.s.len()
                        && self.at(index) == Some('\\')
                        && self.at(index + 1) == Some('u')
                    {
                        let low = self.hex4(index + 2)?;
                        if (0xdc00..0xe000).contains(&low) {
                            out.0.extend([unit, low]);
                            index += 6;
                            continue;
                        }
                    }
                    out.0.push(unit);
                }
                c if u32::from(c) < 0x20 => return raise("Invalid control character at", index),
                c => {
                    out.push_str(c.encode_utf8(&mut [0; 4]));
                    index += 1;
                }
            }
        }
    }

    fn hex4(&self, index: usize) -> Result<u16, ScanError> {
        let mut value = 0u16;
        for offset in 0..4 {
            let digit = self.at(index + offset).and_then(|c| c.to_digit(16));
            let Some(digit) = digit else {
                return raise("Invalid \\uXXXX escape", index - 1);
            };
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }
}

/// An open container: a list, or a dict with the key whose value is being read.
enum Frame {
    List(Vec<PyValue>),
    Dict(Dict, PyStr),
}

/// Output layout: indent, separators and whether output is ASCII-escaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DumpStyle {
    pub indent: usize,
    pub item_separator: &'static str,
    pub key_separator: &'static str,
    pub ensure_ascii: bool,
}

impl DumpStyle {
    /// Indent `n` with separators `,` and `: `, ASCII output.
    pub const fn indented(indent: usize) -> Self {
        DumpStyle { indent, item_separator: ",", key_separator: ": ", ensure_ascii: true }
    }
}

/// The chunks written to the file, in order: a list item that
/// is a scalar shares its chunk with the separator and indentation before it; dict keys, the key separator and
/// scalar values are chunks of their own.
pub fn dump_chunks(value: &PyValue, style: &DumpStyle) -> Vec<PyStr> {
    let mut chunks = Vec::new();
    // Pending work is kept on an explicit stack (LIFO).
    let mut tasks = vec![Task::Value(value, 0)];
    let newline_indent = |level: usize| format!("\n{}", " ".repeat(style.indent * level));
    while let Some(task) = tasks.pop() {
        match task {
            Task::Value(value, level) => match value {
                PyValue::List(items) if items.is_empty() => chunks.push(PyStr::new("[]")),
                PyValue::Dict(dict) if dict.is_empty() => chunks.push(PyStr::new("{}")),
                PyValue::List(items) => tasks.push(Task::ListItem(items, 0, level)),
                PyValue::Dict(dict) => {
                    chunks.push(PyStr::new("{"));
                    chunks.push(PyStr::new(&newline_indent(level + 1)));
                    tasks.push(Task::DictItem(dict, 0, level));
                }
                scalar => chunks.push(scalar_text(scalar, style)),
            },
            Task::ListItem(items, index, level) => {
                let Some(item) = items.get(index) else {
                    chunks.push(PyStr::new(&newline_indent(level)));
                    chunks.push(PyStr::new("]"));
                    continue;
                };
                let buffer = if index == 0 {
                    format!("[{}", newline_indent(level + 1))
                } else {
                    format!("{}{}", style.item_separator, newline_indent(level + 1))
                };
                tasks.push(Task::ListItem(items, index + 1, level));
                let mut chunk = PyStr::new(&buffer);
                if matches!(item, PyValue::List(_) | PyValue::Dict(_)) {
                    tasks.push(Task::Value(item, level + 1));
                } else {
                    chunk.0.extend(scalar_text(item, style).0);
                }
                chunks.push(chunk);
            }
            Task::DictItem(dict, index, level) => {
                let Some((key, value)) = dict.0.get(index) else {
                    chunks.push(PyStr::new(&newline_indent(level)));
                    chunks.push(PyStr::new("}"));
                    continue;
                };
                if index > 0 {
                    chunks.push(PyStr::new(&format!("{}{}", style.item_separator, newline_indent(level + 1))));
                }
                chunks.push(encode_string(key, style.ensure_ascii));
                chunks.push(PyStr::new(style.key_separator));
                tasks.push(Task::DictItem(dict, index + 1, level));
                tasks.push(Task::Value(value, level + 1));
            }
        }
    }
    chunks
}

/// Pending encoding work: a value at an indentation level, or the next item of a container.
enum Task<'a> {
    Value(&'a PyValue, usize),
    ListItem(&'a [PyValue], usize, usize),
    DictItem(&'a Dict, usize, usize),
}

/// The text of a scalar (string, `null`, `true`, `false`, integer or float).
fn scalar_text(value: &PyValue, style: &DumpStyle) -> PyStr {
    match value {
        PyValue::Str(text) => encode_string(text, style.ensure_ascii),
        PyValue::Null => PyStr::new("null"),
        PyValue::Bool(true) => PyStr::new("true"),
        PyValue::Bool(false) => PyStr::new("false"),
        PyValue::Int(int) => PyStr::new(&int.repr()),
        PyValue::Float(float) => PyStr::new(&float_repr(*float)),
        PyValue::List(_) | PyValue::Dict(_) => PyStr::default(),
    }
}

#[cfg(test)]
/// The whole output text; `None` when a string holds a lone surrogate and ASCII escaping is off.
pub fn dump(value: &PyValue, style: &DumpStyle) -> Option<String> {
    let mut all = PyStr::default();
    for chunk in dump_chunks(value, style) {
        all.0.extend(chunk.0);
    }
    all.to_text()
}

/// A quoted string, ASCII-escaped or not.
fn encode_string(text: &PyStr, ensure_ascii: bool) -> PyStr {
    let mut out = PyStr::new("\"");
    for &unit in &text.0 {
        let named = match unit {
            0x22 => Some("\\\""),
            0x5c => Some("\\\\"),
            0x0a => Some("\\n"),
            0x0d => Some("\\r"),
            0x09 => Some("\\t"),
            0x08 => Some("\\b"),
            0x0c => Some("\\f"),
            _ => None,
        };
        match named {
            Some(escape) => out.push_str(escape),
            None if unit < 0x20 || (ensure_ascii && !(0x20..=0x7e).contains(&unit)) => {
                out.push_str(&format!("\\u{unit:04x}"));
            }
            None => out.0.push(unit),
        }
    }
    out.push_str("\"");
    out
}

/// The text of a float: `NaN`, `Infinity`, `-Infinity`, else the shortest
/// round-trip digits ( positional between 1e-4 and 1e16, else `d.ddde±XX`).
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let point = exponent + 1;
    let body = if (-3..=16).contains(&point) {
        positional(&digits, point)
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() { String::new() } else { format!(".{rest}") };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{first}{fraction}e{exponent_sign}{:02}", exponent.abs())
    };
    format!("{sign}{body}")
}

fn positional(digits: &str, point: i32) -> String {
    let count = digits.len() as i32;
    if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point >= count {
        format!("{digits}{}.0", "0".repeat((point - count) as usize))
    } else {
        let (whole, fraction) = digits.split_at(point as usize);
        format!("{whole}.{fraction}")
    }
}

/// Loose equality between two parsed values: booleans equal the integers 0/1, integers equal floats of exactly the
/// same value, NaN equals nothing, dicts compare by keys and values whatever the order.
pub fn py_eq(a: &PyValue, b: &PyValue) -> bool {
    match (a, b) {
        (PyValue::Null, PyValue::Null) => true,
        (PyValue::Str(x), PyValue::Str(y)) => x == y,
        (PyValue::List(x), PyValue::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (PyValue::Dict(x), PyValue::Dict(y)) => {
            x.len() == y.len() && x.iter().all(|(key, value)| y.0.iter().any(|(k, v)| k == key && py_eq(value, v)))
        }
        _ => match (numeric(a), numeric(b)) {
            (Some(x), Some(y)) => numeric_eq(&x, &y),
            _ => false,
        },
    }
}

enum Numeric {
    Int(PyInt),
    Float(f64),
}

fn numeric(value: &PyValue) -> Option<Numeric> {
    match value {
        PyValue::Bool(flag) => Some(Numeric::Int(PyInt::Small(i128::from(*flag)))),
        PyValue::Int(int) => Some(Numeric::Int(int.clone())),
        PyValue::Float(float) => Some(Numeric::Float(*float)),
        _ => None,
    }
}

fn numeric_eq(a: &Numeric, b: &Numeric) -> bool {
    match (a, b) {
        (Numeric::Int(x), Numeric::Int(y)) => x == y,
        (Numeric::Float(x), Numeric::Float(y)) => x == y,
        (Numeric::Int(int), Numeric::Float(float)) | (Numeric::Float(float), Numeric::Int(int)) => {
            float.is_finite() && float.fract() == 0.0 && PyInt::from_float(*float).as_ref() == Some(int)
        }
    }
}

/// The integer a parsed value converts to; `None` when it does not convert.
pub fn py_int(value: &PyValue) -> Option<PyInt> {
    match value {
        PyValue::Bool(flag) => Some(PyInt::Small(i128::from(*flag))),
        PyValue::Int(int) => Some(int.clone()),
        PyValue::Float(float) => PyInt::from_float(*float),
        PyValue::Str(text) => pyunicode::int_from_code_points(text.code_points()),
        PyValue::Null | PyValue::List(_) | PyValue::Dict(_) => None,
    }
}

/// Whether the text form of a parsed value equals `expected`, the decimal text of a port: only a
/// string with that text or an integer with that value can match (the text of a float, bool, null, list or
/// object never consists of digits only).
pub fn py_str_is(value: &PyValue, expected: &str) -> bool {
    match value {
        PyValue::Str(text) => text.is(expected),
        PyValue::Int(int) => int.repr() == expected,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dumped(value: &PyValue, style: &DumpStyle) -> String {
        dump(value, style).unwrap()
    }

    /// Indent 4, indent 2 with compact separators, and ASCII escaping.
    #[test]
    fn dump_layouts() {
        let value = parse(r#"{"a":[1,2.5,{}],"b":{"c":"é\n"},"d":[]}"#).unwrap();
        assert_eq!(
            dumped(&value, &DumpStyle::indented(4)),
            "{\n    \"a\": [\n        1,\n        2.5,\n        {}\n    ],\n    \"b\": {\n        \"c\": \"\\u00e9\\n\"\n    },\n    \"d\": []\n}"
        );
        let compact = DumpStyle { indent: 2, item_separator: ",", key_separator: ":", ensure_ascii: true };
        let value = PyValue::Dict(Dict::of([("x", PyValue::Bool(true)), ("y", PyValue::Null)]));
        assert_eq!(dumped(&value, &compact), "{\n  \"x\":true,\n  \"y\":null\n}");
        let raw = DumpStyle { ensure_ascii: false, ..DumpStyle::indented(2) };
        let list = parse("[\"é\", \"😀\"]").unwrap();
        assert_eq!(dumped(&list, &raw), "[\n  \"é\",\n  \"😀\"\n]");
        assert_eq!(dumped(&parse("[\"😀\"]").unwrap(), &DumpStyle::indented(2)), "[\n  \"\\ud83d\\ude00\"\n]");
    }

    /// The chunk sequence (one write each).
    #[test]
    fn dump_chunk_boundaries() {
        let value = parse(r#"{"a": [1, "x", {"b": null}], "c": 2}"#).unwrap();
        let chunks: Vec<String> =
            dump_chunks(&value, &DumpStyle::indented(1)).iter().map(|c| c.to_text().unwrap()).collect();
        assert_eq!(
            chunks,
            [
                "{",
                "\n ",
                "\"a\"",
                ": ",
                "[\n  1",
                ",\n  \"x\"",
                ",\n  ",
                "{",
                "\n   ",
                "\"b\"",
                ": ",
                "null",
                "\n  ",
                "}",
                "\n ",
                "]",
                ",\n ",
                "\"c\"",
                ": ",
                "2",
                "\n",
                "}"
            ]
        );
    }

    /// Float text as written, and the non-finite literals.
    #[test]
    fn float_reprs() {
        assert_eq!(float_repr(1.0), "1.0");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(0.00001), "1e-05");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1234567890123456.0), "1234567890123456.0");
        assert_eq!(float_repr(-2.5e-7), "-2.5e-07");
        assert_eq!(float_repr(123.456), "123.456");
        assert_eq!(float_repr(-0.0), "-0.0");
        assert_eq!(float_repr(f64::NAN), "NaN");
        assert_eq!(float_repr(f64::NEG_INFINITY), "-Infinity");
    }

    /// Read/write round trip: correctly rounded floats, integer -0 is 0, -0.0 stays a float, "-0"
    /// inside strings untouched, NaN / Infinity / 1e400 / huge integers kept.
    #[test]
    fn python_number_parsing() {
        let style = DumpStyle::indented(0);
        let value = parse(
            "[0.30000000000000004, 2.2250738585072014e-308, -0, -0.0, -0e1, \"-0\", NaN, -Infinity, 1e400, \
             123456789012345678901234567890123456789012345]",
        )
        .unwrap();
        assert_eq!(
            dumped(&value, &style),
            "[\n0.30000000000000004,\n2.2250738585072014e-308,\n0,\n-0.0,\n-0.0,\n\"-0\",\nNaN,\n-Infinity,\n\
             Infinity,\n123456789012345678901234567890123456789012345\n]"
        );
        assert!(parse(&"1".repeat(MAX_STR_DIGITS)).is_ok());
        assert!(parse(&"1".repeat(MAX_STR_DIGITS + 1)).is_err());
    }

    /// The scanner's grammar: literals, trailing commas, control characters, leading zeros, BOM.
    #[test]
    fn scanner_rules() {
        for bad in [
            "",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1.",
            "1e",
            "[\"a\u{1}\"]",
            "{'a':1}",
            "nul",
            "\u{feff}{}",
            "[1] x",
            "+1",
            ".5",
            "\"\\x\"",
            "\"\\u12G4\"",
            "\"abc",
            "Nan",
            "-NaN",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse(" \t\r\n1 \n").unwrap(), PyValue::int(1));
        assert_eq!(parse("1E+2").unwrap(), PyValue::Float(100.0));
        let dict = parse(r#"{"a":1,"b":2,"a":3}"#).unwrap();
        let keys: Vec<String> = dict.as_dict().unwrap().iter().map(|(k, _)| k.to_text().unwrap()).collect();
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(dict.get("a"), Some(&PyValue::int(3)));
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        let parsed = parse(&deep).unwrap();
        assert_eq!(dump_chunks(&parsed, &DumpStyle::indented(0)).len(), 3 * (MAX_DEPTH - 1) + 1);
        let deeper = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(parse(&deeper).is_err());
    }

    /// Surrogate escapes: pairs join, lone surrogates stay (written back as escapes with ASCII escaping, not
    /// encodable without it).
    #[test]
    fn surrogates() {
        let value = parse(r#"["\ud83d\ude00", "\ud800x", "\udc00", "\ud800\u0041"]"#).unwrap();
        let PyValue::List(items) = &value else { panic!() };
        assert_eq!(items[0], PyValue::str("😀"));
        assert_eq!(
            dumped(&value, &DumpStyle::indented(0)),
            "[\n\"\\ud83d\\ude00\",\n\"\\ud800x\",\n\"\\udc00\",\n\"\\ud800A\"\n]"
        );
        let raw = DumpStyle { ensure_ascii: false, ..DumpStyle::indented(0) };
        assert_eq!(dump(&value, &raw), None);
        let chunks = dump_chunks(&value, &raw);
        assert!(chunks[0].to_text().is_some() && chunks[1].to_text().is_none());
    }

    /// ASCII escaping escapes everything outside ' '..'~', so DEL (U+007F) becomes \u007f; without
    /// it only control characters below U+0020 are escaped.
    #[test]
    fn delete_character_escape() {
        assert_eq!(dumped(&PyValue::str("a\u{7f}"), &DumpStyle::indented(2)), "\"a\\u007f\"");
        let raw = DumpStyle { ensure_ascii: false, ..DumpStyle::indented(2) };
        assert_eq!(dumped(&PyValue::str("a\u{7f}\u{1}"), &raw), "\"a\u{7f}\\u0001\"");
    }

    /// Loose equality between parsed values.
    #[test]
    fn python_equality() {
        let v = |text: &str| parse(text).unwrap();
        assert!(py_eq(&v("true"), &v("1")));
        assert!(py_eq(&v("1"), &v("1.0")));
        assert!(!py_eq(&v("1"), &v("\"1\"")));
        assert!(py_eq(&v(r#"{"a": 1, "b": [false]}"#), &v(r#"{"b": [0], "a": 1.0}"#)));
        assert!(!py_eq(&v(r#"{"a": 1}"#), &v(r#"{"a": 1, "b": 2}"#)));
        assert!(!py_eq(&v("NaN"), &v("NaN")));
        assert!(py_eq(&v("10000000000000000303786028427003666890752"), &v("1e40")));
        assert!(!py_eq(&v("10000000000000000000000000000000000000000"), &v("1e40")));
        assert!(py_eq(&v("-0.0"), &v("0")));
    }

    /// Integer and text conversion of wreckfest2 udp entries.
    #[test]
    fn python_int_and_str() {
        let v = |text: &str| parse(text).unwrap();
        assert_eq!(py_int(&v("\" 1 \"")), Some(PyInt::Small(1)));
        assert_eq!(py_int(&v("\"1_0\"")), Some(PyInt::Small(10)));
        assert_eq!(py_int(&v("\"\\u0661\"")), Some(PyInt::Small(1)));
        assert_eq!(py_int(&v("\"1.0\"")), None);
        assert_eq!(py_int(&v("1.9")), Some(PyInt::Small(1)));
        assert_eq!(py_int(&v("NaN")), None);
        assert_eq!(py_int(&v("Infinity")), None);
        assert_eq!(py_int(&v("true")), Some(PyInt::Small(1)));
        assert_eq!(py_int(&v("null")), None);
        assert!(py_str_is(&v("\"20777\""), "20777"));
        assert!(py_str_is(&v("20777"), "20777"));
        assert!(!py_str_is(&v("20777.0"), "20777"));
    }
}

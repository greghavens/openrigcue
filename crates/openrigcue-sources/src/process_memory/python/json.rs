//! A lenient JSON decoder: the standard grammar plus the `NaN`, `Infinity` and `-Infinity` literals,
//! numbers with a fraction or exponent parsed as floats (so `1e400` is infinity), duplicate object keys
//! resolved to the last value, and control characters rejected inside strings.

use super::int_from_str;

/// A decoded JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn native(&self) -> openrigcue_model::native_value::LegacyValue {
        use openrigcue_model::native_value::{Dict, LegacyValue};
        match self {
            Json::Null => LegacyValue::None,
            Json::Bool(b) => LegacyValue::Bool(*b),
            Json::Int(n) => LegacyValue::Int(*n),
            Json::Float(n) => LegacyValue::Float(*n),
            Json::Str(text) => LegacyValue::Str(text.clone()),
            Json::Array(values) => LegacyValue::List(values.iter().map(Json::native).collect()),
            Json::Object(values) => {
                let mut dict = Dict::new();
                for (key, value) in values {
                    dict.insert(key.as_str(), value.native());
                }
                LegacyValue::Dict(dict)
            }
        }
    }

    /// `value[key]` on an object (the last duplicate wins); `None` where the lookup fails.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// `value[index]` on an array.
    pub fn index(&self, index: usize) -> Option<&Json> {
        match self {
            Json::Array(items) => items.get(index),
            _ => None,
        }
    }

    /// The value as a number (a boolean counts as 0 or 1); `None` for every other type.
    pub fn number(&self) -> Option<f64> {
        match *self {
            Json::Bool(b) => Some(f64::from(u8::from(b))),
            Json::Int(i) => Some(i as f64),
            Json::Float(f) => Some(f),
            _ => None,
        }
    }

    /// Integer conversion: truncates floats, parses strings; `None` where the conversion fails.
    pub fn python_int(&self) -> Option<f64> {
        match self {
            Json::Float(f) => f.is_finite().then(|| f.trunc()),
            Json::Str(s) => int_from_str(s).map(|i| i as f64),
            other => other.number(),
        }
    }

    /// Integer conversion into the shared bounded integer representation.
    pub(crate) fn native_integer(&self) -> Option<openrigcue_model::native_value::LegacyValue> {
        use openrigcue_model::native_value::LegacyValue;
        match self {
            Json::Int(n) => Some(LegacyValue::Int(*n)),
            Json::Bool(b) => Some(LegacyValue::Int(i64::from(*b))),
            Json::Float(n) => crate::udp_racing::native_int(*n),
            Json::Str(text) => {
                let normalized = openrigcue_model::python_numeric::python_int_text(text)?;
                normalized
                    .parse::<i64>()
                    .map(LegacyValue::Int)
                    .ok()
                    .or_else(|| normalized.parse::<f64>().ok().filter(|n| n.is_finite()).map(LegacyValue::Float))
            }
            _ => None,
        }
    }

    /// The value a dispatcher channel ends up with when a listener returns this value unchanged: numbers as they
    /// are, strings converted to integers (the dispatcher's loader converts string answers); `None` otherwise.
    pub fn channel_value(&self) -> Option<f64> {
        match self {
            Json::Str(s) => int_from_str(s).map(|i| i as f64),
            other => other.number(),
        }
    }

    /// Truthiness: `null`, `false`, zero, and empty strings, arrays and objects are false; everything
    /// else, NaN included, is true.
    pub fn is_truthy(&self) -> bool {
        match self {
            Json::Null => false,
            Json::Bool(b) => *b,
            Json::Int(i) => *i != 0,
            Json::Float(f) => *f != 0.0,
            Json::Str(s) => !s.is_empty(),
            Json::Array(items) => !items.is_empty(),
            Json::Object(members) => !members.is_empty(),
        }
    }
}

/// Error position and reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid JSON at byte {position}: {reason}")]
pub struct JsonError {
    pub position: usize,
    pub reason: &'static str,
}

/// Decodes bytes: detects UTF-8 (with or without BOM), UTF-16 and UTF-32
/// from the first bytes, then parses the whole text.
pub fn loads_bytes(bytes: &[u8]) -> Result<Json, JsonError> {
    let text = decode(bytes).ok_or(JsonError { position: 0, reason: "undecodable bytes" })?;
    loads(&text)
}

/// Decodes text.
pub fn loads(text: &str) -> Result<Json, JsonError> {
    if text.starts_with('\u{feff}') {
        return Err(JsonError { position: 0, reason: "unexpected UTF-8 BOM" });
    }
    let mut parser = Parser { text: text.as_bytes(), pos: 0 };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != parser.text.len() {
        return Err(parser.error("extra data"));
    }
    Ok(value)
}

fn decode(bytes: &[u8]) -> Option<String> {
    let utf16 = |be: bool, body: &[u8]| -> Option<String> {
        if !body.len().is_multiple_of(2) {
            return None;
        }
        let units =
            body.chunks(2)
                .map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) });
        char::decode_utf16(units).collect::<Result<String, _>>().ok()
    };
    let utf32 = |be: bool, body: &[u8]| -> Option<String> {
        if !body.len().is_multiple_of(4) {
            return None;
        }
        body.chunks(4)
            .map(|c| {
                let raw = [c[0], c[1], c[2], c[3]];
                char::from_u32(if be { u32::from_be_bytes(raw) } else { u32::from_le_bytes(raw) })
            })
            .collect()
    };
    let utf8 = |body: &[u8]| std::str::from_utf8(body).ok().map(str::to_owned);
    // The order in which the encodings are tested matters.
    if let Some(rest) = bytes.strip_prefix(&[0x00, 0x00, 0xFE, 0xFF]) {
        return utf32(true, rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE, 0x00, 0x00]) {
        return utf32(false, rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(true, rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(false, rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return utf8(rest);
    }
    match *bytes {
        [0, b1, _, _, ..] => {
            if b1 != 0 {
                utf16(true, bytes)
            } else {
                utf32(true, bytes)
            }
        }
        [_, 0, b2, b3, ..] => {
            if b2 != 0 || b3 != 0 {
                utf16(false, bytes)
            } else {
                utf32(false, bytes)
            }
        }
        [0, _] => utf16(true, bytes),
        [_, 0] => utf16(false, bytes),
        _ => utf8(bytes),
    }
}

struct Parser<'a> {
    text: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, reason: &'static str) -> JsonError {
        JsonError { position: self.pos, reason }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, literal: &str) -> bool {
        if self.text[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Json, JsonError> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ if self.eat("null") => Ok(Json::Null),
            _ if self.eat("true") => Ok(Json::Bool(true)),
            _ if self.eat("false") => Ok(Json::Bool(false)),
            _ if self.eat("NaN") => Ok(Json::Float(f64::NAN)),
            _ if self.eat("Infinity") => Ok(Json::Float(f64::INFINITY)),
            _ => Err(self.error("expecting value")),
        }
    }

    fn object(&mut self) -> Result<Json, JsonError> {
        self.pos += 1;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.error("expecting property name"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.error("expecting ':'"));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value()?;
            members.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(members));
                }
                _ => return Err(self.error("expecting ',' delimiter")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, JsonError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.error("expecting ',' delimiter")),
            }
        }
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        if self.eat("-Infinity") {
            return Ok(Json::Float(f64::NEG_INFINITY));
        }
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.error("expecting value")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.text.get(self.pos + 1).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
            self.digits();
            is_float = true;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.digits();
                is_float = true;
            } else {
                self.pos = save;
            }
        }
        let literal = std::str::from_utf8(&self.text[start..self.pos]).map_err(|_| self.error("bad number"))?;
        if is_float {
            literal.parse().map(Json::Float).map_err(|_| self.error("bad number"))
        } else {
            match literal.parse::<i64>() {
                Ok(i) => Ok(Json::Int(i)),
                // An integer beyond the bounded range keeps its magnitude as a float.
                Err(_) => literal.parse().map(Json::Float).map_err(|_| self.error("bad number")),
            }
        }
    }

    fn digits(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let byte = self.peek().ok_or_else(|| self.error("unterminated string"))?;
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                0x00..=0x1F => return Err(self.error("invalid control character")),
                _ => {
                    // Copy one UTF-8 character (the input is a valid str).
                    let rest = std::str::from_utf8(&self.text[self.pos..]).map_err(|_| self.error("bad text"))?;
                    let c = rest.chars().next().ok_or_else(|| self.error("unterminated string"))?;
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        let byte = self.peek().ok_or_else(|| self.error("unterminated string"))?;
        self.pos += 1;
        let simple = match byte {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let unit = self.hex4()?;
                let code = if (0xD800..0xDC00).contains(&unit) && self.text[self.pos..].starts_with(b"\\u") {
                    let save = self.pos;
                    self.pos += 2;
                    let low = self.hex4()?;
                    if (0xDC00..0xE000).contains(&low) {
                        0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                    } else {
                        self.pos = save;
                        unit
                    }
                } else {
                    unit
                };
                // Rust strings cannot hold lone surrogates, so they become U+FFFD.
                char::from_u32(code).unwrap_or('\u{fffd}')
            }
            _ => return Err(self.error("invalid escape")),
        };
        out.push(simple);
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let digits = self.text.get(self.pos..self.pos + 4).ok_or_else(|| self.error("invalid \\u escape"))?;
        let text = std::str::from_utf8(digits).map_err(|_| self.error("invalid \\u escape"))?;
        if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(self.error("invalid \\u escape"));
        }
        self.pos += 4;
        u32::from_str_radix(text, 16).map_err(|_| self.error("invalid \\u escape"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decoding (War Thunder, bus simulators, V-Racer): NaN / -Infinity literals, `1e400` as
    /// infinity, the last duplicate key wins.
    #[test]
    fn parses_objects_and_python_extensions() {
        let v = loads(r#"{"a": 1, "b": [true, null, -2.5e1], "c": NaN, "d": -Infinity, "a": 3}"#).unwrap();
        assert_eq!(v.get("a"), Some(&Json::Int(3)));
        assert_eq!(v.get("b").and_then(|b| b.index(2)), Some(&Json::Float(-25.0)));
        assert!(v.get("c").and_then(Json::number).unwrap().is_nan());
        assert_eq!(v.get("d").and_then(Json::number), Some(f64::NEG_INFINITY));
        assert_eq!(loads("1e400").unwrap(), Json::Float(f64::INFINITY));
    }

    /// Rejected input: trailing data, raw control characters, leading zeros, single quotes,
    /// empty text, a BOM in text.
    #[test]
    fn rejects_what_python_rejects() {
        assert!(loads("{\"a\": 1} x").is_err());
        assert!(loads("\"tab\there\"").is_err());
        assert!(loads("01").is_err());
        assert!(loads("{'a': 1}").is_err());
        assert!(loads("").is_err());
        assert!(loads("\u{feff}1").is_err());
    }

    /// Decoding bytes with encoding detection: UTF-8 BOM and UTF-16-LE without BOM.
    #[test]
    fn decodes_bytes_like_detect_encoding() {
        assert_eq!(loads_bytes(b"\xEF\xBB\xBF[1]").unwrap(), Json::Array(vec![Json::Int(1)]));
        assert_eq!(loads_bytes(b"[\x001\x00]\x00").unwrap(), Json::Array(vec![Json::Int(1)]));
        assert_eq!(loads(r#""😀é""#).unwrap(), Json::Str("😀é".into()));
    }

    /// Integer conversion of a JSON value and the dispatcher loader's handling of a
    /// value returned unchanged (strings become integers).
    #[test]
    fn python_int_and_channel_values() {
        assert_eq!(Json::Float(12.9).python_int(), Some(12.0));
        assert_eq!(Json::Float(-12.9).python_int(), Some(-12.0));
        assert_eq!(Json::Str(" 7 ".into()).python_int(), Some(7.0));
        assert_eq!(Json::Bool(true).python_int(), Some(1.0));
        assert_eq!(Json::Null.python_int(), None);
        assert_eq!(Json::Str("x".into()).channel_value(), None);
        assert_eq!(Json::Float(2.5).channel_value(), Some(2.5));
    }

    /// Truthiness of decoded JSON values.
    #[test]
    fn truthiness() {
        for falsy in ["null", "false", "0", "0.0", "-0.0", "\"\"", "[]", "{}"] {
            assert!(!loads(falsy).unwrap().is_truthy(), "{falsy}");
        }
        for truthy in ["true", "2", "-1", "0.5", "NaN", "\"no\"", "[0]", "{\"a\": 0}"] {
            assert!(loads(truthy).unwrap().is_truthy(), "{truthy}");
        }
    }
}

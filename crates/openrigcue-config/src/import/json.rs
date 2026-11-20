//! A JSON text reader for `.srs` profile content, producing
//! [`LegacyValue`]s. It differs from serde_json in what it accepts: `NaN`, `Infinity` and
//! `-Infinity` are numbers, `-0` is the integer 0, a repeated object key keeps its first position
//! with the last value, and an integer literal of more than 4300 digits is refused.

use super::value::{BigInt, Dict, LegacyValue, NativeObject, PythonString};
use std::collections::BTreeMap;

/// Limit on the digits of an integer literal.
const MAX_INT_DIGITS: usize = 4300;

/// The default container nesting budget.
/// Callers already inside nested work can pass their remaining budget.
const MAX_DEPTH: usize = 3000;

/// Why a text did not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonError {
    /// The text is not valid JSON, or an integer literal exceeds the digit limit.
    #[error("invalid JSON at character {position}: {message}")]
    Invalid { position: usize, message: &'static str },
    #[error("maximum recursion depth exceeded while decoding a JSON {kind} from a unicode string")]
    Recursion { kind: &'static str },
    #[error("Exceeds the limit (4300 digits) for integer string conversion: value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit")]
    IntegerLimit { digits: usize },
}

/// Decodes one value, with surrounding JSON whitespace; a leading byte-order
/// mark is refused.
pub fn parse(text: &str) -> Result<LegacyValue, JsonError> {
    parse_with_recursion_budget(text, MAX_DEPTH)
}

/// Decode with the remaining nesting budget, without consuming the
/// Rust thread stack for every JSON container.
pub fn parse_with_recursion_budget(text: &str, remaining: usize) -> Result<LegacyValue, JsonError> {
    let chars: Vec<char> = text.chars().collect();
    if chars.first() == Some(&'\u{feff}') {
        return Err(invalid(0, "Unexpected UTF-8 BOM (decode using utf-8-sig)"));
    }
    let mut parser = Parser { chars: &chars, pos: 0, remaining, keys: BTreeMap::new() };
    parser.skip_whitespace();
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.pos != chars.len() {
        return Err(invalid(parser.pos, "Extra data"));
    }
    Ok(value)
}

fn invalid(position: usize, message: &'static str) -> JsonError {
    JsonError::Invalid { position, message }
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    remaining: usize,
    keys: BTreeMap<PythonString, NativeObject>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn starts_with(&self, word: &str) -> bool {
        word.chars().enumerate().all(|(i, c)| self.peek_at(i) == Some(c))
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    // Each frame spends one unit of the nesting budget; syntactic nesting is
    // held on the heap so exhausting the budget is an error, never a stack overflow.
    fn value(&mut self) -> Result<LegacyValue, JsonError> {
        enum Frame {
            Array(Vec<LegacyValue>),
            Object(Dict, Option<LegacyValue>),
        }
        let mut frames = Vec::new();
        let mut parsed = None;
        loop {
            if let Some(value) = parsed.take() {
                let Some(frame) = frames.last_mut() else {
                    return Ok(value);
                };
                match frame {
                    Frame::Array(values) => {
                        values.push(value);
                        self.skip_whitespace();
                        match self.peek() {
                            Some(']') => {
                                self.pos += 1;
                                let Frame::Array(v) = frames.pop().expect("array") else { unreachable!() };
                                parsed = Some(LegacyValue::List(v));
                            }
                            Some(',') => self.pos += 1,
                            _ => return Err(invalid(self.pos, "Expecting ',' delimiter")),
                        }
                    }
                    Frame::Object(values, key) => {
                        values.insert_native(key.take().expect("object key"), value).expect("string key");
                        self.skip_whitespace();
                        match self.peek() {
                            Some('}') => {
                                self.pos += 1;
                                let Frame::Object(v, _) = frames.pop().expect("object") else { unreachable!() };
                                parsed = Some(LegacyValue::Dict(v));
                            }
                            Some(',') => {
                                self.pos += 1;
                                *key = Some(self.object_key()?);
                            }
                            _ => return Err(invalid(self.pos, "Expecting ',' delimiter")),
                        }
                    }
                }
                continue;
            }
            self.skip_whitespace();
            match self.peek() {
                Some('[') | Some('{') => {
                    let object = self.peek() == Some('{');
                    if frames.len() >= self.remaining {
                        return Err(JsonError::Recursion { kind: if object { "object" } else { "array" } });
                    }
                    self.pos += 1;
                    self.skip_whitespace();
                    if self.peek() == Some(if object { '}' } else { ']' }) {
                        self.pos += 1;
                        parsed =
                            Some(if object { LegacyValue::Dict(Dict::new()) } else { LegacyValue::List(Vec::new()) });
                    } else if object {
                        let key = self.object_key()?;
                        frames.push(Frame::Object(Dict::new(), Some(key)));
                    } else {
                        frames.push(Frame::Array(Vec::new()));
                    }
                }
                Some('"') => {
                    self.pos += 1;
                    parsed = Some(LegacyValue::from_python_string(self.string()?));
                }
                _ => parsed = Some(self.scalar()?),
            }
        }
    }
    fn object_key(&mut self) -> Result<LegacyValue, JsonError> {
        self.skip_whitespace();
        if self.peek() != Some('"') {
            return Err(invalid(self.pos, "Expecting property name enclosed in double quotes"));
        }
        self.pos += 1;
        let key = self.string()?;
        self.skip_whitespace();
        if self.peek() != Some(':') {
            return Err(invalid(self.pos, "Expecting ':' delimiter"));
        }
        self.pos += 1;
        let key = self
            .keys
            .entry(key.clone())
            .or_insert_with(|| NativeObject::new(LegacyValue::from_python_string(key)))
            .clone();
        Ok(LegacyValue::Object(key))
    }

    fn scalar(&mut self) -> Result<LegacyValue, JsonError> {
        let words: [(&str, LegacyValue); 3] =
            [("null", LegacyValue::None), ("true", LegacyValue::Bool(true)), ("false", LegacyValue::Bool(false))];
        for (word, value) in words {
            if self.starts_with(word) {
                self.pos += word.chars().count();
                return Ok(value);
            }
        }
        if let Some(number) = self.number()? {
            return Ok(number);
        }
        let constants = [("NaN", f64::NAN), ("Infinity", f64::INFINITY), ("-Infinity", f64::NEG_INFINITY)];
        for (word, value) in constants {
            if self.starts_with(word) {
                self.pos += word.len();
                if value.is_nan() {
                    static NAN: std::sync::OnceLock<super::value::NativeObject> = std::sync::OnceLock::new();
                    return Ok(LegacyValue::Object(
                        NAN.get_or_init(|| super::value::NativeObject::new(LegacyValue::Float(f64::NAN))).clone(),
                    ));
                }
                return Ok(LegacyValue::Float(value));
            }
        }
        Err(invalid(self.pos, "Expecting value"))
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some('0'..='9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    /// A number (`-?(?:0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?`): a fraction or exponent
    /// makes a float, otherwise an integer. `None` when no number starts here.
    fn number(&mut self) -> Result<Option<LegacyValue>, JsonError> {
        let start = self.pos;
        let sign = usize::from(self.peek() == Some('-'));
        match self.peek_at(sign) {
            Some('0') => self.pos += sign + 1,
            Some('1'..='9') => {
                self.pos += sign;
                self.digits();
            }
            _ => return Ok(None),
        }
        let int_end = self.pos;
        if self.peek() == Some('.') && matches!(self.peek_at(1), Some('0'..='9')) {
            self.pos += 1;
            self.digits();
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let exponent_start = self.pos;
            let exponent_sign = usize::from(matches!(self.peek_at(1), Some('+' | '-')));
            self.pos += 1 + exponent_sign;
            if self.digits() == 0 {
                self.pos = exponent_start;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        if self.pos != int_end {
            let value = text.parse::<f64>().expect("NUMBER_RE text is a valid float literal");
            return Ok(Some(LegacyValue::Float(value)));
        }
        if text.len() - sign > MAX_INT_DIGITS {
            return Err(JsonError::IntegerLimit { digits: text.len() - sign });
        }
        let value = text.parse::<BigInt>().expect("validated decimal integer");
        Ok(Some(LegacyValue::from_integer(value)))
    }

    /// A string (control characters refused): from after the opening quote to after the closing one.
    fn string(&mut self) -> Result<PythonString, JsonError> {
        let begin = self.pos - 1;
        let mut out = Vec::new();
        loop {
            let c = self.peek().ok_or_else(|| invalid(begin, "Unterminated string starting at"))?;
            self.pos += 1;
            match c {
                '"' => return Ok(PythonString::from_codepoints(out).expect("validated Unicode codepoints")),
                '\\' => out.push(self.escape(begin)?),
                c if u32::from(c) < 0x20 => return Err(invalid(self.pos - 1, "Invalid control character at")),
                c => out.push(u32::from(c)),
            }
        }
    }

    fn escape(&mut self, begin: usize) -> Result<u32, JsonError> {
        let esc = self.peek().ok_or_else(|| invalid(begin, "Unterminated string starting at"))?;
        let simple = match esc {
            '"' => Some('"'),
            '\\' => Some('\\'),
            '/' => Some('/'),
            'b' => Some('\u{8}'),
            'f' => Some('\u{c}'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'u' => None,
            _ => return Err(invalid(self.pos, "Invalid \\escape")),
        };
        if let Some(c) = simple {
            self.pos += 1;
            return Ok(u32::from(c));
        }
        let unit = self.hex4(self.pos)?;
        self.pos += 5;
        let mut code = unit;
        if (0xd800..=0xdbff).contains(&unit) && self.peek() == Some('\\') && self.peek_at(1) == Some('u') {
            let low = self.hex4(self.pos + 1)?;
            if (0xdc00..=0xdfff).contains(&low) {
                code = 0x10000 + (((unit - 0xd800) << 10) | (low - 0xdc00));
                self.pos += 6;
            }
        }
        Ok(code)
    }

    /// The four hex digits after the `u` at `u_pos`.
    fn hex4(&self, u_pos: usize) -> Result<u32, JsonError> {
        let digits = self.chars.get(u_pos + 1..u_pos + 5).ok_or_else(|| invalid(u_pos, "Invalid \\uXXXX escape"))?;
        digits
            .iter()
            .try_fold(0, |acc, c| c.to_digit(16).map(|d| acc * 16 + d))
            .ok_or_else(|| invalid(u_pos, "Invalid \\uXXXX escape"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_invalid(text: &str) -> bool {
        matches!(parse(text), Err(JsonError::Invalid { .. }))
    }

    /// Literal words, float constants and number forms.
    #[test]
    fn scalars_as_python_reads_them() {
        assert_eq!(parse(" null "), Ok(LegacyValue::None));
        assert_eq!(parse("true"), Ok(LegacyValue::Bool(true)));
        assert_eq!(parse("-0"), Ok(LegacyValue::Int(0)), "int('-0') is 0");
        assert_eq!(
            parse("-0.0").map(|v| matches!(v, LegacyValue::Float(f) if f == 0.0 && f.is_sign_negative())),
            Ok(true)
        );
        assert_eq!(parse("12"), Ok(LegacyValue::Int(12)));
        assert_eq!(parse("1e2"), Ok(LegacyValue::Float(100.0)));
        assert_eq!(parse("1E+2"), Ok(LegacyValue::Float(100.0)));
        assert_eq!(parse("2.5"), Ok(LegacyValue::Float(2.5)));
        assert_eq!(parse("1e400"), Ok(LegacyValue::Float(f64::INFINITY)), "float('1e400') is inf");
        assert_eq!(parse("Infinity"), Ok(LegacyValue::Float(f64::INFINITY)));
        assert_eq!(parse("-Infinity"), Ok(LegacyValue::Float(f64::NEG_INFINITY)));
        assert!(matches!(parse("NaN").unwrap().resolved(),LegacyValue::Float(f)if f.is_nan()));
        for bad in
            ["", "01", "1.", ".5", "1e", "-", "+1", "nan", "inf", "[1,]", "{\"a\":1,}", "{'a':1}", "1 2", "\u{feff}{}"]
        {
            assert!(is_invalid(bad), "{bad:?}");
        }
    }

    /// Integers: the 4300-digit limit is invalid; a 64-bit overflow is valid JSON openrigcue cannot hold.
    #[test]
    fn integer_limits() {
        assert_eq!(parse(&"1".repeat(4301)), Err(JsonError::IntegerLimit { digits: 4301 }));
        assert_eq!(parse(&"1".repeat(4300)).unwrap().integer().unwrap().to_string(), "1".repeat(4300));
        assert_eq!(parse("-9223372036854775808"), Ok(LegacyValue::Int(i64::MIN)));
    }

    /// Strings: escapes, surrogate pairs, strict control characters.
    #[test]
    fn strings_as_scanstring_reads_them() {
        assert_eq!(parse(r#""a\"\\\/\b\f\n\r\t""#), Ok(LegacyValue::Str("a\"\\/\u{8}\u{c}\n\r\t".into())));
        assert_eq!(parse(r#""😀é""#), Ok(LegacyValue::Str("😀é".into())));
        assert_eq!(parse("\"\u{7f}é\""), Ok(LegacyValue::Str("\u{7f}é".into())), "DEL is not a control character here");
        assert!(is_invalid("\"a\tb\""));
        assert!(is_invalid(r#""\x""#));
        assert!(is_invalid(r#""\u12""#));
        assert!(is_invalid(r#""\ud83d\uZZZZ""#), "the second escape after a high surrogate is decoded too");
        assert!(is_invalid("\"abc"));
        assert_eq!(parse(r#""\ud83d""#).unwrap().python_string().unwrap().codepoints(), &[0xd83d]);
        assert_eq!(parse(r#""\ud83dA""#).unwrap().python_string().unwrap().codepoints(), &[0xd83d, 65]);
    }

    /// Objects: a repeated key keeps its first position and takes the last value.
    #[test]
    fn objects_and_arrays() {
        let v = parse(r#"{ "b": 1, "a": [true, {}], "b": 2 }"#).unwrap();
        let d = v.as_dict().unwrap();
        assert_eq!(d.str_keys(), ["b", "a"]);
        assert_eq!(d.get("b"), Some(&LegacyValue::Int(2)));
        assert_eq!(d.get("a"), Some(&LegacyValue::List(vec![LegacyValue::Bool(true), LegacyValue::Dict(Dict::new())])));
        assert!(is_invalid(r#"{"a" 1}"#));
        assert!(is_invalid(r#"{1: 2}"#));
        let deep = "[[[]]]";
        assert!(matches!(parse_with_recursion_budget(deep, 2), Err(JsonError::Recursion { kind: "array" })));
    }
}

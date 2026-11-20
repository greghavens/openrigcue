//! Parser for a flat dict literal, the payload format of the Farming Simulator telemetry mod
//! (`{"avX1": 0.1234, ..., }`).
//!
//! Supported: a `{...}` display with string keys (single or double quotes, the usual escapes, adjacent literals
//! concatenated) and values that are numbers (decimal, hexadecimal, octal, binary and float literals, one optional
//! sign, underscores between digits), strings, `True`, `False` and `None`; whitespace and a trailing comma.
//! Container values and other literal forms remain outside this parser's supported payload subset.

use std::collections::HashMap;

/// A literal value of the payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Number(f64),
    Bool(bool),
    None,
    Str(String),
}

impl Literal {
    /// The value in arithmetic (a boolean counts as 0 or 1); `None` where arithmetic is an error.
    pub fn number(&self) -> Option<f64> {
        match *self {
            Literal::Int(n) => Some(n as f64),
            Literal::Number(n) => Some(n),
            Literal::Bool(b) => Some(f64::from(u8::from(b))),
            _ => None,
        }
    }

    pub fn native(&self) -> openrigcue_model::native_value::LegacyValue {
        use openrigcue_model::native_value::LegacyValue;
        match self {
            Literal::Int(n) => LegacyValue::Int(*n),
            Literal::Number(n) => LegacyValue::Float(*n),
            Literal::Bool(b) => LegacyValue::Bool(*b),
            Literal::None => LegacyValue::None,
            Literal::Str(s) => LegacyValue::Str(s.clone()),
        }
    }
}

/// Why a payload was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a dict literal at offset {position}")]
pub struct LiteralError {
    pub position: usize,
}

/// Parses the payload into its key/value map (a repeated key keeps the last value).
pub fn parse_dict(text: &str) -> Result<HashMap<String, Literal>, LiteralError> {
    // Leading spaces and tabs are stripped before parsing.
    let text = text.trim_start_matches([' ', '\t']);
    let mut parser = Parser { chars: text.char_indices().collect(), pos: 0 };
    parser.skip_ws();
    parser.expect('{')?;
    let mut map = HashMap::new();
    loop {
        parser.skip_ws();
        if parser.peek() == Some('}') {
            parser.pos += 1;
            break;
        }
        let key = parser.strings()?;
        parser.skip_ws();
        parser.expect(':')?;
        parser.skip_ws();
        let value = parser.value()?;
        map.insert(key, value);
        parser.skip_ws();
        match parser.peek() {
            Some(',') => parser.pos += 1,
            Some('}') => {
                parser.pos += 1;
                break;
            }
            _ => return Err(parser.error()),
        }
    }
    parser.skip_ws();
    if parser.pos != parser.chars.len() {
        return Err(parser.error());
    }
    Ok(map)
}

struct Parser {
    chars: Vec<(usize, char)>,
    pos: usize,
}

impl Parser {
    fn error(&self) -> LiteralError {
        LiteralError { position: self.chars.get(self.pos).map_or(usize::MAX, |&(offset, _)| offset) }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).map(|&(_, c)| c)
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).map(|&(_, c)| c)
    }

    fn expect(&mut self, c: char) -> Result<(), LiteralError> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error())
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r' | '\x0c')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Literal, LiteralError> {
        match self.peek() {
            Some('"' | '\'') => self.strings().map(Literal::Str),
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let start = self.pos;
                while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    self.pos += 1;
                }
                let word: String = self.chars[start..self.pos].iter().map(|&(_, c)| c).collect();
                match word.as_str() {
                    "True" => Ok(Literal::Bool(true)),
                    "False" => Ok(Literal::Bool(false)),
                    "None" => Ok(Literal::None),
                    _ => {
                        self.pos = start;
                        Err(self.error())
                    }
                }
            }
            Some('-' | '+') => {
                let negative = self.peek() == Some('-');
                self.pos += 1;
                self.skip_ws();
                self.number(negative)
            }
            _ => self.number(false),
        }
    }

    /// One or more adjacent string literals, concatenated.
    fn strings(&mut self) -> Result<String, LiteralError> {
        let mut out = self.string()?;
        loop {
            let save = self.pos;
            self.skip_ws();
            if matches!(self.peek(), Some('"' | '\'')) {
                out.push_str(&self.string()?);
            } else {
                self.pos = save;
                return Ok(out);
            }
        }
    }

    fn string(&mut self) -> Result<String, LiteralError> {
        let quote = self.peek().filter(|c| *c == '"' || *c == '\'').ok_or_else(|| self.error())?;
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or_else(|| self.error())?;
            self.pos += 1;
            match c {
                c if c == quote => return Ok(out),
                '\n' => return Err(self.error()),
                '\\' => self.escape(&mut out)?,
                c => out.push(c),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), LiteralError> {
        let c = self.peek().ok_or_else(|| self.error())?;
        self.pos += 1;
        let push = match c {
            '\n' => return Ok(()),
            '\\' | '\'' | '"' => c,
            'a' => '\u{7}',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{b}',
            'x' => self.hex_escape(2)?,
            'u' => self.hex_escape(4)?,
            'U' => self.hex_escape(8)?,
            '0'..='7' => {
                let mut value = c.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match self.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            value = value * 8 + d;
                            self.pos += 1;
                        }
                        None => break,
                    }
                }
                char::from_u32(value).ok_or_else(|| self.error())?
            }
            other => {
                // Unknown escapes are kept verbatim.
                out.push('\\');
                other
            }
        };
        out.push(push);
        Ok(())
    }

    fn hex_escape(&mut self, len: usize) -> Result<char, LiteralError> {
        let digits: String = (0..len).filter_map(|i| self.peek_at(i)).collect();
        if digits.len() != len || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(self.error());
        }
        self.pos += len;
        u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32).ok_or_else(|| self.error())
    }

    fn number(&mut self, negative: bool) -> Result<Literal, LiteralError> {
        let start = self.pos;
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X' | 'o' | 'O' | 'b' | 'B')) {
            let radix = match self.peek_at(1) {
                Some('x' | 'X') => 16,
                Some('o' | 'O') => 8,
                _ => 2,
            };
            self.pos += 2;
            let digits = self.digit_run(|c| c.is_digit(radix), true)?;
            return u64::from_str_radix(&digits, radix)
                .map(|v| {
                    let signed = if negative { -i128::from(v) } else { i128::from(v) };
                    i64::try_from(signed).map_or_else(|_| Literal::Number(signed as f64), Literal::Int)
                })
                .map_err(|_| self.error());
        }
        let mut literal = String::new();
        let mut is_float = false;
        if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            literal.push_str(&self.digit_run(|c| c.is_ascii_digit(), false)?);
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            is_float = true;
            literal.push('.');
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                literal.push_str(&self.digit_run(|c| c.is_ascii_digit(), false)?);
            }
        }
        if literal.is_empty() || literal == "." {
            self.pos = start;
            return Err(self.error());
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.pos += 1;
            is_float = true;
            literal.push('e');
            if let Some(sign @ ('+' | '-')) = self.peek() {
                literal.push(sign);
                self.pos += 1;
            }
            literal.push_str(&self.digit_run(|c| c.is_ascii_digit(), false)?);
        }
        if self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return Err(self.error());
        }
        if !is_float && literal.len() > 1 && literal.starts_with('0') && literal.bytes().any(|b| b != b'0') {
            // Leading zeros in decimal integers ("007") are rejected.
            return Err(self.error());
        }
        if !is_float && literal.len() > openrigcue_model::python_numeric::DEFAULT_INT_MAX_STR_DIGITS {
            return Err(self.error());
        }
        if negative {
            literal.insert(0, '-');
        }
        if !is_float {
            if let Ok(integer) = literal.parse::<i64>() {
                return Ok(Literal::Int(integer));
            }
        }
        literal.parse().map(Literal::Number).map_err(|_| self.error())
    }

    /// Digits with single underscores between them (and, after a radix prefix, before the first one).
    fn digit_run(&mut self, is_digit: impl Fn(char) -> bool, leading_underscore: bool) -> Result<String, LiteralError> {
        let mut out = String::new();
        if leading_underscore && self.peek() == Some('_') {
            self.pos += 1;
        }
        loop {
            match self.peek() {
                Some(c) if is_digit(c) => {
                    out.push(c);
                    self.pos += 1;
                }
                Some('_') if self.peek_at(1).is_some_and(&is_digit) && !out.is_empty() => self.pos += 1,
                _ => break,
            }
        }
        if out.is_empty() {
            Err(self.error())
        } else {
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape the FS25 TelemetryMod writes (`string.format('"%s": %.4f, ', ...)` between braces).
    #[test]
    fn parses_mod_payload_with_trailing_comma() {
        let map = parse_dict(r#"{"avX1": 0.1234, "lvZ1": -0.0000, "rpm": 850.0000, "speed": 12.5000, }"#).unwrap();
        assert_eq!(map["avX1"], Literal::Number(0.1234));
        assert_eq!(map["lvZ1"].number(), Some(-0.0));
        assert_eq!(map["rpm"].number(), Some(850.0));
        assert_eq!(map.len(), 4);
    }

    /// A dict literal: underscores, `.5`, exponents,
    /// True/None, implicit string concatenation, hex, last duplicate key wins.
    #[test]
    fn python_literal_forms() {
        let map =
            parse_dict("  {'a': 1_000, 'b': .5, 'c': 1e3, 'd': True, 'e': None, 'f': 'x' \"y\", 'g': 0x1F, 'a': -2}")
                .unwrap();
        assert_eq!(map["a"], Literal::Int(-2));
        assert_eq!(map["b"], Literal::Number(0.5));
        assert_eq!(map["c"], Literal::Number(1000.0));
        assert_eq!(map["d"].number(), Some(1.0));
        assert_eq!(map["e"].number(), None);
        assert_eq!(map["f"], Literal::Str("xy".into()));
        assert_eq!(map["g"], Literal::Int(31));
    }

    #[test]
    fn integer_kinds_negative_zero_minimum_and_decimal_limit() {
        let values =
            parse_dict("{'zero': -0, 'float': -0.0, 'min': -9223372036854775808, 'hex': -0x8000000000000000}").unwrap();
        assert_eq!(values["zero"], Literal::Int(0));
        assert_eq!(values["min"], Literal::Int(i64::MIN));
        assert_eq!(values["hex"], Literal::Int(i64::MIN));
        assert!(matches!(values["float"], Literal::Number(v) if v.to_bits() == (-0.0f64).to_bits()));
        assert_eq!(parse_dict(&format!("{{'zero': {}}}", "0".repeat(4300))).unwrap()["zero"], Literal::Int(0));
        assert!(parse_dict(&format!("{{'zero': {}}}", "0".repeat(4301))).is_err());
    }

    /// Lua's `%.4f` of NaN or infinity is not a numeric literal: the payload is rejected.
    #[test]
    fn rejects_non_literals() {
        assert!(parse_dict(r#"{"a": nan, }"#).is_err());
        assert!(parse_dict(r#"{"a": -nan(ind), }"#).is_err());
        assert!(parse_dict(r#"{"a": inf}"#).is_err());
        assert!(parse_dict(r#"{"a": 1"#).is_err());
        assert!(parse_dict(r#"{"a": 007}"#).is_err());
        assert!(parse_dict(r#"{"a": 1} 2"#).is_err());
        assert!(parse_dict("").is_err());
    }
}

//! Evaluation of a dictionary literal, for the Epic Roller Coasters listener.
//!
//! The listener rewrites the game's text into a dictionary literal and evaluates it. This module accepts
//! every form such a text can take: string literals with any quote style,
//! `r`/`u` prefixes, escape sequences and implicit concatenation; integer, float and boolean values; signed
//! numbers; parentheses; comments, line continuations and ASCII whitespace. Values
//! this crate cannot represent (bytes, complex numbers, containers, integers too large for
//! a float, `\N{...}` escapes and lone surrogates) are reported as [`LiteralError::Unsupported`].

use openrigcue_model::native_value::LegacyValue;

/// A key or value of the evaluated dictionary.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A text.
    Str(String),
    /// An integer, float or boolean, converted to a float.
    Number(f64),
    /// A retained typed literal outside the scalar float conversion (currently None).
    Native(LegacyValue),
}

/// Why the text was not turned into a dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralError {
    /// The text is not a valid literal, or it is not a dictionary.
    Rejected,
    /// The text is valid, but a key or value is outside [`Literal`].
    Unsupported,
}

type Parsed<T> = Result<T, LiteralError>;

/// Evaluates a text that must be a dictionary literal; the entries in display order, with only
/// text keys kept (no channel name matches another key type; a repeated key appears once per occurrence).
pub fn literal_eval_dict(text: &str) -> Parsed<Vec<(String, Literal)>> {
    let mut p = Parser { chars: text.chars().collect(), at: 0 };
    if p.chars.contains(&'\0') {
        return Err(LiteralError::Rejected);
    }
    // Leading spaces and tabs are stripped first.
    while matches!(p.peek(), Some(' ' | '\t')) {
        p.at += 1;
    }
    p.skip_space()?;
    if p.peek() != Some('{') {
        return Err(LiteralError::Rejected);
    }
    p.at += 1;
    let mut entries = Vec::new();
    loop {
        p.skip_space()?;
        if p.eat('}') {
            break;
        }
        let key = p.value()?;
        p.skip_space()?;
        if !p.eat(':') {
            return Err(LiteralError::Rejected);
        }
        p.skip_space()?;
        let value = p.value()?;
        if let Literal::Str(key) = key {
            entries.push((key, value));
        }
        p.skip_space()?;
        if p.eat(',') {
            continue;
        }
        p.skip_space()?;
        if !p.eat('}') {
            return Err(LiteralError::Rejected);
        }
        break;
    }
    p.skip_space()?;
    if p.at == p.chars.len() {
        Ok(entries)
    } else {
        Err(LiteralError::Rejected)
    }
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// Spaces, tabs, form feeds, newlines and comments (the whole text is one bracketed expression, or blank
    /// lines around it). A backslash followed by a newline joins lines.
    fn skip_space(&mut self) -> Parsed<()> {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\x0c' | '\n' | '\r') => self.at += 1,
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n' | '\r')) {
                        self.at += 1;
                    }
                }
                Some('\\') => match self.peek_at(1) {
                    Some('\n') => self.at += 2,
                    Some('\r') => self.at += if self.peek_at(2) == Some('\n') { 3 } else { 2 },
                    _ => return Err(LiteralError::Rejected),
                },
                _ => return Ok(()),
            }
        }
    }

    /// One expression accepted as a value or key.
    fn value(&mut self) -> Parsed<Literal> {
        match self.peek() {
            Some('(') => {
                self.at += 1;
                self.skip_space()?;
                if self.peek() == Some(')') {
                    return Err(LiteralError::Unsupported); // ()
                }
                let inner = self.value()?;
                self.skip_space()?;
                match self.peek() {
                    Some(')') => {
                        self.at += 1;
                        Ok(inner)
                    }
                    Some(',') => Err(LiteralError::Unsupported), // a tuple
                    _ => Err(LiteralError::Rejected),
                }
            }
            // A container: it may be valid, but it is not representable here.
            Some('[' | '{') => Err(LiteralError::Unsupported),
            Some('+' | '-') => self.signed_number(),
            Some(c) if c.is_ascii_digit() => self.number(),
            Some('.') if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => self.number(),
            Some(c) if is_identifier_start(c) => self.name_or_string(),
            Some('\'' | '"') => self.strings(),
            _ => Err(LiteralError::Rejected),
        }
    }

    fn signed_number(&mut self) -> Parsed<Literal> {
        let negative = self.peek() == Some('-');
        self.at += 1;
        self.skip_space()?;
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {}
            Some('.') if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => {}
            // One sign is accepted, on a number literal only (`--1`, `-True` and `-"x"` are rejected).
            _ => return Err(LiteralError::Rejected),
        }
        match self.number()? {
            Literal::Number(n) => Ok(Literal::Number(if negative { -n } else { n })),
            Literal::Str(_) | Literal::Native(_) => Err(LiteralError::Rejected),
        }
    }

    /// A number token; an imaginary literal (complex) or a `num ± complex` sum is unsupported.
    fn number(&mut self) -> Parsed<Literal> {
        let start = self.at;
        let value = if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X' | 'o' | 'O' | 'b' | 'B')) {
            let radix = match self.peek_at(1) {
                Some('x' | 'X') => 16,
                Some('o' | 'O') => 8,
                _ => 2,
            };
            self.at += 2;
            let digits = self.digit_run(|c| c.is_digit(radix), true)?;
            if digits.is_empty() {
                return Err(LiteralError::Rejected);
            }
            let int = u128::from_str_radix(&digits, radix).map_err(|_| LiteralError::Unsupported)?;
            int as f64
        } else {
            let mut text = self.digit_run(|c| c.is_ascii_digit(), false)?;
            let integer_part = text.clone();
            let mut is_float = false;
            if self.peek() == Some('.') {
                self.at += 1;
                is_float = true;
                text.push('.');
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    text.push_str(&self.digit_run(|c| c.is_ascii_digit(), false)?);
                }
            }
            if matches!(self.peek(), Some('e' | 'E'))
                && (self.peek_at(1).is_some_and(|c| c.is_ascii_digit())
                    || (matches!(self.peek_at(1), Some('+' | '-'))
                        && self.peek_at(2).is_some_and(|c| c.is_ascii_digit())))
            {
                is_float = true;
                text.push('e');
                self.at += 1;
                if let Some(sign @ ('+' | '-')) = self.peek() {
                    text.push(sign);
                    self.at += 1;
                }
                text.push_str(&self.digit_run(|c| c.is_ascii_digit(), false)?);
            }
            if matches!(self.peek(), Some('j' | 'J')) {
                return Err(LiteralError::Unsupported);
            }
            if !is_float {
                // Decimal integers other than zero may not start with 0 (`01` is rejected).
                if integer_part.len() > 1 && integer_part.starts_with('0') && integer_part.bytes().any(|b| b != b'0') {
                    return Err(LiteralError::Rejected);
                }
            }
            let parsed: f64 = text.parse().map_err(|_| LiteralError::Rejected)?;
            if !is_float && parsed.is_infinite() {
                // The integer is too large for a float: it stays an integer this crate cannot hold.
                return Err(LiteralError::Unsupported);
            }
            parsed
        };
        if self.peek().is_some_and(is_identifier_continue) {
            return Err(LiteralError::Rejected);
        }
        debug_assert!(self.at > start);
        // `1 + 2j` is the only binary operation allowed; it builds a complex number.
        let save = self.at;
        self.skip_space()?;
        if matches!(self.peek(), Some('+' | '-')) {
            return Err(LiteralError::Unsupported);
        }
        self.at = save;
        Ok(Literal::Number(value))
    }

    /// Digits with single `_` separators between them; `after_prefix` allows one `_` right after a
    /// radix prefix.
    fn digit_run(&mut self, is_digit: impl Fn(char) -> bool, after_prefix: bool) -> Parsed<String> {
        let mut out = String::new();
        let mut last_underscore = after_prefix;
        if after_prefix && self.peek() == Some('_') {
            self.at += 1;
            last_underscore = true;
        } else if after_prefix {
            last_underscore = false;
        }
        while let Some(c) = self.peek() {
            if is_digit(c) {
                out.push(c);
                last_underscore = false;
                self.at += 1;
            } else if c == '_' && !out.is_empty() && !last_underscore {
                if !self.peek_at(1).is_some_and(&is_digit) {
                    return Err(LiteralError::Rejected);
                }
                last_underscore = true;
                self.at += 1;
            } else {
                break;
            }
        }
        if last_underscore && !out.is_empty() {
            return Err(LiteralError::Rejected);
        }
        Ok(out)
    }

    /// `True`, `False`, `None`, or a string prefix; any other name is rejected.
    fn name_or_string(&mut self) -> Parsed<Literal> {
        let start = self.at;
        while self.peek().is_some_and(is_identifier_continue) {
            self.at += 1;
        }
        let name: String = self.chars[start..self.at].iter().collect();
        if matches!(self.peek(), Some('\'' | '"')) && is_string_prefix(&name) {
            self.at = start;
            return self.strings();
        }
        match name.as_str() {
            "True" => Ok(Literal::Number(1.0)),
            "False" => Ok(Literal::Number(0.0)),
            "None" => Ok(Literal::Native(LegacyValue::None)),
            _ => Err(LiteralError::Rejected),
        }
    }

    /// One or more adjacent string literals, concatenated.
    fn strings(&mut self) -> Parsed<Literal> {
        let mut text = String::new();
        loop {
            text.push_str(&self.string_literal()?);
            let save = self.at;
            self.skip_space()?;
            let next_is_string = match self.peek() {
                Some('\'' | '"') => true,
                Some(c) if is_identifier_start(c) => {
                    let mut end = self.at;
                    while self.chars.get(end).copied().is_some_and(is_identifier_continue) {
                        end += 1;
                    }
                    let name: String = self.chars[self.at..end].iter().collect();
                    matches!(self.chars.get(end), Some('\'' | '"')) && is_string_prefix(&name)
                }
                _ => false,
            };
            if !next_is_string {
                self.at = save;
                return Ok(Literal::Str(text));
            }
        }
    }

    /// One string literal with its prefix.
    fn string_literal(&mut self) -> Parsed<String> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.at += 1;
        }
        let prefix: String = self.chars[start..self.at].iter().collect::<String>().to_ascii_lowercase();
        let raw = prefix.contains('r');
        if prefix.contains('b') {
            return Err(LiteralError::Unsupported); // bytes
        }
        if prefix.contains('f') || prefix.contains('t') {
            return Err(LiteralError::Rejected); // a formatted string is not a literal
        }
        let quote = self.peek().ok_or(LiteralError::Rejected)?;
        let triple = self.peek_at(1) == Some(quote) && self.peek_at(2) == Some(quote);
        self.at += if triple { 3 } else { 1 };
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or(LiteralError::Rejected)?;
            if c == quote {
                if !triple {
                    self.at += 1;
                    return Ok(out);
                }
                if self.peek_at(1) == Some(quote) && self.peek_at(2) == Some(quote) {
                    self.at += 3;
                    return Ok(out);
                }
                out.push(c);
                self.at += 1;
                continue;
            }
            if matches!(c, '\n' | '\r') && !triple {
                return Err(LiteralError::Rejected);
            }
            if c != '\\' {
                // `\r\n` and `\r` are read as `\n`.
                if c == '\r' {
                    self.at += if self.peek_at(1) == Some('\n') { 2 } else { 1 };
                    out.push('\n');
                } else {
                    out.push(c);
                    self.at += 1;
                }
                continue;
            }
            let next = self.peek_at(1).ok_or(LiteralError::Rejected)?;
            if raw {
                // The backslash stays; it only stops the next character from ending the literal.
                out.push('\\');
                self.at += 1;
                if next == '\r' {
                    self.at += if self.peek_at(1) == Some('\n') { 2 } else { 1 };
                    out.push('\n');
                } else {
                    out.push(next);
                    self.at += 1;
                }
                continue;
            }
            self.at += 2;
            match next {
                '\n' => {}
                '\r' => {
                    if self.peek() == Some('\n') {
                        self.at += 1;
                    }
                }
                '\\' | '\'' | '"' => out.push(next),
                'a' => out.push('\x07'),
                'b' => out.push('\x08'),
                'f' => out.push('\x0c'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'v' => out.push('\x0b'),
                '0'..='7' => {
                    let mut value = next.to_digit(8).unwrap_or(0);
                    for _ in 0..2 {
                        match self.peek().and_then(|c| c.to_digit(8)) {
                            Some(d) => {
                                value = value * 8 + d;
                                self.at += 1;
                            }
                            None => break,
                        }
                    }
                    out.push(char::from_u32(value).ok_or(LiteralError::Unsupported)?);
                }
                'x' => out.push(self.hex_escape(2)?),
                'u' => out.push(self.hex_escape(4)?),
                'U' => out.push(self.hex_escape(8)?),
                'N' => return Err(LiteralError::Unsupported),
                other => {
                    // An unknown escape keeps its backslash (not an error).
                    out.push('\\');
                    out.push(other);
                }
            }
        }
    }

    /// `\xhh`, `\uXXXX`, `\UXXXXXXXX`: exactly `len` hex digits. Code points above U+10FFFF are rejected; a
    /// surrogate is a valid character this crate cannot hold.
    fn hex_escape(&mut self, len: usize) -> Parsed<char> {
        let mut value: u32 = 0;
        for _ in 0..len {
            let digit = self.peek().and_then(|c| c.to_digit(16)).ok_or(LiteralError::Rejected)?;
            value = value * 16 + digit;
            self.at += 1;
        }
        if value > 0x10FFFF {
            return Err(LiteralError::Rejected);
        }
        char::from_u32(value).ok_or(LiteralError::Unsupported)
    }
}

/// The prefixes a string literal may carry (any case).
fn is_string_prefix(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "r" | "u" | "b" | "br" | "rb" | "f" | "fr" | "rf" | "t" | "tr" | "rt")
}

fn is_identifier_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_identifier_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(text: &str) -> Parsed<Vec<(String, Literal)>> {
        literal_eval_dict(text)
    }

    fn s(key: &str, value: &str) -> (String, Literal) {
        (key.into(), Literal::Str(value.into()))
    }

    #[test]
    fn the_rewrites_shape() {
        assert_eq!(eval("{\"speed\": \"12.3\", \"yaw\": \"1.0\"}"), Ok(vec![s("speed", "12.3"), s("yaw", "1.0")]));
        assert_eq!(eval("{\"a\": \"1\", }"), Ok(vec![s("a", "1")]));
        assert_eq!(eval("{}"), Ok(vec![]));
    }

    #[test]
    fn string_literal_forms() {
        // Quotes, prefixes, escapes and implicit concatenation.
        assert_eq!(eval("{'a': 'x', \"b\": '''y\"z'''}"), Ok(vec![s("a", "x"), s("b", "y\"z")]));
        assert_eq!(eval("{\"a\": \"1\" '2' u\"3\"}"), Ok(vec![s("a", "123")]));
        assert_eq!(eval(r#"{"a": "\x31\u0032\063\n\q"}"#), Ok(vec![s("a", "123\n\\q")]));
        assert_eq!(eval(r#"{"a": r"\"x"}"#), Ok(vec![s("a", "\\\"x")]));
        assert_eq!(eval("{\"a\": \"x\\\ny\"}"), Ok(vec![s("a", "xy")]));
        assert_eq!(eval("{\"a\": (\"x\")}"), Ok(vec![s("a", "x")]));
    }

    #[test]
    fn numbers_booleans_comments_and_layout() {
        assert_eq!(
            eval("{\"a\": 1, \"b\": -2.5e1, \"c\": True, # note\n \"d\": 0x_1F, \"e\": 1_000, \"f\": .5}"),
            Ok(vec![
                ("a".into(), Literal::Number(1.0)),
                ("b".into(), Literal::Number(-25.0)),
                ("c".into(), Literal::Number(1.0)),
                ("d".into(), Literal::Number(31.0)),
                ("e".into(), Literal::Number(1000.0)),
                ("f".into(), Literal::Number(0.5)),
            ])
        );
        assert_eq!(eval("{1: \"x\", \"k\": False}"), Ok(vec![("k".into(), Literal::Number(0.0))]));
    }

    #[test]
    fn none_values_and_keys_are_valid_python_literals() {
        assert_eq!(
            eval("{None: 'ignored', 'nullable': (None), 'text': 'None'}"),
            Ok(vec![
                ("nullable".into(), Literal::Native(LegacyValue::None)),
                ("text".into(), Literal::Str("None".into()))
            ])
        );
        for text in ["{'a':-None}", "{'a':None 'text'}", "{'a':none}"] {
            assert_eq!(eval(text), Err(LiteralError::Rejected), "{text}");
        }
    }

    #[test]
    fn python_rejects() {
        for text in [
            "{\"a\": \"1\"",
            "{\"a\" \"1\"}",
            "{\"a\": x}",
            "{\"a\": --1}",
            "{\"a\": 01}",
            "{\"a\": 1_}",
            "{\"a\": 1__0}",
            "{\"a\": f\"x\"}",
            "{\"a\": \"\\x1\"}",
            "{\"a\": \"x\ny\"}",
            "{\"a\": \"1\"} 2",
            "\"a\"",
            "{\"a\": \"1\"}\0",
            "{\"a\":\u{a0}\"1\"}",
            "{\"a\": 1abc}",
        ] {
            assert_eq!(eval(text), Err(LiteralError::Rejected), "{text:?}");
        }
    }

    #[test]
    fn unrepresented_objects_are_unsupported() {
        for text in [
            "{\"a\": b\"1\"}",
            "{\"a\": 1j}",
            "{\"a\": 1 + 2j}",
            "{\"a\": [1]}",
            "{\"a\": (\"1\",)}",
            "{\"a\": {}}",
            "{\"a\": \"\\N{DIGIT ONE}\"}",
            "{\"a\": \"\\ud800\"}",
        ] {
            assert_eq!(eval(text), Err(LiteralError::Unsupported), "{text:?}");
        }
    }
}

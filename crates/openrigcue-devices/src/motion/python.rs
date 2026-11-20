//! How values and errors read in the messages the motion drivers print or log: error texts,
//! quoted text, byte strings and string maps.

use std::io;

/// The hexadecimal integer in `bytes`, when it fits an `i64`. Byte input
/// accepts ASCII digits/letters and six ASCII whitespace bytes, not
/// the Unicode grammar of text input. Base 16 has no decimal digit limit.
pub fn int_hex_bytes(bytes: &[u8]) -> Option<i64> {
    parse_hex_bytes(bytes).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HexParseError {
    InvalidLiteral,
    /// Valid integer syntax whose value does not fit an `i64`.
    OutOfRange,
}

pub(crate) fn parse_hex_bytes(bytes: &[u8]) -> Result<i64, HexParseError> {
    let text = std::str::from_utf8(bytes).map_err(|_| HexParseError::InvalidLiteral)?;
    if !text.is_ascii() {
        return Err(HexParseError::InvalidLiteral);
    }
    let text = text.trim_matches([' ', '\t', '\n', '\r', '\u{b}', '\u{c}']);
    let (sign, digits) = match text.as_bytes().first().ok_or(HexParseError::InvalidLiteral)? {
        b'-' => ("-", &text[1..]),
        b'+' => ("+", &text[1..]),
        _ => ("", text),
    };
    let digits = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
        .map_or(digits, |rest| rest.strip_prefix('_').unwrap_or(rest));
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'_')
    {
        return Err(HexParseError::InvalidLiteral);
    }
    i64::from_str_radix(&format!("{sign}{}", digits.replace('_', "")), 16).map_err(|_| HexParseError::OutOfRange)
}

/// The error of a strict UTF-8 decode of `bytes`, as `UnicodeDecodeError: <message>`: the first
/// undecodable byte (or bytes, when a sequence stops short) with its position and the reason.
pub fn utf8_decode_error(bytes: &[u8], error: &std::str::Utf8Error) -> String {
    let start = error.valid_up_to();
    let (len, reason) = match error.error_len() {
        None => (bytes.len() - start, "unexpected end of data"),
        Some(len) if len > 1 || matches!(bytes[start], 0xC2..=0xF4) => (len, "invalid continuation byte"),
        Some(len) => (len, "invalid start byte"),
    };
    let place = if len == 1 {
        format!("byte 0x{:02x} in position {start}", bytes[start])
    } else {
        format!("bytes in position {start}-{}", start + len - 1)
    };
    format!("UnicodeDecodeError: 'utf-8' codec can't decode {place}: {reason}")
}

/// The bytes of hexadecimal `text`: pairs of hexadecimal digits with ASCII whitespace allowed between pairs.
/// `Err` is the position of the offending character:
/// the first character that is not ASCII when there is one, else the first that is not a digit where one is
/// needed (the end of the text for a lone last digit).
pub fn bytes_fromhex(text: &str) -> Result<Vec<u8>, usize> {
    let raw: Vec<char> = text.chars().collect();
    if let Some(position) = raw.iter().position(|c| !c.is_ascii()) {
        return Err(position);
    }
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}');
    let digit = |i: usize| raw.get(i).and_then(|c| c.to_digit(16)).ok_or(i);
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if is_space(raw[i]) {
            i += 1;
            continue;
        }
        out.push((digit(i)? * 16 + digit(i + 1)?) as u8);
        i += 2;
    }
    Ok(out)
}

/// `<class>: <message>` for the socket error `e`: `TimeoutError: timed out` for
/// a call that ran into the socket's timeout, else the error class of the error number
/// (`ConnectionRefusedError`, `ConnectionResetError`, `BrokenPipeError`, …) with `[Errno N] <strerror>`. The
/// error text is the platform's.
pub fn socket_exception(e: &io::Error) -> String {
    use io::ErrorKind as Kind;
    let class = match e.kind() {
        Kind::WouldBlock | Kind::TimedOut => "TimeoutError",
        Kind::ConnectionRefused => "ConnectionRefusedError",
        Kind::ConnectionReset => "ConnectionResetError",
        Kind::ConnectionAborted => "ConnectionAbortedError",
        Kind::BrokenPipe => "BrokenPipeError",
        Kind::PermissionDenied => "PermissionError",
        Kind::Interrupted => "InterruptedError",
        _ => "OSError",
    };
    format!("{class}: {}", os_error_str(e))
}

/// The error of an ASCII decode of `bytes`, as `UnicodeDecodeError: <message>`; `None` when every
/// byte is ASCII.
pub fn ascii_decode_error(bytes: &[u8]) -> Option<String> {
    let position = bytes.iter().position(|byte| !byte.is_ascii())?;
    Some(format!(
        "UnicodeDecodeError: 'ascii' codec can't decode byte 0x{:02x} in position {position}: ordinal not in \
         range(128)",
        bytes[position]
    ))
}

/// The message of the system error `e`: `'timed out'` for a socket timeout (of a
/// receive or connect), `'[Errno N] <strerror>'` for a system error, the error's own
/// text otherwise.
pub fn os_error_str(e: &io::Error) -> String {
    if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) {
        return "timed out".into();
    }
    match e.raw_os_error() {
        Some(code) => {
            let text = e.to_string();
            let strerror = text.strip_suffix(&format!(" (os error {code})")).unwrap_or(&text);
            format!("[Errno {code}] {strerror}")
        }
        None => e.to_string(),
    }
}

/// The message of the serial link error `e`: the write-timeout text, the port-not-open
/// text for a closed port, the system error text of a system error.
pub fn link_error_str(e: &crate::serial::LinkError) -> String {
    use crate::serial::LinkError;
    match e {
        LinkError::Io(io) if io.kind() == io::ErrorKind::TimedOut => "Write timeout".into(),
        LinkError::Io(io) if io.kind() == io::ErrorKind::NotConnected => io.to_string(),
        LinkError::Io(io) => os_error_str(io),
        LinkError::Port(port) => port.description.clone(),
        other => other.to_string(),
    }
}

/// The quote of a quoted form: `'`, unless the text holds `'` and no `"`.
fn quote_for(has_single: bool, has_double: bool) -> char {
    if has_single && !has_double {
        '"'
    } else {
        '\''
    }
}

/// The quoted form of `text`, with escapes.
pub fn str_repr(text: &str) -> String {
    let quote = quote_for(text.contains('\''), text.contains('"'));
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The printed form of a byte string: `b'…'` with printable ASCII as is and `\xNN` for the rest.
pub fn bytes_repr(data: &[u8]) -> String {
    let quote = quote_for(data.contains(&b'\''), data.contains(&b'"'));
    let mut out = String::with_capacity(data.len() + 3);
    out.push('b');
    out.push(quote);
    for &b in data {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b if char::from(b) == quote => {
                out.push('\\');
                out.push(char::from(b));
            }
            0x20..=0x7e => out.push(char::from(b)),
            b => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push(quote);
    out
}

/// The printed form of a map whose keys and values are all text, in insertion order.
pub fn str_dict_repr(entries: &[(&str, &str)]) -> String {
    let items: Vec<String> = entries.iter().map(|(k, v)| format!("{}: {}", str_repr(k), str_repr(v))).collect();
    format!("{{{}}}", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strict UTF-8 decoding: the reason and the byte (or byte range) reported.
    #[test]
    fn utf8_decode_error_reports_cpythons_position_and_reason() {
        let message = |bytes: &[u8]| {
            let error = std::str::from_utf8(bytes).unwrap_err();
            utf8_decode_error(bytes, &error)
        };
        let prefix = "UnicodeDecodeError: 'utf-8' codec can't decode ";
        for (bytes, rest) in [
            (&b"\x80"[..], "byte 0x80 in position 0: invalid start byte"),
            (b"\xc0\x80", "byte 0xc0 in position 0: invalid start byte"),
            (b"ab\xe2\x28", "byte 0xe2 in position 2: invalid continuation byte"),
            (b"\xed\xa0\x80", "byte 0xed in position 0: invalid continuation byte"),
            (b"\xe2\x82\x28", "bytes in position 0-1: invalid continuation byte"),
            (b"a\xc3", "byte 0xc3 in position 1: unexpected end of data"),
            (b"\xe2\x82", "bytes in position 0-1: unexpected end of data"),
            (b"ok\xf0\x9f\x98", "bytes in position 2-4: unexpected end of data"),
        ] {
            assert_eq!(message(bytes), format!("{prefix}{rest}"), "{bytes:02x?}");
        }
    }

    /// Hexadecimal text: a character that is not ASCII is reported before any earlier invalid one.
    #[test]
    fn fromhex_error_position() {
        assert_eq!(bytes_fromhex(" 0a ff\n"), Ok(vec![0x0A, 0xFF]));
        assert_eq!(bytes_fromhex("0G"), Err(1));
        assert_eq!(bytes_fromhex("a"), Err(1));
        assert_eq!(bytes_fromhex("zz\u{e9}"), Err(2));
        assert_eq!(bytes_fromhex("0a f"), Err(4));
    }

    /// A timeout prints `timed out`; a system error prints `[Errno N] strerror`.
    #[test]
    fn os_errors_read_as_python_prints_them() {
        assert_eq!(os_error_str(&io::Error::from(io::ErrorKind::WouldBlock)), "timed out");
        let refused = io::Error::from_raw_os_error(111);
        assert_eq!(os_error_str(&refused), "[Errno 111] Connection refused");
    }

    /// Quoted forms of text and byte strings: quote choice, escapes.
    #[test]
    fn reprs() {
        assert_eq!(str_repr("abc"), "'abc'");
        assert_eq!(str_repr("it's"), "\"it's\"");
        assert_eq!(str_repr("a'\"b"), "'a\\'\"b'");
        assert_eq!(bytes_repr(b"ok\x00\xff\n"), "b'ok\\x00\\xff\\n'");
        assert_eq!(bytes_repr(b""), "b''");
        assert_eq!(str_dict_repr(&[("A", "1"), ("B", "x")]), "{'A': '1', 'B': 'x'}");
    }
}

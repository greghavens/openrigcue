//! `unicode_escape` and `raw_unicode_escape` decoding of complete input. Invalid escapes keep
//! their backslash.

use super::{names, Errors, NativeException};

fn hex_digit(c: u8) -> Option<u32> {
    char::from(c).to_digit(16)
}

/// Decodes `data` as `unicode_escape` with the given error handler.
pub(super) fn unicode_escape(data: &[u8], errors: Errors) -> Result<Vec<u32>, NativeException> {
    let end = data.len();
    let mut out = Vec::with_capacity(end);
    let mut s = 0;
    while s < end {
        let c = data[s];
        s += 1;
        if c != b'\\' {
            out.push(u32::from(c));
            continue;
        }
        let start = s - 1;
        if s >= end {
            let (replacement, position) = errors.handle("unicodeescape", data, start, end, "\\ at end of string")?;
            out.extend(replacement);
            s = position;
            continue;
        }
        let c = data[s];
        s += 1;
        let simple = match c {
            b'\n' => Some(None),
            b'\\' => Some(Some(b'\\')),
            b'\'' => Some(Some(b'\'')),
            b'"' => Some(Some(b'"')),
            b'b' => Some(Some(8)),
            b'f' => Some(Some(12)),
            b't' => Some(Some(9)),
            b'n' => Some(Some(10)),
            b'r' => Some(Some(13)),
            b'v' => Some(Some(11)),
            b'a' => Some(Some(7)),
            _ => None,
        };
        if let Some(simple) = simple {
            out.extend(simple.map(u32::from));
            continue;
        }
        let failure: (usize, &str) = match c {
            b'0'..=b'7' => {
                let mut ch = u32::from(c - b'0');
                for _ in 0..2 {
                    match data.get(s) {
                        Some(&d @ b'0'..=b'7') => {
                            ch = (ch << 3) + u32::from(d - b'0');
                            s += 1;
                        }
                        _ => break,
                    }
                }
                out.push(ch);
                continue;
            }
            b'x' | b'u' | b'U' => {
                let (count, message) = match c {
                    b'x' => (2, "truncated \\xXX escape"),
                    b'u' => (4, "truncated \\uXXXX escape"),
                    _ => (8, "truncated \\UXXXXXXXX escape"),
                };
                let mut ch = 0u32;
                let mut failed = None;
                for _ in 0..count {
                    if s >= end {
                        failed = Some((end, message));
                        break;
                    }
                    let Some(digit) = hex_digit(data[s]) else {
                        failed = Some((s, message));
                        break;
                    };
                    ch = (ch << 4) + digit;
                    s += 1;
                }
                match failed {
                    Some(failure) => failure,
                    None if ch > 0x10FFFF => (s, "illegal Unicode character"),
                    None => {
                        out.push(ch);
                        continue;
                    }
                }
            }
            b'N' => {
                let message = "malformed \\N character escape";
                if s >= end {
                    (end, message)
                } else if data[s] == b'{' {
                    s += 1;
                    let name_start = s;
                    while s < end && data[s] != b'}' {
                        s += 1;
                    }
                    if s >= end {
                        (end, message)
                    } else if s == name_start {
                        (s, message)
                    } else {
                        let name = &data[name_start..s];
                        s += 1;
                        match names::lookup(name) {
                            Some(ch) => {
                                out.push(ch);
                                continue;
                            }
                            None => (s, "unknown Unicode character name"),
                        }
                    }
                } else {
                    (s, message)
                }
            }
            _ => {
                out.push(u32::from(b'\\'));
                out.push(u32::from(c));
                continue;
            }
        };
        let (failure_end, reason) = failure;
        let (replacement, position) = errors.handle("unicodeescape", data, start, failure_end, reason)?;
        out.extend(replacement);
        s = position;
    }
    Ok(out)
}

/// Decodes `data` as `raw_unicode_escape` with the given error handler.
pub(super) fn raw_unicode_escape(data: &[u8], errors: Errors) -> Result<Vec<u32>, NativeException> {
    let end = data.len();
    let mut out = Vec::with_capacity(end);
    let mut s = 0;
    while s < end {
        let c = data[s];
        s += 1;
        if c != b'\\' || s >= end {
            out.push(u32::from(c));
            continue;
        }
        let start = s - 1;
        let c = data[s];
        s += 1;
        let (count, message) = match c {
            b'u' => (4, "truncated \\uXXXX escape"),
            b'U' => (8, "truncated \\UXXXXXXXX escape"),
            _ => {
                out.push(u32::from(b'\\'));
                out.push(u32::from(c));
                continue;
            }
        };
        let mut ch = 0u32;
        let mut failure = None;
        for _ in 0..count {
            if s >= end {
                failure = Some((end, message));
                break;
            }
            let Some(digit) = hex_digit(data[s]) else {
                failure = Some((s, message));
                break;
            };
            ch = (ch << 4) + digit;
            s += 1;
        }
        let failure = match failure {
            Some(failure) => failure,
            None if ch > 0x10FFFF => (s, "\\Uxxxxxxxx out of range"),
            None => {
                out.push(ch);
                continue;
            }
        };
        let (replacement, position) = errors.handle("rawunicodeescape", data, start, failure.0, failure.1)?;
        out.extend(replacement);
        s = position;
    }
    Ok(out)
}

//! Protocol-zero text number loaders, including the signed 32-bit fast path for integers.

use super::{BigInt, LegacyValue, PickleError};
use num_traits::ToPrimitive;

fn ascii_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn nul_prefix(text: &str) -> &str {
    text.split('\0').next().unwrap_or_default()
}

// The error text is built from at most 200 input bytes, and the quoted
// representation of that text is limited to 200 characters a second time.
fn c_literal_repr(text: &str) -> String {
    let text = nul_prefix(text);
    let mut end = text.len().min(200);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    openrigcue_model::python_repr::repr_str(&text[..end]).chars().take(200).collect()
}

/// INT first tries a signed 32-bit parse with automatic base detection.
/// That parse accepts a leading-zero octal literal, unlike the arbitrary-width fallback.
fn c_long(text: &str) -> Option<i32> {
    if text.starts_with('\0') {
        return Some(0);
    }
    let text = nul_prefix(text).trim_start_matches(ascii_space);
    let (negative, text) = if let Some(text) = text.strip_prefix('-') {
        (true, text)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let (base, digits) = if text.starts_with("0x") || text.starts_with("0X") {
        (16, &text[2..])
    } else if text.starts_with('0') {
        (8, text)
    } else {
        (10, text)
    };
    if digits.is_empty() {
        return None;
    }
    // Unlike the arbitrary-width parse, the 32-bit parse does not consume trailing whitespace.
    let magnitude =
        digits.chars().try_fold(0_u64, |n, c| n.checked_mul(base.into())?.checked_add(u64::from(c.to_digit(base)?)))?;
    let signed = if negative { -i128::from(magnitude) } else { i128::from(magnitude) };
    i32::try_from(signed).ok()
}

enum ParseError {
    Invalid(u32),
    Digits(usize),
    Width,
}

fn python_long(text: &str) -> Result<BigInt, ParseError> {
    python_long_base(text, 0)
}

fn python_long_base(text: &str, requested_base: u32) -> Result<BigInt, ParseError> {
    let text = nul_prefix(text).trim_matches(ascii_space);
    let (negative, digits) = if let Some(text) = text.strip_prefix('-') {
        (true, text)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let (base, digits, leading_zero) = if (requested_base == 0 || requested_base == 16)
        && (digits.starts_with("0x") || digits.starts_with("0X"))
    {
        (16_u32, digits[2..].strip_prefix('_').unwrap_or(&digits[2..]), false)
    } else if (requested_base == 0 || requested_base == 8) && (digits.starts_with("0o") || digits.starts_with("0O")) {
        (8, digits[2..].strip_prefix('_').unwrap_or(&digits[2..]), false)
    } else if (requested_base == 0 || requested_base == 2) && (digits.starts_with("0b") || digits.starts_with("0B")) {
        (2, digits[2..].strip_prefix('_').unwrap_or(&digits[2..]), false)
    } else if requested_base == 0 {
        (10, digits, digits.starts_with('0'))
    } else {
        (requested_base, digits, false)
    };
    let mut count = 0;
    let mut previous_digit = false;
    let mut nonzero = false;
    let mut magnitude = BigInt::from(0);
    for byte in digits.bytes() {
        if byte == b'_' {
            if !previous_digit {
                return Err(ParseError::Invalid(base));
            }
            previous_digit = false;
            continue;
        }
        let digit = char::from(byte).to_digit(base).ok_or(ParseError::Invalid(base))?;
        nonzero |= digit != 0;
        count += 1;
        previous_digit = true;
        magnitude = magnitude * base + digit;
    }
    if !previous_digit {
        return Err(ParseError::Invalid(base));
    }
    if !base.is_power_of_two() && count > openrigcue_model::python_numeric::DEFAULT_INT_MAX_STR_DIGITS {
        return Err(ParseError::Digits(count));
    }
    if leading_zero && nonzero {
        return Err(ParseError::Invalid(0));
    }
    Ok(if negative { -magnitude } else { magnitude })
}

pub(super) fn based_integer(text: &str, original: &LegacyValue, base: u32) -> Result<LegacyValue, PickleError> {
    let normalized = match original {
        LegacyValue::Str(_) => text
            .chars()
            .map(|c| {
                if !c.is_ascii() && c.is_whitespace() {
                    ' '
                } else {
                    openrigcue_model::python_numeric::decimal_digit(c).map_or(c, |n| char::from(b'0' + n as u8))
                }
            })
            .collect::<String>(),
        _ => text.to_owned(),
    };
    let parsed =
        if normalized.contains('\0') { Err(ParseError::Invalid(base)) } else { python_long_base(&normalized, base) };
    parsed.map(LegacyValue::from_integer).map_err(|error| match error {
        ParseError::Width => PickleError::Malformed("integer wider than 64 bits".into()),
        ParseError::Invalid(base) => PickleError::Constructor {
            class: "ValueError",
            message: format!("invalid literal for int() with base {base}: {}", openrigcue_model::python_repr::repr(original)),
        },
        ParseError::Digits(count) => PickleError::Constructor {
            class: "ValueError",
            message: format!("Exceeds the limit (4300 digits) for integer string conversion: value has {count} digits; use sys.set_int_max_str_digits() to increase the limit"),
        },
    })
}

/// GET/PUT parse a base-10 integer, then narrow it to a signed index.
/// The grammar is ASCII only; other Unicode digits are not accepted.
pub(super) fn memo_index(line: &str) -> Result<i64, PickleError> {
    if line.is_empty() {
        return Err(PickleError::Truncated);
    }
    let input = format!("{line}\n");
    python_long_base(&input, 10).and_then(|value|value.to_i64().ok_or(ParseError::Width)).map_err(|error| {
        let (class, message) = match error {
            ParseError::Invalid(_) => (
                "ValueError",
                format!(
                    "invalid literal for int() with base 10: {}",
                    c_literal_repr(&input)
                ),
            ),
            ParseError::Digits(count) => (
                "ValueError",
                format!(
                    "Exceeds the limit (4300 digits) for integer string conversion: value has {count} digits; use sys.set_int_max_str_digits() to increase the limit"
                ),
            ),
            ParseError::Width => ("OverflowError", "Python int too large to convert to C ssize_t".into()),
        };
        PickleError::Constructor { class, message }
    })
}

pub(super) fn integer(line: &str, long: bool) -> Result<LegacyValue, PickleError> {
    if line.is_empty() {
        return Err(PickleError::Truncated);
    }
    if !long {
        if let Some(value) = c_long(line) {
            return Ok(if line.len() == 2 && matches!(value, 0 | 1) {
                LegacyValue::Bool(value != 0)
            } else {
                LegacyValue::Int(i64::from(value))
            });
        }
    }
    // LONG removes one terminal L, not every L. Otherwise the newline
    // remains part of the parsed text and of its error representation.
    let input = if long {
        line.strip_suffix('L').map_or_else(|| format!("{line}\n"), str::to_owned)
    } else {
        format!("{line}\n")
    };
    match python_long(&input) {
        Ok(value) => Ok(LegacyValue::from_integer(value)),
        Err(ParseError::Width) => Err(PickleError::Malformed("integer wider than 64 bits".into())),
        Err(error) => {
            let message = if !long {
                "could not convert string to int".into()
            } else {
                match error {
                    ParseError::Invalid(base) => format!(
                        "invalid literal for int() with base {base}: {}",
                        c_literal_repr(&input)
                    ),
                    ParseError::Digits(count) => format!(
                        "Exceeds the limit (4300 digits) for integer string conversion: value has {count} digits; use sys.set_int_max_str_digits() to increase the limit"
                    ),
                    ParseError::Width => unreachable!(),
                }
            };
            Err(PickleError::Constructor { class: "ValueError", message })
        }
    }
}

/// FLOAT parses the longest float prefix, reporting overflow as an error,
/// then requires that the suffix start at newline or NUL.
pub(super) fn float(line: &str) -> Result<LegacyValue, PickleError> {
    if line.is_empty() {
        return Err(PickleError::Truncated);
    }
    let text = nul_prefix(line);
    let bytes = text.as_bytes();
    let mut end = usize::from(bytes.first().is_some_and(|b| matches!(b, b'+' | b'-')));
    let unsigned = text.get(end..).unwrap_or_default();
    let lower = unsigned.to_ascii_lowercase();
    let special = if lower.starts_with("infinity") {
        Some(8)
    } else if lower.starts_with("inf") || lower.starts_with("nan") {
        Some(3)
    } else {
        None
    };
    if let Some(size) = special {
        end += size;
    } else {
        let start = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        let mut digits = end - start;
        if bytes.get(end) == Some(&b'.') {
            end += 1;
            let start = end;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            digits += end - start;
        }
        if digits == 0 {
            let input = nul_prefix(&format!("{line}\n")).chars().take(200).collect::<String>();
            return Err(PickleError::Constructor {
                class: "ValueError",
                message: format!("could not convert string to float: '{input}'"),
            });
        }
        if bytes.get(end).is_some_and(|b| matches!(b, b'e' | b'E')) {
            let exponent = end;
            end += 1;
            if bytes.get(end).is_some_and(|b| matches!(b, b'+' | b'-')) {
                end += 1;
            }
            let start = end;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                end = exponent;
            }
        }
    }
    let value: f64 = text[..end].parse().map_err(|_| PickleError::Malformed("FLOAT conversion".into()))?;
    if special.is_none() && value.is_infinite() {
        let input = nul_prefix(&format!("{line}\n")).chars().take(200).collect::<String>();
        return Err(PickleError::Constructor {
            class: "OverflowError",
            message: format!("value too large to convert to float: '{input}'"),
        });
    }
    if end != text.len() {
        return Err(PickleError::Constructor {
            class: "ValueError",
            message: "could not convert string to float".into(),
        });
    }
    Ok(LegacyValue::Float(value))
}

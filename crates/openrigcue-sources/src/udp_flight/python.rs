//! Number parsing and conversion used by the flight listeners.
//!
//! The listeners parse number text, truncate to integers and round to a number of decimals; the helpers here
//! define those conversions for the inputs the games send.

use std::borrow::Cow;
use std::f64::consts::PI;

/// Whitespace: Rust's `char::is_whitespace` plus the ASCII separators
/// U+001C..U+001F.
pub(crate) fn is_python_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// The first code point of every run of ten Unicode decimal digits (general category `Nd`, values 0..9 in
/// order) of the Unicode 15.1 database, ASCII `0` included.
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50, 0xED0,
    0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620,
    0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0,
    0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50,
    0x16A60, 0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950,
    0x1FBF0,
];

/// The value of a Unicode decimal digit.
fn decimal_value(c: char) -> Option<u8> {
    let code = u32::from(c);
    DECIMAL_ZEROS.iter().find(|&&zero| (zero..zero + 10).contains(&code)).map(|&zero| (code - zero) as u8)
}

/// The normalisation applied to number text before
/// parsing: every Unicode whitespace character becomes a space, every Unicode decimal digit its ASCII digit, and
/// any other non-ASCII character `?` (which no number accepts).
fn decimal_and_space_to_ascii(text: &str) -> Cow<'_, str> {
    if text.is_ascii() {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| match c {
                c if c.is_ascii() => c,
                c if is_python_space(c) => ' ',
                c => decimal_value(c).map_or('?', |digit| char::from(b'0' + digit)),
            })
            .collect(),
    )
}

/// Parses float text: surrounding whitespace is ignored, `_` is accepted between two digits, and
/// `inf`, `infinity` and `nan` (any case, optional sign) are accepted; Unicode digits and spaces count as their
/// ASCII forms. Returns `None` for any other text.
pub(crate) fn parse_float(text: &str) -> Option<f64> {
    let text = decimal_and_space_to_ascii(text);
    let trimmed = text.trim();
    if !trimmed.is_ascii() {
        return None;
    }
    let digits = remove_digit_separators(trimmed)?;
    digits.parse::<f64>().ok()
}

/// Parses base-10 integer text: surrounding whitespace, a sign and `_` between digits are
/// accepted. Returns `None` for any other text (or when the value does not fit an `i64`).
pub(crate) fn parse_int(text: &str) -> Option<i64> {
    match parse_int_text(text)? {
        PyInt::Small(value) => Some(value),
        PyInt::Large => None,
    }
}

/// The integer that integer text produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PyInt {
    Small(i64),
    /// Valid, but outside the `i64` range.
    Large,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntTextError {
    Invalid,
    DigitLimit(usize),
}

impl std::fmt::Display for IntTextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid integer literal"),
            Self::DigitLimit(digits) => write!(
                f,
                "Exceeds the limit ({} digits) for integer string conversion: value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit",
                openrigcue_model::python_numeric::DEFAULT_INT_MAX_STR_DIGITS
            ),
        }
    }
}

/// Parses base-10 integer text (Unicode digits and spaces count as their ASCII forms), telling a value
/// too large for `i64` apart from rejected text (`None`).
pub(crate) fn parse_int_text(text: &str) -> Option<PyInt> {
    parse_int_text_checked(text).ok()
}

/// Keeps the digit-limit error separate from invalid syntax for callers that print it.
pub(crate) fn parse_int_text_checked(text: &str) -> Result<PyInt, IntTextError> {
    let text = decimal_and_space_to_ascii(text);
    let trimmed = text.trim();
    let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        return Err(IntTextError::Invalid);
    }
    let digits = remove_digit_separators(trimmed).ok_or(IntTextError::Invalid)?;
    let count = digits.trim_start_matches(['+', '-']).len();
    if count > openrigcue_model::python_numeric::DEFAULT_INT_MAX_STR_DIGITS {
        return Err(IntTextError::DigitLimit(count));
    }
    Ok(digits.parse().map_or(PyInt::Large, PyInt::Small))
}

/// Drops `_` separators, rejecting any `_` that is not between two ASCII digits.
fn remove_digit_separators(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'_' {
            let before = i.checked_sub(1).map(|j| bytes[j].is_ascii_digit()).unwrap_or(false);
            let after = bytes.get(i + 1).map(u8::is_ascii_digit).unwrap_or(false);
            if !(before && after) {
                return None;
            }
        } else {
            out.push(b as char);
        }
    }
    Some(out)
}

/// Truncates a float toward zero; `None` for NaN and infinity.
pub(crate) fn truncate(x: f64) -> Option<f64> {
    x.is_finite().then(|| x.trunc())
}

/// Rounds a float: the exact binary value rounded half-to-even to `digits`
/// decimals. Non-finite values are returned unchanged.
pub(crate) fn round_to(x: f64, digits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.digits$}").parse().unwrap_or(x)
}

/// Radians to degrees: `radians * 180.0 / π`, in that order.
pub(crate) fn rad_to_deg(radians: f64) -> f64 {
    radians * 180.0 / PI
}

/// Folds a bank angle beyond ±90° back into ±90°, as every flight listener does before publishing roll:
/// `180 - r` above 90, `-180 - r` below -90.
pub(crate) fn fold_roll(roll: f64) -> f64 {
    if roll > 90.0 {
        180.0 - roll
    } else if roll < -90.0 {
        -180.0 - roll
    } else {
        roll
    }
}

/// Linear rescale of `x` from
/// `[in_min, in_max]` to `[out_min, out_max]`, then clamped to the output range (upper bound first; a NaN result
/// becomes `out_max`).
pub(crate) fn map_range(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> f64 {
    let scaled = (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
    let below_max = if scaled <= out_max { scaled } else { out_max };
    if out_min <= below_max {
        below_max
    } else {
        out_min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_accepts_what_python_float_accepts() {
        assert_eq!(parse_float(" 1.25\n"), Some(1.25));
        assert_eq!(parse_float("1_000.5"), Some(1000.5));
        assert_eq!(parse_float("-.5"), Some(-0.5));
        assert_eq!(parse_float("1e3"), Some(1000.0));
        assert_eq!(parse_float("Infinity"), Some(f64::INFINITY));
        assert!(parse_float("-nan").unwrap().is_nan());
        assert_eq!(parse_float("\u{1f}2\u{1c}"), None);
    }

    #[test]
    fn float_rejects_what_python_float_rejects() {
        for bad in ["", " ", "1__0", "_1", "1_", "1._5", "0x10", "1,5", "abc", "SpitfireLFMkIX"] {
            assert_eq!(parse_float(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn int_parses_like_python_int() {
        assert_eq!(parse_int(" 20777 "), Some(20777));
        assert_eq!(parse_int("+1_000"), Some(1000));
        assert_eq!(parse_int("-5"), Some(-5));
        for bad in ["", "+", "1.0", "0x10", "1__0", "abc"] {
            assert_eq!(parse_int(bad), None, "{bad:?}");
        }
        assert_eq!(parse_int_text("99999999999999999999"), Some(PyInt::Large));
        assert_eq!(parse_int("99999999999999999999"), None);
        assert_eq!(parse_int_text("7"), Some(PyInt::Small(7)));
        assert_eq!(parse_int_text("7.5"), None);
        let boundary = format!("+{}7", "０_".repeat(4299));
        assert_eq!(parse_int_text(&boundary), Some(PyInt::Small(7)));
        assert_eq!(parse_int_text(&format!("0{boundary}")), None, "a sign in the digits is invalid");
        assert_eq!(parse_int_text(&format!("-{}7", "０_".repeat(4300))), None, "leading zeroes count toward the limit");
        assert_eq!(parse_int_text(&"9".repeat(4300)), Some(PyInt::Large));
        assert_eq!(parse_int_text(&"9".repeat(4301)), None);
    }

    #[test]
    fn unicode_digits_and_spaces_count_as_ascii() {
        // Number text is normalised first: Unicode 15.1 decimal digits and spaces become ASCII
        assert_eq!(parse_int("\u{661}\u{662}"), Some(12), "Arabic-Indic digits");
        assert_eq!(parse_int("\u{3000}\u{FF17}\u{2003}"), Some(7), "ideographic space, fullwidth 7, em space");
        assert_eq!(parse_int("\u{1D7CE}\u{1D7D9}"), Some(1), "mathematical bold 0, double-struck 1");
        assert_eq!(parse_int("1\u{2082}"), None, "subscript two is not a decimal digit");
        assert_eq!(parse_int("\u{B2}"), None, "superscript two is not a decimal digit");
        assert_eq!(parse_float("\u{967}.\u{96B}"), Some(1.5), "Devanagari digits");
        assert_eq!(parse_float("\u{FF11}_\u{FF10}"), Some(10.0));
        assert_eq!(parse_float("1\u{FF0E}5"), None, "a fullwidth full stop is not a decimal point");
    }

    #[test]
    fn int_truncates_toward_zero_and_rejects_non_finite() {
        assert_eq!(truncate(-2.7), Some(-2.0));
        assert_eq!(truncate(2.7), Some(2.0));
        assert_eq!(truncate(f64::NAN), None);
        assert_eq!(truncate(f64::INFINITY), None);
    }

    #[test]
    fn round_is_half_even_on_the_binary_value() {
        assert_eq!(round_to(0.125, 2), 0.12);
        assert_eq!(round_to(0.375, 2), 0.38);
        assert_eq!(round_to(2.5, 0), 2.0);
        assert_eq!(round_to(1.23456789, 6), 1.234568);
        assert_eq!(round_to(-0.0000004, 6), 0.0);
        assert!(round_to(f64::NAN, 6).is_nan());
    }

    #[test]
    fn convert_rad_to_deg_multiplies_before_dividing() {
        assert_eq!(rad_to_deg(1.0), 180.0 / PI);
        assert_eq!(rad_to_deg(PI), 180.0);
    }

    #[test]
    fn roll_folding_boundaries() {
        assert_eq!(fold_roll(90.0), 90.0);
        assert_eq!(fold_roll(91.0), 89.0);
        assert_eq!(fold_roll(-90.0), -90.0);
        assert_eq!(fold_roll(-91.0), -89.0);
        assert_eq!(fold_roll(180.0), 0.0);
    }

    #[test]
    fn functions_map_clamps_to_the_output_range() {
        assert_eq!(map_range(16383.5, 0.0, 32767.0, 0.0, 10.0), 5.0);
        assert_eq!(map_range(40000.0, 0.0, 32767.0, 0.0, 10.0), 10.0);
        assert_eq!(map_range(-5.0, 0.0, 100.0, 0.0, 10900.0), 0.0);
        assert_eq!(map_range(f64::NAN, 0.0, 1.0, 0.0, 7.0), 7.0);
    }
}

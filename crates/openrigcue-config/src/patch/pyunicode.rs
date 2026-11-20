//! Unicode character classes the patch modules depend on (Unicode 15.1): whitespace, digits,
//! and the decimal value of a digit.

/// First code point (digit zero) of every run of ten Unicode decimal digits (category Nd).
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50, 0xED0,
    0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620,
    0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0,
    0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50,
    0x16A60, 0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950,
    0x1FBF0,
];

/// Code point ranges of digits (Numeric_Type Decimal or Digit).
const DIGIT_RANGES: [(u32, u32); 83] = [
    (0x30, 0x39),
    (0xB2, 0xB3),
    (0xB9, 0xB9),
    (0x660, 0x669),
    (0x6F0, 0x6F9),
    (0x7C0, 0x7C9),
    (0x966, 0x96F),
    (0x9E6, 0x9EF),
    (0xA66, 0xA6F),
    (0xAE6, 0xAEF),
    (0xB66, 0xB6F),
    (0xBE6, 0xBEF),
    (0xC66, 0xC6F),
    (0xCE6, 0xCEF),
    (0xD66, 0xD6F),
    (0xDE6, 0xDEF),
    (0xE50, 0xE59),
    (0xED0, 0xED9),
    (0xF20, 0xF29),
    (0x1040, 0x1049),
    (0x1090, 0x1099),
    (0x1369, 0x1371),
    (0x17E0, 0x17E9),
    (0x1810, 0x1819),
    (0x1946, 0x194F),
    (0x19D0, 0x19DA),
    (0x1A80, 0x1A89),
    (0x1A90, 0x1A99),
    (0x1B50, 0x1B59),
    (0x1BB0, 0x1BB9),
    (0x1C40, 0x1C49),
    (0x1C50, 0x1C59),
    (0x2070, 0x2070),
    (0x2074, 0x2079),
    (0x2080, 0x2089),
    (0x2460, 0x2468),
    (0x2474, 0x247C),
    (0x2488, 0x2490),
    (0x24EA, 0x24EA),
    (0x24F5, 0x24FD),
    (0x24FF, 0x24FF),
    (0x2776, 0x277E),
    (0x2780, 0x2788),
    (0x278A, 0x2792),
    (0xA620, 0xA629),
    (0xA8D0, 0xA8D9),
    (0xA900, 0xA909),
    (0xA9D0, 0xA9D9),
    (0xA9F0, 0xA9F9),
    (0xAA50, 0xAA59),
    (0xABF0, 0xABF9),
    (0xFF10, 0xFF19),
    (0x104A0, 0x104A9),
    (0x10A40, 0x10A43),
    (0x10D30, 0x10D39),
    (0x10E60, 0x10E68),
    (0x11052, 0x1105A),
    (0x11066, 0x1106F),
    (0x110F0, 0x110F9),
    (0x11136, 0x1113F),
    (0x111D0, 0x111D9),
    (0x112F0, 0x112F9),
    (0x11450, 0x11459),
    (0x114D0, 0x114D9),
    (0x11650, 0x11659),
    (0x116C0, 0x116C9),
    (0x11730, 0x11739),
    (0x118E0, 0x118E9),
    (0x11950, 0x11959),
    (0x11C50, 0x11C59),
    (0x11D50, 0x11D59),
    (0x11DA0, 0x11DA9),
    (0x11F50, 0x11F59),
    (0x16A60, 0x16A69),
    (0x16AC0, 0x16AC9),
    (0x16B50, 0x16B59),
    (0x1D7CE, 0x1D7FF),
    (0x1E140, 0x1E149),
    (0x1E2F0, 0x1E2F9),
    (0x1E4F0, 0x1E4F9),
    (0x1E950, 0x1E959),
    (0x1F100, 0x1F10A),
    (0x1FBF0, 0x1FBF9),
];

/// Limit on the digits of an integer read from text.
pub const MAX_STR_DIGITS: usize = 4300;

/// Whether one code point is whitespace. Lone surrogates are not spaces.
pub fn is_space(code_point: u32) -> bool {
    char::from_u32(code_point).is_some_and(super::text::is_py_space)
}

/// The decimal value of a code point, if it is a decimal digit.
pub fn decimal_value(code_point: u32) -> Option<u32> {
    let index = DECIMAL_ZEROS.partition_point(|zero| *zero <= code_point).checked_sub(1)?;
    let offset = code_point - DECIMAL_ZEROS[index];
    (offset < 10).then_some(offset)
}

/// Whether one code point is a digit.
pub fn is_digit(code_point: u32) -> bool {
    let index = DIGIT_RANGES.partition_point(|(first, _)| *first <= code_point);
    index > 0 && code_point <= DIGIT_RANGES[index - 1].1
}

/// Whether a string is all digits: true when it is not empty and every character is a digit.
pub fn str_is_digit(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| is_digit(u32::from(c)))
}

/// An arbitrary-size integer read from text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyInt {
    /// A value that fits 128 bits.
    Small(i128),
    /// Any other value, as its canonical decimal text (`-` sign, no leading zeros).
    Big(String),
}

impl PyInt {
    /// Builds the canonical value from a sign and decimal digits (leading zeros allowed).
    pub fn from_digits(negative: bool, digits: &str) -> PyInt {
        let trimmed = digits.trim_start_matches('0');
        if trimmed.is_empty() {
            return PyInt::Small(0);
        }
        let text = if negative { format!("-{trimmed}") } else { trimmed.to_owned() };
        match text.parse::<i128>() {
            Ok(value) => PyInt::Small(value),
            Err(_) => PyInt::Big(text),
        }
    }

    /// The integer part of a finite float (truncated toward zero); `None` for NaN and infinities.
    pub fn from_float(value: f64) -> Option<PyInt> {
        if !value.is_finite() {
            return None;
        }
        let truncated = value.trunc();
        let text = format!("{truncated:.0}");
        let (negative, digits) = match text.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, text.as_str()),
        };
        Some(PyInt::from_digits(negative, digits))
    }

    /// The canonical decimal text.
    pub fn repr(&self) -> String {
        match self {
            PyInt::Small(value) => value.to_string(),
            PyInt::Big(text) => text.clone(),
        }
    }
}

/// Reads a base-10 integer: surrounding whitespace, a sign, decimal digits of any
/// script, single underscores between digits; `None` for anything else (including more than
/// [`MAX_STR_DIGITS`] digits).
pub fn int_from_code_points(code_points: impl IntoIterator<Item = u32>) -> Option<PyInt> {
    parse_int(code_points).ok()
}

pub(crate) enum IntParseError {
    InvalidLiteral,
    TooManyDigits(usize),
}

pub(crate) fn parse_int(code_points: impl IntoIterator<Item = u32>) -> Result<PyInt, IntParseError> {
    // Normalize to ASCII first: spaces become ' ', decimal digits their ASCII digit, other
    // non-ASCII characters an invalid character.
    let ascii: Vec<u8> = code_points
        .into_iter()
        .map(|c| match decimal_value(c) {
            _ if c >= 0x80 && is_space(c) => b' ',
            Some(value) => b'0' + value as u8,
            None if c < 0x80 => c as u8,
            None => b'?',
        })
        .collect();
    let is_c_space = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    let start = ascii.iter().position(|b| !is_c_space(b)).ok_or(IntParseError::InvalidLiteral)?;
    let end = ascii.iter().rposition(|b| !is_c_space(b)).ok_or(IntParseError::InvalidLiteral)? + 1;
    let body = &ascii[start..end];
    let (negative, body) = match body.first() {
        Some(b'-') => (true, &body[1..]),
        Some(b'+') => (false, &body[1..]),
        _ => (false, body),
    };
    let mut digits = String::new();
    let mut previous_underscore = true;
    for byte in body {
        match byte {
            b'0'..=b'9' => {
                digits.push(char::from(*byte));
                previous_underscore = false;
            }
            b'_' if !previous_underscore => previous_underscore = true,
            _ => return Err(IntParseError::InvalidLiteral),
        }
    }
    if digits.is_empty() || previous_underscore {
        return Err(IntParseError::InvalidLiteral);
    }
    if digits.len() > MAX_STR_DIGITS {
        return Err(IntParseError::TooManyDigits(digits.len()));
    }
    Ok(PyInt::from_digits(negative, &digits))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Character classes: Arabic-Indic and mathematical digits are decimal; superscripts
    /// and circled digits are digits but not decimal.
    #[test]
    fn classes() {
        assert_eq!(decimal_value(u32::from('7')), Some(7));
        assert_eq!(decimal_value(0x0663), Some(3));
        assert_eq!(decimal_value(0x1D7FF), Some(9));
        assert_eq!(decimal_value(0xB2), None);
        assert_eq!(decimal_value(u32::from('a')), None);
        assert!(is_digit(0xB2) && is_digit(0x2460) && is_digit(0x0663));
        assert!(!is_digit(u32::from('a')) && !is_digit(0x2469));
        assert!(str_is_digit("12\u{b3}") && !str_is_digit("") && !str_is_digit("1a"));
        assert!(is_space(0x1c) && is_space(0x3000) && !is_space(0x200b) && !is_space(0xd800));
    }

    /// Integer reading: whitespace, signs, underscores, other scripts, the digit limit.
    #[test]
    fn int_parsing() {
        let int = |text: &str| int_from_code_points(text.chars().map(u32::from));
        assert_eq!(int(" 1 "), Some(PyInt::Small(1)));
        assert_eq!(int("\u{3000}-0_1\n"), Some(PyInt::Small(-1)));
        assert_eq!(int("\u{0661}\u{0660}"), Some(PyInt::Small(10)));
        assert_eq!(int("007"), Some(PyInt::Small(7)));
        assert_eq!(int("-0"), Some(PyInt::Small(0)));
        for bad in ["", " ", "1_", "_1", "1__0", "1.0", "0x1", "\u{b2}", "+-1", "1 2", "\u{1c}1", "1\u{1f}"] {
            assert_eq!(int(bad), None, "{bad:?}");
        }
        let big = "9".repeat(40);
        assert_eq!(int(&big), Some(PyInt::Big(big.clone())));
        assert!(int(&"1".repeat(MAX_STR_DIGITS)).is_some());
        assert!(int(&"1".repeat(MAX_STR_DIGITS + 1)).is_none());
        assert_eq!(PyInt::from_float(-1.9), Some(PyInt::Small(-1)));
        assert_eq!(PyInt::from_float(1e40).map(|i| i.repr()), Some("10000000000000000303786028427003666890752".into()));
        assert_eq!(PyInt::from_float(f64::NAN), None);
    }
}

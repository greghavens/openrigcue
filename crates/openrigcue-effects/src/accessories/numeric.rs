//! Numeric helpers for accessory calculations.

/// A division by zero, which callers treat as a
/// failed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("division by zero")]
pub struct DivisionByZero;

/// Linear rescale of `x` from `in_min..in_max` to `out_min..out_max`, without clamping.
///
/// Unclamped arithmetic helper; [`map_clamped`] is the clamped form.
pub fn rescale(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> Result<f64, DivisionByZero> {
    let span = in_max - in_min;
    if span == 0.0 {
        return Err(DivisionByZero);
    }
    Ok((x - in_min) * (out_max - out_min) / span + out_min)
}

/// Clamped linear rescale: double arithmetic, then the upper comparison before the lower one.
/// A NaN intermediate selects `out_max`; reversed output bounds select `out_min`.
pub fn map_clamped(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> f64 {
    let value = (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
    let upper = if value <= out_max { value } else { out_max };
    if out_min <= upper {
        upper
    } else {
        out_min
    }
}

/// Float division `a / b`; a zero divisor is an error.
pub fn divide(a: f64, b: f64) -> Result<f64, DivisionByZero> {
    if b == 0.0 {
        Err(DivisionByZero)
    } else {
        Ok(a / b)
    }
}

/// `a / b`, or 0 when the divisor is 0.
pub fn divide_or_zero(a: f64, b: f64) -> f64 {
    divide(a, b).unwrap_or(0.0)
}

/// The integer part of a float: truncation toward zero; `None` for NaN or infinities.
pub fn truncate(x: f64) -> Option<i64> {
    x.is_finite().then(|| x.trunc() as i64)
}

/// A float rounded to the nearest integer, ties to even; `None` for NaN or infinities.
pub fn round_half_even(x: f64) -> Option<i64> {
    x.is_finite().then(|| x.round_ties_even() as i64)
}

/// A float rounded to two decimals: the exactly rounded two-decimal value (ties to even), which is what
/// Rust's exact decimal formatting produces.
pub fn round_to_hundredths(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.2}").parse().unwrap_or(x)
}

use openrigcue_model::python_numeric::decimal_digit;
pub use openrigcue_model::python_numeric::python_strip;

/// Why [`python_int_checked`] gives no `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyIntError {
    /// The text is not an integer.
    Invalid,
    /// The integer is outside `i64`.
    OutOfRange,
}

/// Reads an integer from text: surrounding white space, an optional sign, decimal digits of any script with
/// single underscores between them.
/// At most 4300 digits are accepted;
/// leading zeroes count, while signs and underscores do not.
pub fn python_int_checked(text: &str) -> Result<i64, PyIntError> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    // Accumulated negatively so that i64::MIN fits.
    let mut value: Option<i64> = Some(0);
    let mut seen_digit = false;
    let mut after_underscore = false;
    let mut digit_count = 0;
    for c in digits.chars() {
        if c == '_' {
            if !seen_digit || after_underscore {
                return Err(PyIntError::Invalid);
            }
            after_underscore = true;
            continue;
        }
        let digit = decimal_digit(c).ok_or(PyIntError::Invalid)?;
        digit_count += 1;
        value = value.and_then(|v| v.checked_mul(10)).and_then(|v| v.checked_sub(i64::from(digit)));
        seen_digit = true;
        after_underscore = false;
    }
    if !seen_digit || after_underscore || digit_count > 4300 {
        return Err(PyIntError::Invalid);
    }
    let value = if negative { value } else { value.and_then(i64::checked_neg) };
    value.ok_or(PyIntError::OutOfRange)
}

/// [`python_int_checked`] as an `Option`: `None` for invalid text and where the value is outside `i64`.
pub fn python_int(text: &str) -> Option<i64> {
    python_int_checked(text).ok()
}

/// An unbounded integer: within `i64`, or beyond it with its decimal text (without the sign).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyInt {
    Small(i64),
    Large { negative: bool, digits: String },
}

impl PyInt {
    /// `value == n`.
    pub fn equals(&self, n: i64) -> bool {
        *self == PyInt::Small(n)
    }

    /// `value > n`.
    pub fn greater_than(&self, n: i64) -> bool {
        match self {
            PyInt::Small(v) => *v > n,
            PyInt::Large { negative, .. } => !negative,
        }
    }
}

impl std::fmt::Display for PyInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PyInt::Small(v) => write!(f, "{v}"),
            PyInt::Large { negative, digits } => write!(f, "{}{digits}", if *negative { "-" } else { "" }),
        }
    }
}

/// Reads an integer of any size from text; `None` for invalid text.
pub fn python_int_value(text: &str) -> Option<PyInt> {
    match python_int_checked(text) {
        Ok(v) => Some(PyInt::Small(v)),
        Err(PyIntError::Invalid) => None,
        Err(PyIntError::OutOfRange) => {
            let text = text.trim();
            let negative = text.starts_with('-');
            let digits: String = text
                .chars()
                .filter_map(decimal_digit)
                .map(|d| char::from_digit(d, 10).unwrap_or('0'))
                .skip_while(|c| *c == '0')
                .collect();
            Some(PyInt::Large { negative, digits })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An integer beyond 64 bits keeps its value; its text is ASCII digits without leading zeros.
    #[test]
    fn python_int_beyond_64_bits() {
        let big = python_int_value(" +000123456789012345678901234567890 ").unwrap();
        assert_eq!(big.to_string(), "123456789012345678901234567890");
        assert!(big.greater_than(1) && !big.equals(255));
        let negative = python_int_value("-99_999_999_999_999_999_999").unwrap();
        assert_eq!(negative.to_string(), "-99999999999999999999");
        assert!(!negative.greater_than(1));
        assert_eq!(python_int_value("255"), Some(PyInt::Small(255)));
        assert_eq!(python_int_value("2x"), None);
    }

    /// The integer text grammar.
    #[test]
    fn python_int_of_text() {
        for (text, expected) in [
            ("1", Some(1)),
            (" 12 ", Some(12)),
            ("\u{1c}7\u{1f}", None),
            ("+5", Some(5)),
            ("-0", Some(0)),
            ("-12", Some(-12)),
            ("007", Some(7)),
            ("1_000", Some(1000)),
            ("\u{0663}\u{0661}", Some(31)),
            ("\u{ff11}\u{ff10}", Some(10)),
            ("-9223372036854775808", Some(i64::MIN)),
            ("9223372036854775808", None),
            ("", None),
            (" ", None),
            ("+", None),
            ("- 1", None),
            ("_1", None),
            ("1_", None),
            ("1__0", None),
            ("1.0", None),
            ("0x10", None),
            ("\u{200b}1", None),
            ("\u{00b2}", None),
        ] {
            assert_eq!(python_int(text), expected, "{text:?}");
        }
        assert_eq!(python_int_checked("9223372036854775808"), Err(PyIntError::OutOfRange));
        assert_eq!(python_int_checked("-99999999999999999999"), Err(PyIntError::OutOfRange));
        assert_eq!(python_int_checked("1x"), Err(PyIntError::Invalid));
    }

    /// The unclamped helper does not clamp.
    #[test]
    fn rescale_unclamped_arithmetic() {
        assert_eq!(rescale(5.0, 0.0, 10.0, 0.0, 100.0), Ok(50.0));
        assert_eq!(rescale(20.0, 0.0, 10.0, 0.0, 100.0), Ok(200.0));
        assert_eq!(rescale(1.0, 1.0, 10.0, 1.0, 5.0), Ok(1.0));
        assert_eq!(rescale(3.0, 2.0, 2.0, 0.0, 1.0), Err(DivisionByZero));
    }

    /// Truncation and round-half-even.
    #[test]
    fn python_integer_conversions() {
        assert_eq!(truncate(-2.7), Some(-2));
        assert_eq!(truncate(2.7), Some(2));
        assert_eq!(truncate(f64::NAN), None);
        assert_eq!(round_half_even(2.5), Some(2));
        assert_eq!(round_half_even(3.5), Some(4));
        assert_eq!(round_half_even(-0.5), Some(0));
        assert_eq!(round_half_even(f64::INFINITY), None);
        assert_eq!(round_to_hundredths(2.675), 2.67);
        assert_eq!(round_to_hundredths(1.005), 1.0);
        assert_eq!(round_to_hundredths(-12.345678), -12.35);
    }
}

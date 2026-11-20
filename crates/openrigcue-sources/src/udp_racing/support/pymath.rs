//! Numeric conversions the listeners rely on.

use std::f64::consts::PI;

/// The integer value of a float: truncation toward zero. `None` where there is none (NaN, infinity). The result is
/// never `-0.0`, as an integer has no negative zero.
pub fn trunc(x: f64) -> Option<f64> {
    x.is_finite().then(|| x.trunc() + 0.0)
}

/// The integer text of a float: truncate, normalize signed zero and retain the complete decimal integer.
pub fn int_text(value: f64) -> Option<String> {
    trunc(value).map(|value| if value == 0.0 { "0".into() } else { format!("{value:.0}") })
}

/// The integer value of a float as a typed value, retaining the shared model's finite-float policy beyond i64.
pub fn native_int(value: f64) -> Option<openrigcue_model::native_value::LegacyValue> {
    use openrigcue_model::native_value::LegacyValue::{Float, Int};
    trunc(value).map(|value| {
        if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value) {
            Int(value as i64)
        } else {
            Float(value)
        }
    })
}

/// The integer of base-10 text: surrounding whitespace, an optional sign, single underscores between digits,
/// Unicode digits and spaces. `None` for any other text (or where the value does not fit an `i64`).
pub fn parse_int(text: &str) -> Option<i64> {
    crate::udp_flight::python::parse_int(text)
}

/// A float rounded to `digits` decimals: the binary value rounded (ties to even)
/// through a correctly rounded decimal conversion. Non-finite values are returned unchanged.
pub fn round_to(x: f64, digits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.digits$}").parse().unwrap_or(x)
}

/// Radians to degrees as `radians * 180.0 / pi`.
pub fn rad_to_deg(radians: f64) -> f64 {
    radians * 180.0 / PI
}

/// Radians to degrees as `x * (180 / pi)`.
pub fn degrees(radians: f64) -> f64 {
    radians * (180.0 / PI)
}

/// The float of a text: accepts surrounding whitespace (including U+001C..U+001F),
/// `inf`, `infinity` and `nan` and underscores between digits.
pub fn parse_float(text: &str) -> Option<f64> {
    crate::udp_flight::python::parse_float(text)
}

/// The text of a float: the shortest round-trip digits, positional while the decimal exponent
/// is in -4..16 (whole numbers end in `.0`), otherwise `d.ddde±XX`; `nan`, `inf`, `-inf`.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    let shortest = format!("{x:e}");
    let (mantissa, exponent) = shortest.split_once('e').unwrap_or((&shortest, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if mantissa.starts_with('-') { "-" } else { "" };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let body = if (-4..16).contains(&exponent) {
        positional(&digits, exponent)
    } else {
        let fraction = if digits.len() > 1 { format!(".{}", &digits[1..]) } else { String::new() };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{}{fraction}e{exponent_sign}{:02}", &digits[..1], exponent.unsigned_abs())
    };
    format!("{sign}{body}")
}

/// `digits` (d.ddd) times 10^`exponent` written without an exponent, with at least one fractional digit.
fn positional(digits: &str, exponent: i32) -> String {
    let point = exponent + 1;
    if point <= 0 {
        return format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize));
    }
    let point = point as usize;
    if point >= digits.len() {
        format!("{digits}{}.0", "0".repeat(point - digits.len()))
    } else {
        format!("{}.{}", &digits[..point], &digits[point..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_truncates_toward_zero() {
        assert_eq!(trunc(-2.7), Some(-2.0));
        assert!(trunc(-0.5).unwrap().is_sign_positive());
        assert_eq!(trunc(2.7), Some(2.0));
        assert_eq!(trunc(f64::NAN), None);
    }

    #[test]
    fn round_is_half_even_on_exact_ties() {
        // 1/128 = 0.0078125 is an exact tie at six decimals; ties to even give 0.007812.
        assert_eq!(round_to(0.0078125, 6), 0.007812);
        assert_eq!(round_to(0.0234375, 6), 0.023438);
        assert_eq!(round_to(1.23456789, 6), 1.234568);
        assert_eq!(round_to(-0.0000004, 6), -0.0);
    }

    #[test]
    fn float_repr_follows_python() {
        // Float text
        let cases = [
            (5.0, "5.0"),
            (-0.0, "-0.0"),
            (0.1, "0.1"),
            (-12.345, "-12.345"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1234567890123456.0, "1234567890123456.0"),
            (1e16, "1e+16"),
            (1e200, "1e+200"),
            (f64::NAN, "nan"),
            (f64::NEG_INFINITY, "-inf"),
        ];
        for (x, text) in cases {
            assert_eq!(float_repr(x), text, "{x:e}");
        }
    }

    #[test]
    fn int_parsing_follows_python() {
        assert_eq!(parse_int(" 7576\n"), Some(7576));
        assert_eq!(parse_int("+1_000"), Some(1000));
        assert_eq!(parse_int("-3"), Some(-3));
        assert_eq!(parse_int("7576.0"), None);
        assert_eq!(parse_int("1__0"), None);
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("-"), None);
    }

    #[test]
    fn float_parsing_follows_python() {
        assert_eq!(parse_float(" 1.5 "), Some(1.5));
        assert_eq!(parse_float("1_000"), Some(1000.0));
        assert_eq!(parse_float("-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_float("N"), None);
        assert_eq!(parse_float("1__0"), None);
        assert_eq!(parse_float(""), None);
    }

    #[test]
    fn degree_conversions() {
        assert_eq!(rad_to_deg(PI), 180.0);
        assert!((degrees(1.0) - 57.29577951308232).abs() < 1e-12);
    }
}

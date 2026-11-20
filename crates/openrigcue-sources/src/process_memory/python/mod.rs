//! Text and number handling shared by these listeners: lenient JSON decoding, dict-literal parsing, integer
//! text conversion, rounding to 6 decimals and the whitespace class.

pub mod json;
pub mod literal;

/// Whether one character is whitespace: Unicode whitespace plus the ASCII separators U+001C..U+001F.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Integer from text within the i64 value range: surrounding whitespace, a sign, Unicode decimal digits and
/// single underscores between digits are accepted.
pub fn int_from_str(text: &str) -> Option<i64> {
    openrigcue_config::import::value::parse_py_int(text)
}

/// Rounds a float to 6 decimals (exact decimal rounding of the binary value), then
/// truncates toward zero. `None` for NaN and infinities.
pub fn int_of_round6(x: f64) -> Option<f64> {
    if !x.is_finite() {
        return None;
    }
    if x.abs() >= 1e15 {
        return Some(x.trunc());
    }
    let rounded: f64 = format!("{x:.6}").parse().ok()?;
    Some(rounded.trunc())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integer text as the dispatcher's loader and the listeners accept it: whitespace,
    /// sign, single underscores between digits.
    #[test]
    fn int_from_str_follows_python() {
        assert_eq!(int_from_str(" 42 "), Some(42));
        assert_eq!(int_from_str("-1_000"), Some(-1000));
        assert_eq!(int_from_str("+7"), Some(7));
        assert_eq!(int_from_str("1.5"), None);
        assert_eq!(int_from_str("_1"), None);
        assert_eq!(int_from_str("1__0"), None);
        assert_eq!(int_from_str(""), None);
        assert_eq!(int_from_str("\u{1f}3\n"), None);
        assert_eq!(int_from_str("１２_٣"), Some(123));
        assert_eq!(int_from_str("-9223372036854775808"), Some(i64::MIN));
    }

    /// War Thunder speed and rpm: round the binary value to
    /// 6 decimals, then truncate toward zero; NaN and infinity are errors.
    #[test]
    fn int_of_round6_crosses_integer_only_within_half_micro() {
        assert_eq!(int_of_round6(12.9999996), Some(13.0));
        assert_eq!(int_of_round6(12.9999994), Some(12.0));
        assert_eq!(int_of_round6(-12.9999996), Some(-13.0));
        assert_eq!(int_of_round6(5.7), Some(5.0));
        assert_eq!(int_of_round6(f64::NAN), None);
        assert_eq!(int_of_round6(f64::INFINITY), None);
    }
}

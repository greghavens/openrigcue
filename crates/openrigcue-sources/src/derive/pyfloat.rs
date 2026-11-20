//! The two numeric conversions the dispatcher relies on.

/// Rounds to `digits` decimals: the exact binary value is rounded, exact
/// ties go to the even digit. Rust's fixed-precision formatting rounds the same way, so the formatted text is
/// parsed back as the result.
pub fn round_to(value: f64, digits: i32) -> f64 {
    if !value.is_finite() || digits < 0 {
        return value;
    }
    let precision = digits as usize;
    format!("{value:.precision$}").parse().unwrap_or(value)
}

/// Truncation toward zero; `None` for NaN and ±inf.
pub fn truncate(value: f64) -> Option<f64> {
    value.is_finite().then(|| value.trunc())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rounding is half-to-even on the exact binary value.
    #[test]
    fn round_matches_python() {
        assert_eq!(round_to(0.125, 2), 0.12);
        assert_eq!(round_to(0.375, 2), 0.38);
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(-0.125, 2), -0.12);
        assert_eq!(round_to(123.456, 2), 123.46);
        assert!(round_to(f64::NAN, 2).is_nan());
    }

    /// Integer conversion truncates and rejects non-finite values.
    #[test]
    fn truncate_matches_python() {
        assert_eq!(truncate(-1.9), Some(-1.0));
        assert_eq!(truncate(55.99), Some(55.0));
        assert_eq!(truncate(f64::INFINITY), None);
        assert_eq!(truncate(f64::NAN), None);
    }
}

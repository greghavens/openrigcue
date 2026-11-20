//! Numeric conversions the shared-memory listeners use.
//!
//! A conversion that can fail returns `None` where it fails
//! (rounding NaN, truncating infinity, invalid UTF-8): the listener then leaves that channel unchanged, which is what
//! the dispatcher does when a read fails for a channel without a registry default.

use std::f64::consts::PI;

/// Rounds a float to the nearest integer, ties to even. `None` for NaN and infinities.
pub(crate) fn round_to_int(x: f64) -> Option<f64> {
    x.is_finite().then(|| x.round_ties_even())
}

/// Rounds a float to 6 decimals: correctly rounded from the exact binary value, ties to
/// even; NaN and infinities pass through unchanged.
pub(crate) fn round_6(x: f64) -> f64 {
    // Doubles of this magnitude have no fractional digits; they are returned unchanged.
    if !x.is_finite() || x.abs() >= 1e17 {
        return x;
    }
    // Rust formats with exact decimal expansion and ties-to-even.
    format!("{x:.6}").parse().unwrap_or(x)
}

/// Truncates a float toward zero. `None` for NaN and infinities.
pub(crate) fn truncate_to_int(x: f64) -> Option<f64> {
    x.is_finite().then(|| x.trunc())
}

/// Radians to degrees: `radians * 180.0 / π`, in that evaluation order.
pub(crate) fn rad_to_deg(radians: f64) -> f64 {
    radians * 180.0 / PI
}

/// Strict UTF-8 decoding of a character array value. `None` for invalid UTF-8.
pub(crate) fn decode_utf8(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// `a / b * 100` for floats, 0 for a zero divisor (the
/// listeners' fuel-level expressions).
pub(crate) fn percent_or_zero(numerator: f64, denominator: f64) -> f64 {
    if denominator == 0.0 {
        0.0
    } else {
        numerator / denominator * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rounding to an integer, rounding to 6 decimals and truncation.
    #[test]
    fn round_matches_python_half_to_even() {
        assert_eq!(round_to_int(2.5), Some(2.0));
        assert_eq!(round_to_int(3.5), Some(4.0));
        assert_eq!(round_to_int(-0.5), Some(-0.0));
        assert_eq!(round_to_int(1234.49), Some(1234.0));
        assert_eq!(round_to_int(f64::NAN), None);
        assert_eq!(round_to_int(f64::INFINITY), None);
    }

    #[test]
    fn round_6_matches_python() {
        // 0.0078125 rounds to 0.007812 (exact tie, to even); 0.0234375 rounds to 0.023438.
        assert_eq!(round_6(0.0078125), 0.007812);
        assert_eq!(round_6(0.0234375), 0.023438);
        assert_eq!(round_6(-0.0078125), -0.007812);
        // 1.0000005 rounds to 1.000001 (binary value just above the tie).
        assert_eq!(round_6(1.0000005), 1.000001);
        assert_eq!(round_6(9.80665 / 3.0), 3.268883);
        assert!(round_6(-0.0000001).is_sign_negative());
        assert!(round_6(f64::NAN).is_nan());
        assert_eq!(round_6(1e300), 1e300);
    }

    #[test]
    fn int_truncates_toward_zero() {
        assert_eq!(truncate_to_int(-2.9), Some(-2.0));
        assert_eq!(truncate_to_int(10299.99), Some(10299.0));
        assert_eq!(truncate_to_int(f64::NAN), None);
    }

    #[test]
    fn rad_to_deg_uses_python_order() {
        assert_eq!(rad_to_deg(1.0), 180.0 / PI);
        assert_eq!(rad_to_deg(0.1), 0.1 * 180.0 / PI);
    }

    #[test]
    fn strict_utf8_decode() {
        assert_eq!(decode_utf8(b"Scania"), Some("Scania".to_owned()));
        assert_eq!(decode_utf8(&[0xff, 0x41]), None);
    }

    #[test]
    fn percent_handles_zero_division() {
        assert_eq!(percent_or_zero(30.0, 60.0), 50.0);
        assert_eq!(percent_or_zero(30.0, 0.0), 0.0);
        assert_eq!(percent_or_zero(30.0, -0.0), 0.0);
    }
}

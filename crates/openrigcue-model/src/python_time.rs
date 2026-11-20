//! Conversion of a floating number of seconds to a sleep interval.

use std::time::Duration;

/// Convert a nonnegative floating number of seconds to the requested sleep interval.
/// The value is rounded away from zero to integer nanoseconds, signed-64 overflow is rejected,
/// and the result is rounded up once more to a multiple of 100 ns.
/// This represents the timeout, not the scheduler's actual wake-up time.
pub fn sleep_duration(seconds: f64) -> Result<Duration, &'static str> {
    sleep_duration_for_platform(seconds, cfg!(windows))
}

fn sleep_duration_for_platform(seconds: f64, windows: bool) -> Result<Duration, &'static str> {
    if seconds.is_nan() {
        return Err("Invalid value NaN (not a number)");
    }
    let scaled = seconds * 1_000_000_000.0;
    let nanos = if scaled >= 0.0 { scaled.ceil() } else { scaled.floor() };
    if nanos < i64::MIN as f64 || nanos >= -(i64::MIN as f64) || !nanos.is_finite() {
        return Err("timestamp out of range for platform time_t");
    }
    if nanos < 0.0 {
        return Err("sleep length must be non-negative");
    }
    let nanos = nanos as u64;
    let nanos = if windows { nanos.div_ceil(100) * 100 } else { nanos };
    Ok(Duration::from_nanos(nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_rounds_up_before_windows_tick_conversion() {
        for (seconds, unix, windows) in [
            (0.0, 0, 0),
            (-0.0, 0, 0),
            (1.0 / 30.0, 33_333_334, 33_333_400),
            (1.0 / 7.0, 142_857_143, 142_857_200),
            (1.0 / 90.0, 11_111_112, 11_111_200),
            (1.0e-10, 1, 100),
            (f64::from_bits(1), 1, 100),
            (0.05, 50_000_000, 50_000_000),
        ] {
            assert_eq!(sleep_duration_for_platform(seconds, false), Ok(Duration::from_nanos(unix)));
            assert_eq!(sleep_duration_for_platform(seconds, true), Ok(Duration::from_nanos(windows)));
        }
    }

    #[test]
    fn timeout_checks_conversion_range_before_negative_sleep() {
        for value in [-1.0, -f64::from_bits(1)] {
            assert_eq!(sleep_duration_for_platform(value, false), Err("sleep length must be non-negative"));
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::MAX, -f64::MAX] {
            assert_eq!(sleep_duration_for_platform(value, false), Err("timestamp out of range for platform time_t"));
        }
        assert_eq!(sleep_duration_for_platform(f64::NAN, false), Err("Invalid value NaN (not a number)"));
    }
}

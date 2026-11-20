//! Main-loop pacing.

use std::time::Duration;

/// Lowest main-loop rate, Hz.
const MIN_RATE_HZ: i64 = 30;

/// Refresh rates of the three output devices (`refresh_rate` of `[SHAKEBOX]`, `[INTELLIBOX]`, `[MOTIONBOX]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshRates {
    pub shakebox: i64,
    pub intellibox: i64,
    pub motionbox: i64,
}

/// Pause after each main-loop tick: one period of the fastest device, at least 30 Hz.
pub fn tick_period(rates: RefreshRates) -> Duration {
    let rate = rates.shakebox.max(rates.intellibox).max(rates.motionbox).max(MIN_RATE_HZ);
    // One period of the rate: the same conversion as the checked path below. A rate of at least 30 Hz
    // always gives a valid interval.
    openrigcue_model::python_time::sleep_duration(1.0 / rate as f64).unwrap_or_default()
}

/// The refresh rates are converted to integers from the canonical values, in device order. A failure ends
/// the main loop after the telemetry/export/session updates have already happened.
pub(super) fn native_tick_period(rates: &[Option<openrigcue_model::native_value::LegacyValue>; 3]) -> Option<Duration> {
    use openrigcue_model::native_value::LegacyValue;
    use openrigcue_model::python_numeric::python_int_text;
    fn integer(value: &LegacyValue) -> Option<f64> {
        let text = match value {
            LegacyValue::Int(value) => return Some(*value as f64),
            LegacyValue::Bool(value) => return Some(f64::from(*value)),
            LegacyValue::Float(value) => return value.is_finite().then(|| value.trunc()),
            LegacyValue::Str(text) => text.as_str(),
            LegacyValue::Bytes(bytes) if bytes.is_ascii() => std::str::from_utf8(bytes).ok()?,
            _ => return None,
        };
        // Integers of any size are accepted here: an enormous negative one loses to the
        // minimum rate, while a positive one too large for a finite float yields no
        // period at all.
        python_int_text(text)?.parse().ok()
    }
    let mut rate = MIN_RATE_HZ as f64;
    for value in rates {
        rate = rate.max(integer(value.as_ref()?)?);
    }
    rate.is_finite().then(|| openrigcue_model::python_time::sleep_duration(1.0 / rate).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_tick_sleep_rounds_up_fractional_nanoseconds() {
        use openrigcue_model::native_value::LegacyValue;
        for (rate, nanos) in [(30, 33_333_334_u128), (90, 11_111_112), (10_000_000_000_i64, 1)] {
            let rates = [Some(LegacyValue::Int(rate)), Some(LegacyValue::Int(30)), Some(LegacyValue::Int(30))];
            let expected = if cfg!(windows) { nanos.div_ceil(100) * 100 } else { nanos };
            assert_eq!(native_tick_period(&rates).unwrap().as_nanos(), expected);
        }
    }

    /// Pacing: max of the three rates, floored at 30 Hz.
    #[test]
    fn native_rate_integer_limit_precedes_rate_selection() {
        use openrigcue_model::native_value::LegacyValue;
        let within = format!("{}80", "0".repeat(4298));
        let rates = |text| [Some(LegacyValue::Str(text)), Some(LegacyValue::Int(30)), Some(LegacyValue::Int(30))];
        assert_eq!(native_tick_period(&rates(within.clone())), Some(Duration::from_secs_f64(1.0 / 80.0)));
        assert_eq!(native_tick_period(&rates(format!("0{within}"))), None);
    }

    #[test]
    fn huge_integer_rate_follows_float_division_after_maximum_selection() {
        use openrigcue_model::native_value::LegacyValue as V;
        let huge = format!("1{}", "0".repeat(400));
        let normal = Some(V::Int(30));
        assert_eq!(native_tick_period(&[Some(V::Str(huge.clone())), normal.clone(), normal.clone()]), None);
        assert_eq!(
            native_tick_period(&[Some(V::Bytes(huge.as_bytes().to_vec())), normal.clone(), normal.clone()]),
            None
        );
        assert_eq!(
            native_tick_period(&[Some(V::Str(format!("-{huge}"))), normal.clone(), normal.clone()]),
            openrigcue_model::python_time::sleep_duration(1.0 / 30.0).ok()
        );
        assert_eq!(native_tick_period(&[Some(V::Str(huge)), Some(V::Str("bad".into())), normal.clone()]), None);
        assert_eq!(native_tick_period(&[Some(V::Float(f64::INFINITY)), normal.clone(), normal]), None);
    }

    #[test]
    fn fastest_device_with_floor() {
        let rates = |s, i, m| RefreshRates { shakebox: s, intellibox: i, motionbox: m };
        // One period of the rate: the float is rounded up to whole nanoseconds, as on the checked path.
        let unix = |nanos| if cfg!(windows) { None } else { Some(Duration::from_nanos(nanos)) };
        let sleep = |rate: f64| openrigcue_model::python_time::sleep_duration(1.0 / rate).unwrap();
        assert_eq!(tick_period(rates(10, 20, 0)), sleep(30.0));
        assert_eq!(tick_period(rates(60, 100, 30)), sleep(100.0));
        assert_eq!(tick_period(rates(31, 0, 0)), sleep(31.0));
        for (rate, nanos) in [(30, 33_333_334), (90, 11_111_112), (100, 10_000_000), (31, 32_258_065)] {
            if let Some(expected) = unix(nanos) {
                assert_eq!(tick_period(rates(rate, 0, 0)), expected, "{rate} Hz");
            }
        }
    }
}

//! LED / tachometer test pattern written while no listener delivers.

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

/// Period of the test-pattern writer: a thirtieth of a second, rounded up to whole
/// nanoseconds and to the 100 ns timer unit.
pub const TEST_PATTERN_PERIOD: Duration = Duration::from_nanos(if cfg!(windows) { 33_333_400 } else { 33_333_334 });

/// Keeps LED and tachometer devices lit while no game runs and either is set to "Always On"
/// (status 2).
#[derive(Debug, Clone, Default)]
pub struct TestPattern {
    sending: bool,
}

impl TestPattern {
    /// Fields assigned by a changed pass, in the order they are written.
    /// An active pattern writes its eight special values before the other registry
    /// fields; the single restore pass writes the registry in its usual order.
    pub fn assigned_channels(active: bool) -> impl Iterator<Item = &'static str> {
        const FIRST: &[&str] =
            &["speed", "max_speed", "speed_kmh", "max_speed_kmh", "gear", "harmonized_gear", "rpm", "max_rpm"];
        FIRST
            .iter()
            .copied()
            .filter(move |_| active)
            .chain(Telemetry::CHANNELS.iter().copied().filter(move |name| !active || !FIRST.contains(name)))
    }

    /// One period of the writer. With the pattern active every channel is at its value when off except speed 0,
    /// max speed 1 m/s (3.6 km/h), gear 0 and rpm above max rpm (11000/10999) when the tachometer is on (0/0 for the
    /// LEDs alone). When the pattern ends every channel returns to its value when off once.
    pub fn apply(
        &mut self,
        listener_idle: bool,
        led_always_on: bool,
        tach_always_on: bool,
        telemetry: &mut Telemetry,
    ) -> bool {
        self.apply_with(listener_idle, led_always_on, tach_always_on, |name, value| telemetry.set_native(name, value))
    }

    /// Publish each channel assignment in write order. The writer is called
    /// once per channel so independent readers and writers can run between channels.
    pub fn apply_with(
        &mut self,
        listener_idle: bool,
        led_always_on: bool,
        tach_always_on: bool,
        assign: impl FnMut(&'static str, LegacyValue),
    ) -> bool {
        self.apply_live_with(|| listener_idle, || led_always_on, || tach_always_on, assign)
    }

    /// Keep condition reads separate: the tach switch is read again after the
    /// initial "LED or tachometer always on" condition succeeds.
    pub fn apply_live_with(
        &mut self,
        mut listener_idle: impl FnMut() -> bool,
        mut led_always_on: impl FnMut() -> bool,
        mut tach_always_on: impl FnMut() -> bool,
        mut assign: impl FnMut(&'static str, LegacyValue),
    ) -> bool {
        let active = listener_idle() && (led_always_on() || tach_always_on());
        if active {
            let (rpm, max_rpm) = if tach_always_on() { (11000, 10999) } else { (0, 0) };
            self.sending = true;
            for (name, value) in [
                ("speed", LegacyValue::Int(0)),
                ("max_speed", LegacyValue::Int(1)),
                ("speed_kmh", LegacyValue::Float(0.0)),
                ("max_speed_kmh", LegacyValue::Float(3.6)),
                ("gear", LegacyValue::Int(0)),
                ("harmonized_gear", LegacyValue::Int(0)),
                ("rpm", LegacyValue::Int(rpm)),
                ("max_rpm", LegacyValue::Int(max_rpm)),
            ] {
                assign(name, value);
            }
            for (&name, value) in Telemetry::CHANNELS.iter().zip(Telemetry::OFF_VALUES) {
                if !matches!(
                    name,
                    "speed"
                        | "max_speed"
                        | "speed_kmh"
                        | "max_speed_kmh"
                        | "gear"
                        | "harmonized_gear"
                        | "rpm"
                        | "max_rpm"
                ) {
                    assign(name, value.clone());
                }
            }
            true
        } else if self.sending {
            self.sending = false;
            for (&name, value) in Telemetry::CHANNELS.iter().zip(Telemetry::OFF_VALUES) {
                assign(name, value.clone());
            }
            true
        } else {
            false
        }
    }
}

/// Every registry channel to its value when off; game, vehicle and location are not registry channels.
pub(crate) fn reset_channels(telemetry: &mut Telemetry) {
    telemetry.reset_to_off();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tachometer "Always On" lights every LED (rpm > max_rpm), then restores the off values once.
    #[test]
    fn tach_pattern_then_restore() {
        let mut pattern = TestPattern::default();
        let mut telemetry = Telemetry::off();
        telemetry.pitch = 5.0;
        pattern.apply(true, false, true, &mut telemetry);
        assert_eq!((telemetry.rpm, telemetry.max_rpm, telemetry.gear), (11000.0, 10999.0, 0.0));
        assert_eq!((telemetry.max_speed, telemetry.max_speed_kmh, telemetry.pitch), (1.0, 3.6, 0.0));
        pattern.apply(false, false, true, &mut telemetry);
        assert_eq!((telemetry.rpm, telemetry.max_rpm, telemetry.gear), (0.0, 20000.0, 11.0));
        telemetry.rpm = 4000.0;
        pattern.apply(false, false, true, &mut telemetry);
        assert_eq!(telemetry.rpm, 4000.0);
    }

    /// LEDs alone use rpm 0 / max rpm 0.
    #[test]
    fn led_pattern() {
        let mut pattern = TestPattern::default();
        let mut telemetry = Telemetry::off();
        pattern.apply(true, true, false, &mut telemetry);
        assert_eq!((telemetry.rpm, telemetry.max_rpm, telemetry.harmonized_gear), (0.0, 0.0, 0.0));
    }

    #[test]
    fn assignments_expose_prefixes_and_preserve_a_later_producer_write() {
        let mut pattern = TestPattern::default();
        let mut telemetry = Telemetry::unfilled();
        telemetry.set_native("pitch", LegacyValue::Int(42));
        telemetry.set_native("rpm", LegacyValue::Int(123));
        let mut assignments = Vec::new();
        assert!(pattern.apply_with(true, false, true, |name, value| {
            telemetry.set_native(name, value);
            assignments.push(name);
            if name == "speed" {
                assert_eq!(telemetry.native_value("pitch"), Some(&LegacyValue::Int(42)));
                assert_eq!(telemetry.native_value("rpm"), Some(&LegacyValue::Int(123)));
                assert_eq!(telemetry.get_assigned("max_speed"), None);
            }
            if name == "rpm" {
                assert_eq!(telemetry.native_value("rpm"), Some(&LegacyValue::Int(11000)));
                telemetry.set_native("rpm", LegacyValue::Int(456));
            }
        }));
        assert_eq!(assignments, TestPattern::assigned_channels(true).collect::<Vec<_>>());
        assert_eq!(telemetry.native_value("pitch"), Some(&LegacyValue::Int(0)));
        assert_eq!(telemetry.native_value("rpm"), Some(&LegacyValue::Int(456)));
        assert_eq!(telemetry.native_value("speed_kmh"), Some(&LegacyValue::Float(0.0)));
        assignments.clear();
        assert!(pattern.apply_with(false, false, false, |name, value| {
            telemetry.set_native(name, value);
            assignments.push(name);
            if name == "rpm" {
                telemetry.set_native("rpm", LegacyValue::Int(789));
            }
        }));
        assert_eq!(assignments, Telemetry::CHANNELS);
        assert_eq!(telemetry.native_value("rpm"), Some(&LegacyValue::Int(789)));
        assert!(!pattern.apply_with(false, false, false, |_, _| panic!("restore must run only once")));
    }

    #[test]
    fn tach_is_read_again_after_the_short_circuit_condition() {
        use std::cell::{Cell, RefCell};
        let calls = RefCell::new(Vec::new());
        let tach_reads = Cell::new(0);
        let mut telemetry = Telemetry::unfilled();
        let mut pattern = TestPattern::default();
        assert!(pattern.apply_live_with(
            || {
                calls.borrow_mut().push("idle");
                true
            },
            || {
                calls.borrow_mut().push("led");
                false
            },
            || {
                calls.borrow_mut().push("tach");
                tach_reads.set(tach_reads.get() + 1);
                tach_reads.get() == 1
            },
            |name, value| telemetry.set_native(name, value),
        ));
        assert_eq!(*calls.borrow(), ["idle", "led", "tach", "tach"]);
        assert_eq!(telemetry.native_value("rpm"), Some(&LegacyValue::Int(0)));
        assert_eq!(telemetry.native_value("max_rpm"), Some(&LegacyValue::Int(0)));
        calls.borrow_mut().clear();
        assert!(pattern.apply_live_with(
            || {
                calls.borrow_mut().push("idle");
                true
            },
            || {
                calls.borrow_mut().push("led");
                true
            },
            || {
                calls.borrow_mut().push("tach");
                true
            },
            |name, value| telemetry.set_native(name, value),
        ));
        assert_eq!(*calls.borrow(), ["idle", "led", "tach"]);
        assert_eq!(telemetry.native_value("rpm"), Some(&LegacyValue::Int(11000)));
    }
}

#[cfg(test)]
mod period_tests {
    use super::TEST_PATTERN_PERIOD;

    /// Each pass ends with a pause of a thirtieth of a second.
    #[test]
    fn period_is_the_original_sleep() {
        assert_eq!(Ok(TEST_PATTERN_PERIOD), openrigcue_model::python_time::sleep_duration(0.033_333_333_333_333_33));
        #[cfg(not(windows))]
        assert_eq!(TEST_PATTERN_PERIOD.as_nanos(), 33_333_334);
    }
}

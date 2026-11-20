//! Wheel slip synthesised from successive speed / rpm samples, for games that publish no slip data
//! (rFactor 1, rFpro and rFactor 2 here).

use openrigcue_model::native_value::LegacyValue;

/// Which side of the car a wheel is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WheelSide {
    Left,
    Right,
}

/// Inputs of one estimate, in the listener's units (m/s, rev/min, pedals 0..1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SlipInputs {
    pub(crate) lateral_velocity: f64,
    pub(crate) speed: f64,
    pub(crate) rpm: f64,
    pub(crate) gear: i64,
    pub(crate) previous_speed: f64,
    pub(crate) previous_rpm: f64,
    pub(crate) throttle: f64,
    pub(crate) brake: f64,
}

/// One km/h in m/s: below it a sample is treated as standing still.
const STANDSTILL_SPEED: f64 = 0.277778;
/// Scale of the change in speed/rpm ratio between samples.
const RATIO_CHANGE_GAIN: f64 = 4000.0;

/// The estimated slip of one wheel, as a float.
pub(crate) fn estimated_wheel_slip(side: WheelSide, s: &SlipInputs) -> f64 {
    native_estimated_wheel_slip(side, s).py_float().expect("slip is always numeric")
}

/// The estimate with its value kind: the early returns are integer zero, a calculated result is a float.
pub(crate) fn native_estimated_wheel_slip(side: WheelSide, s: &SlipInputs) -> LegacyValue {
    if s.speed <= STANDSTILL_SPEED || s.rpm <= 0.0 || s.previous_speed <= STANDSTILL_SPEED || s.previous_rpm <= 0.0 {
        return LegacyValue::Int(0);
    }
    let lateral = lateral_component(side, s.lateral_velocity);
    if s.gear == 0 {
        return LegacyValue::Int(0);
    }
    let ratio = s.speed / s.rpm;
    let previous_ratio = s.previous_speed / s.previous_rpm;
    let brake_percent = s.brake * 100.0;
    let throttle_percent = s.throttle * 100.0;
    // This branch is taken only for an ordered greater-than.
    // An inverted <= condition would lose that distinction for NaN.
    let ratio_slip = if brake_percent > 20.0 {
        (previous_ratio - ratio).abs() * RATIO_CHANGE_GAIN * range_fraction(55.0, 90.0, brake_percent)
    } else if throttle_percent > 40.0 {
        let gain = ratio - previous_ratio;
        let gain = if gain > 0.0 { gain } else { 0.0 };
        gain * RATIO_CHANGE_GAIN * range_fraction(70.0, 100.0, throttle_percent)
    } else {
        return LegacyValue::Int(0);
    };
    LegacyValue::Float((if ratio_slip > lateral { ratio_slip } else { lateral }) * 10.0)
}

/// Sideways slip share of one wheel: 0..1 over |lateral velocity| 0.5..2 m/s, halved on the inside wheel.
fn lateral_component(side: WheelSide, lateral_velocity: f64) -> f64 {
    let lateral = range_fraction(0.5, 2.0, lateral_velocity.abs());
    let inside = match side {
        WheelSide::Left => lateral_velocity < 0.0,
        WheelSide::Right => lateral_velocity > 0.0,
    };
    if inside {
        lateral * 0.5
    } else {
        lateral
    }
}

/// Where `value`
/// sits between `min` and `max`, clamped to 0..1, measured from `min` (a reversed range counts down).
fn range_fraction(min: f64, max: f64, value: f64) -> f64 {
    if min == max {
        return 0.0;
    }
    let (low, high) = if min <= max { (min, max) } else { (max, min) };
    let fraction = if value >= low {
        if value <= high {
            (value - low) / (high - low)
        } else {
            1.0
        }
    } else {
        0.0
    };
    if min <= max {
        fraction
    } else {
        1.0 - fraction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> SlipInputs {
        SlipInputs {
            lateral_velocity: 0.0,
            speed: 40.0,
            rpm: 6000.0,
            gear: 3,
            previous_speed: 29.0,
            previous_rpm: 5000.0,
            throttle: 0.0,
            brake: 0.0,
        }
    }

    #[test]
    fn get_offset_clamps_and_inverts() {
        assert_eq!(range_fraction(0.5, 2.0, 1.25), 0.5);
        assert_eq!(range_fraction(0.5, 2.0, 0.1), 0.0);
        assert_eq!(range_fraction(0.5, 2.0, 5.0), 1.0);
        assert_eq!(range_fraction(2.0, 0.5, 1.25), 0.5);
        assert_eq!(range_fraction(2.0, 0.5, 0.5), 1.0);
        assert_eq!(range_fraction(1.0, 1.0, 3.0), 0.0);
        assert_eq!(range_fraction(0.5, 2.0, f64::NAN), 0.0);
    }

    #[test]
    fn get_rpm_speed_slip_standstill_gates() {
        let at = |f: fn(&mut SlipInputs)| {
            let mut s = inputs();
            s.throttle = 1.0;
            f(&mut s);
            estimated_wheel_slip(WheelSide::Left, &s)
        };
        assert_eq!(at(|s| s.speed = 0.277778), 0.0);
        assert_eq!(at(|s| s.rpm = 0.0), 0.0);
        assert_eq!(at(|s| s.previous_speed = 0.277778), 0.0);
        assert_eq!(at(|s| s.previous_rpm = 0.0), 0.0);
        assert_eq!(at(|s| s.gear = 0), 0.0);
        assert!(at(|_| ()) > 0.0);
    }

    #[test]
    fn nan_brake_skips_the_compiled_ordered_greater_than_branch() {
        let mut s = inputs();
        s.brake = f64::NAN;
        s.lateral_velocity = 1.25;
        assert_eq!(native_estimated_wheel_slip(WheelSide::Left, &s), LegacyValue::Int(0));
        s.throttle = 1.0;
        s.speed = 30.0; // No positive ratio change, so lateral slip wins.
        assert_eq!(estimated_wheel_slip(WheelSide::Left, &s), 5.0);
        assert_eq!(estimated_wheel_slip(WheelSide::Right, &s), 2.5);
    }

    #[test]
    fn slip_early_zero_and_calculated_zero_have_distinct_native_kinds() {
        let mut s = inputs();
        assert_eq!(native_estimated_wheel_slip(WheelSide::Left, &s), LegacyValue::Int(0));
        s.throttle = 1.0;
        s.speed = 30.0;
        assert_eq!(native_estimated_wheel_slip(WheelSide::Left, &s), LegacyValue::Float(0.0));
        s.gear = 0;
        assert_eq!(native_estimated_wheel_slip(WheelSide::Left, &s), LegacyValue::Int(0));
    }

    #[test]
    fn get_rpm_speed_slip_throttle_branch() {
        let mut s = inputs();
        s.throttle = 0.85;
        s.speed = 30.0;
        let gain = (30.0f64 / 6000.0 - 29.0 / 5000.0).max(0.0);
        // speed/rpm fell (30/6000 < 29/5000): no spin, lateral 0 -> 0.
        assert_eq!(gain, 0.0);
        assert_eq!(estimated_wheel_slip(WheelSide::Left, &s), 0.0);
        s.speed = 40.0;
        let expected = (40.0 / 6000.0 - 29.0 / 5000.0) * 4000.0 * range_fraction(70.0, 100.0, 0.85 * 100.0) * 10.0;
        assert_eq!(estimated_wheel_slip(WheelSide::Left, &s), expected);
        // Below 40 % neither branch applies.
        s.throttle = 0.375;
        assert_eq!(estimated_wheel_slip(WheelSide::Left, &s), 0.0);
    }

    #[test]
    fn get_rpm_speed_slip_brake_branch_takes_precedence() {
        let mut s = inputs();
        s.brake = 0.725;
        s.throttle = 1.0;
        let expected =
            (29.0f64 / 5000.0 - 40.0 / 6000.0).abs() * 4000.0 * range_fraction(55.0, 90.0, 0.725 * 100.0) * 10.0;
        assert_eq!(estimated_wheel_slip(WheelSide::Right, &s), expected);
        // Below 20 % braking the throttle branch applies.
        s.brake = 0.125;
        let spin = (40.0f64 / 6000.0 - 29.0 / 5000.0) * 4000.0 * range_fraction(70.0, 100.0, 100.0) * 10.0;
        assert_eq!(estimated_wheel_slip(WheelSide::Right, &s), spin);
    }

    #[test]
    fn get_rpm_speed_slip_lateral_component_halved_inside() {
        let mut s = inputs();
        s.brake = 0.3; // brake branch with a tiny ratio term
        s.previous_speed = 25.0;
        s.previous_rpm = 5000.0; // ratio 0.005 both -> ratio slip 0
        s.speed = 30.0;
        s.rpm = 6000.0;
        s.lateral_velocity = -2.0; // full lateral share, left wheel is inside
        assert_eq!(estimated_wheel_slip(WheelSide::Left, &s), 0.5 * 10.0);
        assert_eq!(estimated_wheel_slip(WheelSide::Right, &s), 10.0);
        s.lateral_velocity = 2.0;
        assert_eq!(estimated_wheel_slip(WheelSide::Right, &s), 0.5 * 10.0);
        assert_eq!(estimated_wheel_slip(WheelSide::Left, &s), 10.0);
    }
}

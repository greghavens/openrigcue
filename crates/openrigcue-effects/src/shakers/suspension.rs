//! Suspension texture, rumble strips and road bumps: travel deltas to an intensity sample.

use super::tables::{lookup, py_round, ROAD_BUMP, SUSPENSION};
use super::tuning::{DspError, NativeSuspensionSettings, ShakeboxConfig};

/// Which row set of the suspension / road-bump tables applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VelocityKind {
    /// Type 0: the game reports a shock velocity inside the configured window.
    Measured,
    /// Type 1: the game reports no (zero) velocity; the gentler rows apply.
    Missing,
}

impl VelocityKind {
    fn rows(self) -> usize {
        match self {
            VelocityKind::Measured => 0,
            VelocityKind::Missing => 1,
        }
    }
}

/// Travel of one wheel and of the other wheel of the same axle, this frame and last frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxleTravel {
    pub current: f64,
    pub previous: f64,
    pub side_current: f64,
    pub side_previous: f64,
}

/// The adjusted travel: the per-frame travel change in mm, zeroed when the
/// other wheel of the axle dominates ("side enhancement") or when a reported shock velocity is
/// outside `[min_suspension_velocity, max_suspension_velocity]` mm/s; replaced by
/// `rumble_pitch / 15` on a kerb.
pub fn adjusted_travel(
    axle: AxleTravel,
    shock_velocity: f64,
    rumble_pitch: f64,
    config: &ShakeboxConfig,
) -> (f64, VelocityKind) {
    adjusted_travel_checked(axle, shock_velocity, rumble_pitch, config, None).expect("compatibility thresholds")
}

pub(super) fn adjusted_travel_checked(
    axle: AxleTravel,
    shock_velocity: f64,
    rumble_pitch: f64,
    config: &ShakeboxConfig,
    native: Option<&NativeSuspensionSettings>,
) -> Result<(f64, VelocityKind), DspError> {
    let mut travel = (axle.current - axle.previous).abs() * 1000.0;
    let side = (axle.side_current - axle.side_previous).abs() * 1000.0;
    let velocity = shock_velocity.abs() * 1000.0;
    if travel < side {
        let enhancement = threshold(native.map(|n| &n.side_enhancement), config.side_enhancement)?;
        if greater(side - travel, enhancement) {
            let minimum = threshold(native.map(|n| &n.min_side_enhancement), config.min_side_enhancement)?;
            if greater(side, minimum) {
                travel = 0.0;
            }
        }
    }
    let kind = if velocity == 0.0 {
        VelocityKind::Missing
    } else {
        let min = threshold(native.map(|n| &n.min_suspension_velocity), config.min_suspension_velocity)?;
        let in_window = if at_least(velocity, min) {
            let max = threshold(native.map(|n| &n.max_suspension_velocity), config.max_suspension_velocity)?;
            at_most(velocity, max)
        } else {
            false
        };
        if !in_window {
            travel = 0.0;
        }
        VelocityKind::Measured
    };
    if rumble_pitch > 0.0 {
        travel = rumble_pitch / 15.0;
    }
    Ok((travel, kind))
}

/// The exact integer `SHAKEBOX` setting, or the typed setting when no raw snapshot exists.
fn threshold(
    native: Option<&Result<super::tuning::NativeInt, DspError>>,
    compatibility: i64,
) -> Result<super::tuning::NativeInt, DspError> {
    native.cloned().unwrap_or(Ok(super::tuning::NativeInt::Int(compatibility)))
}

/// A frame float against an exact integer of any width (no float rounding of the integer).
fn compare(value: f64, threshold: &super::tuning::NativeInt) -> Option<std::cmp::Ordering> {
    use openrigcue_model::{native_value::LegacyValue, python_compare::numeric_cmp};
    numeric_cmp(&LegacyValue::Float(value), threshold).expect("int() results are integers")
}

fn greater(value: f64, threshold: super::tuning::NativeInt) -> bool {
    compare(value, &threshold) == Some(std::cmp::Ordering::Greater)
}

fn at_least(value: f64, threshold: super::tuning::NativeInt) -> bool {
    matches!(compare(value, &threshold), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal))
}

fn at_most(value: f64, threshold: super::tuning::NativeInt) -> bool {
    matches!(compare(value, &threshold), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal))
}

/// The suspension sample number for one wheel (-1 = none). A parked car
/// (`rpm < 400` and `speed_kmh < 1`) selects sample 0 (silence).
pub fn suspension_sample(
    axle: AxleTravel,
    shock_velocity: f64,
    rumble_pitch: f64,
    rpm: f64,
    speed_kmh: f64,
    config: &ShakeboxConfig,
) -> i64 {
    suspension_sample_checked(axle, shock_velocity, rumble_pitch, rpm, speed_kmh, config, None)
        .expect("compatibility thresholds")
}

pub(super) fn suspension_sample_checked(
    axle: AxleTravel,
    shock_velocity: f64,
    rumble_pitch: f64,
    rpm: f64,
    speed_kmh: f64,
    config: &ShakeboxConfig,
    native: Option<&NativeSuspensionSettings>,
) -> Result<i64, DspError> {
    let (travel, kind) = if rpm < 400.0 && speed_kmh < 1.0 {
        (0.0, VelocityKind::Measured)
    } else {
        adjusted_travel_checked(axle, shock_velocity, rumble_pitch, config, native)?
    };
    Ok(lookup(&SUSPENSION[kind.rows()], travel))
}

/// The road-bump sample: the sample for
/// the vertical acceleration, gated by the largest (signed) shock velocity of the four wheels.
pub fn road_bump_sample(vertical_acceleration: f64, max_shock_velocity: f64, config: &ShakeboxConfig) -> i64 {
    road_bump_sample_checked(vertical_acceleration, max_shock_velocity, config, None).expect("compatibility thresholds")
}

pub(super) fn road_bump_sample_checked(
    vertical_acceleration: f64,
    max_shock_velocity: f64,
    config: &ShakeboxConfig,
    native: Option<&NativeSuspensionSettings>,
) -> Result<i64, DspError> {
    let mut level = py_round(vertical_acceleration.abs(), 2);
    let velocity = max_shock_velocity.abs() * 1000.0;
    let kind = if velocity == 0.0 {
        VelocityKind::Missing
    } else {
        let min = threshold(native.map(|n| &n.min_bump_velocity), config.min_bump_velocity)?;
        let in_window = if at_least(velocity, min) {
            let max = threshold(native.map(|n| &n.max_bump_velocity), config.max_bump_velocity)?;
            at_most(velocity, max)
        } else {
            false
        };
        if !in_window {
            level = 0.0;
        }
        VelocityKind::Measured
    };
    Ok(if level != 0.0 { lookup(&ROAD_BUMP[kind.rows()], level) } else { -1 })
}

/// The largest of the four shock velocities, in the order LF, LR, RF, RR (first maximum wins, NaN never replaces).
pub fn max_velocity(velocities: [f64; 4]) -> f64 {
    velocities.into_iter().skip(1).fold(velocities[0], |m, v| if v > m { v } else { m })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shakers::tuning::NativeInt;

    fn axle(cur: f64, prev: f64, side_cur: f64, side_prev: f64) -> AxleTravel {
        AxleTravel { current: cur, previous: prev, side_current: side_cur, side_previous: side_prev }
    }

    fn native() -> NativeSuspensionSettings {
        let c = ShakeboxConfig::default();
        NativeSuspensionSettings {
            side_enhancement: Ok(NativeInt::Int(c.side_enhancement)),
            min_side_enhancement: Ok(NativeInt::Int(c.min_side_enhancement)),
            min_suspension_velocity: Ok(NativeInt::Int(c.min_suspension_velocity)),
            max_suspension_velocity: Ok(NativeInt::Int(c.max_suspension_velocity)),
            min_bump_velocity: Ok(NativeInt::Int(c.min_bump_velocity)),
            max_bump_velocity: Ok(NativeInt::Int(c.max_bump_velocity)),
        }
    }

    fn bad(key: &str) -> Result<NativeInt, DspError> {
        Err(DspError { class: "KeyError", message: format!("'{key}'") })
    }

    #[test]
    fn native_suspension_side_thresholds_follow_both_short_circuits() {
        let c = ShakeboxConfig::default();
        let mut n = native();
        n.side_enhancement = bad("side");
        n.min_side_enhancement = bad("minimum-side");
        assert!(adjusted_travel_checked(axle(0.003, 0.0, 0.002, 0.0), 0.0, 0.0, &c, Some(&n)).is_ok());
        assert_eq!(
            adjusted_travel_checked(axle(0.001, 0.0, 0.005, 0.0), 0.0, 45.0, &c, Some(&n)).unwrap_err(),
            bad("side").unwrap_err()
        );
        n.side_enhancement = Ok(NativeInt::Int(10));
        assert!(adjusted_travel_checked(axle(0.001, 0.0, 0.005, 0.0), 0.0, 0.0, &c, Some(&n)).is_ok());
        n.side_enhancement = Ok(NativeInt::Int(1));
        assert_eq!(
            adjusted_travel_checked(axle(0.001, 0.0, 0.005, 0.0), 0.0, 0.0, &c, Some(&n)).unwrap_err(),
            bad("minimum-side").unwrap_err()
        );
    }

    #[test]
    fn native_suspension_velocity_and_parked_gates_skip_unreached_reads() {
        let c = ShakeboxConfig::default();
        let mut n = native();
        n.min_suspension_velocity = bad("minimum");
        n.max_suspension_velocity = bad("maximum");
        let a = axle(0.003, 0.0, 0.0, 0.0);
        assert_eq!(adjusted_travel_checked(a, 0.0, 0.0, &c, Some(&n)).unwrap(), (3.0, VelocityKind::Missing));
        assert_eq!(
            adjusted_travel_checked(a, 0.1, 45.0, &c, Some(&n)).unwrap_err(),
            bad("minimum").unwrap_err(),
            "rumble is later"
        );
        n.min_suspension_velocity = Ok(NativeInt::Int(200));
        assert_eq!(adjusted_travel_checked(a, 0.1, 0.0, &c, Some(&n)).unwrap(), (0.0, VelocityKind::Measured));
        assert_eq!(adjusted_travel_checked(a, 0.1, 45.0, &c, Some(&n)).unwrap(), (3.0, VelocityKind::Measured));
        n.min_suspension_velocity = Ok(NativeInt::Int(100));
        assert_eq!(adjusted_travel_checked(a, 0.1, 0.0, &c, Some(&n)).unwrap_err(), bad("maximum").unwrap_err());
        n.side_enhancement = bad("side");
        assert_eq!(suspension_sample_checked(a, 0.1, 45.0, 399.0, 0.5, &c, Some(&n)).unwrap(), 0);
    }

    #[test]
    fn native_road_bump_threshold_reads_precede_zero_level_check() {
        let c = ShakeboxConfig::default();
        let mut n = native();
        n.min_bump_velocity = bad("minimum-bump");
        n.max_bump_velocity = bad("maximum-bump");
        assert_eq!(road_bump_sample_checked(4.5, 0.0, &c, Some(&n)).unwrap(), 3);
        assert_eq!(road_bump_sample_checked(0.0, 0.1, &c, Some(&n)).unwrap_err(), bad("minimum-bump").unwrap_err());
        n.min_bump_velocity = Ok(NativeInt::Int(200));
        assert_eq!(road_bump_sample_checked(4.5, 0.1, &c, Some(&n)).unwrap(), -1);
        n.min_bump_velocity = Ok(NativeInt::Int(100));
        assert_eq!(road_bump_sample_checked(4.5, 0.1, &c, Some(&n)).unwrap_err(), bad("maximum-bump").unwrap_err());
    }

    /// An integer setting keeps any width: a threshold beyond i64 is compared exactly with the
    /// frame's float instead of falling back to the typed setting, so the velocity window is missed.
    #[test]
    fn native_suspension_thresholds_keep_their_exact_integer() {
        let c = ShakeboxConfig::default();
        let huge = |digits: usize| {
            NativeInt::from_integer(NativeInt::from(format!("1{}", "0".repeat(digits)).as_str()).py_integer().unwrap())
        };
        let exact = |text: &str| NativeInt::from_integer(NativeInt::from(text).py_integer().unwrap());
        // 1e30 is 1000000000000000019884624838656: greater than the exact integer 10^30.
        assert_eq!(compare(1e30, &huge(30)), Some(std::cmp::Ordering::Greater));
        assert_eq!(compare(1e30, &huge(31)), Some(std::cmp::Ordering::Less));
        assert_eq!(compare(2f64.powi(100), &exact("1267650600228229401496703205376")), Some(std::cmp::Ordering::Equal));
        assert_eq!(compare(f64::INFINITY, &huge(400)), Some(std::cmp::Ordering::Greater));
        let mut n = native();
        n.min_suspension_velocity = Ok(huge(30));
        let a = axle(0.003, 0.0, 0.0, 0.0);
        assert_eq!(adjusted_travel_checked(a, 0.1, 0.0, &c, Some(&n)).unwrap(), (0.0, VelocityKind::Measured));
        n.min_bump_velocity = Ok(huge(25));
        assert_eq!(road_bump_sample_checked(4.5, 0.1, &c, Some(&n)).unwrap(), -1);
    }

    #[test]
    fn native_suspension_threshold_comparison_keeps_integer_precision() {
        let exactly_represented = 9_007_199_254_740_992.0;
        assert!(!at_least(exactly_represented, NativeInt::Int(9_007_199_254_740_993)));
        assert!(at_most(exactly_represented, NativeInt::Int(9_007_199_254_740_993)));
        assert!(!at_least(f64::NAN, NativeInt::Int(0)));
        assert!(!at_most(f64::NAN, NativeInt::Int(0)));
        assert!(at_least(f64::INFINITY, NativeInt::Int(i64::MAX)));
        let c = ShakeboxConfig::default();
        let n = native();
        assert_eq!(
            adjusted_travel_checked(axle(0.003, 0.0, 0.0, 0.0), 0.1, 0.0, &c, Some(&n)).unwrap(),
            adjusted_travel(axle(0.003, 0.0, 0.0, 0.0), 0.1, 0.0, &c)
        );
    }

    #[test]
    fn get_adj_suspension_travel_velocity_window() {
        let c = ShakeboxConfig::default();
        // 3 mm change, 0.1 m/s = 100 mm/s inside [90, 100000] -> type 0.
        let (t, k) = adjusted_travel(axle(0.003, 0.0, 0.0, 0.0), 0.1, 0.0, &c);
        assert!((t - 3.0).abs() < 1e-9);
        assert_eq!(k, VelocityKind::Measured);
        // 0.05 m/s = 50 mm/s below 90 -> travel zeroed.
        assert_eq!(adjusted_travel(axle(0.003, 0.0, 0.0, 0.0), 0.05, 0.0, &c).0, 0.0);
        // No velocity reported -> type 1, travel kept.
        let (t, k) = adjusted_travel(axle(0.003, 0.0, 0.0, 0.0), 0.0, 0.0, &c);
        assert!((t - 3.0).abs() < 1e-9);
        assert_eq!(k, VelocityKind::Missing);
    }

    #[test]
    fn get_adj_suspension_travel_side_enhancement() {
        let c = ShakeboxConfig::default();
        // side 5 mm vs own 1 mm: 4 > 1 and 5 > 2 -> own travel zeroed.
        assert_eq!(adjusted_travel(axle(0.001, 0.0, 0.005, 0.0), 0.0, 0.0, &c).0, 0.0);
        // side 2 mm vs own 0: 2 > 1 but 2 > 2 fails -> kept.
        assert_eq!(adjusted_travel(axle(0.0, 0.0, 0.002, 0.0), 0.0, 0.0, &c).0, 0.0);
        let (t, _) = adjusted_travel(axle(0.0015, 0.0, 0.0025, 0.0), 0.0, 0.0, &c);
        assert!((t - 1.5).abs() < 1e-9); // difference 1 is not > 1
    }

    #[test]
    fn rumble_strip_replaces_travel() {
        let c = ShakeboxConfig::default();
        let (t, _) = adjusted_travel(axle(0.0, 0.0, 0.009, 0.0), 0.05, 45.0, &c);
        assert_eq!(t, 3.0);
    }

    #[test]
    fn get_suspension_vibration_parked_and_tables() {
        let c = ShakeboxConfig::default();
        assert_eq!(suspension_sample(axle(0.01, 0.0, 0.0, 0.0), 0.0, 0.0, 399.0, 0.5, &c), 0);
        // 10 mm, no velocity -> type 1 row [8, 10.99] -> 5.
        assert_eq!(suspension_sample(axle(0.01, 0.0, 0.0, 0.0), 0.0, 0.0, 400.0, 0.5, &c), 5);
        // 10 mm, velocity 1 m/s -> type 0 row [8, 10.99] -> 5; 2.5 mm -> 4.
        assert_eq!(suspension_sample(axle(0.0025, 0.0, 0.0, 0.0), 1.0, 0.0, 3000.0, 50.0, &c), 4);
    }

    #[test]
    fn get_road_bump_vibration_cases() {
        let c = ShakeboxConfig::default();
        assert_eq!(road_bump_sample(0.0, 0.5, &c), -1);
        // velocity 0 -> type 1: |-4.5| -> row [4, 5.99] -> 3.
        assert_eq!(road_bump_sample(-4.5, 0.0, &c), 3);
        // velocity 0.1 m/s = 100 mm/s in [70, 100000] -> type 0 -> 4.
        assert_eq!(road_bump_sample(-4.5, 0.1, &c), 4);
        // 0.05 m/s = 50 mm/s outside -> level 0 -> -1.
        assert_eq!(road_bump_sample(-4.5, 0.05, &c), -1);
        // 0.305 rounds to 0.3 -> type 1 row [0.11, 0.99] -> 0.
        assert_eq!(road_bump_sample(0.305, 0.0, &c), 0);
    }

    #[test]
    fn max_of_four_is_signed() {
        assert_eq!(max_velocity([-3.0, -1.0, -2.0, -5.0]), -1.0);
        assert_eq!(max_velocity([0.2, 0.4, 0.4, 0.1]), 0.4);
    }
}

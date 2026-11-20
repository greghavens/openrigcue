//! Writing a listener's channel answers into [`Telemetry`].
//!
//! The dispatcher pulls channels by name once per tick. A listener answer that
//! fails leaves the channel unchanged (most channels have no `default`); `python_int` reports such an answer as
//! `None`.

use openrigcue_model::Telemetry;

/// The integer value of a float: truncation toward zero. `None` where there is none (NaN, infinities).
pub(crate) fn python_int(x: f64) -> Option<f64> {
    // `+ 0.0` turns a truncated `-0.0` into `0`.
    x.is_finite().then(|| x.trunc() + 0.0)
}

/// List indexing: negative indices count from the end; `None` is an index out of range.
pub(crate) fn python_index(index: i64, len: usize) -> Option<usize> {
    let len = i64::try_from(len).ok()?;
    let resolved = if index < 0 { index + len } else { index };
    (0..len).contains(&resolved).then_some(resolved as usize)
}

/// Write 0 to every registry channel the dispatcher reads from a listener: the auto-loaded
/// channels plus `speed`, `lateral_velocity`, `pitch`, `roll` and `yaw`, which
/// the dispatcher reads by name. Listeners that answer 0 for unmapped channels call it
/// before writing the channels they map, and alone where they answer 0 for every name.
pub(crate) fn zero_dispatcher_channels(t: &mut Telemetry) {
    for slot in [
        &mut t.pitch,
        &mut t.roll,
        &mut t.yaw,
        &mut t.lateral_acceleration,
        &mut t.longitudinal_acceleration,
        &mut t.vertical_acceleration,
        &mut t.lateral_velocity,
        &mut t.longitudinal_velocity,
        &mut t.vertical_velocity,
        &mut t.speed,
        &mut t.max_rpm,
        &mut t.rpm,
        &mut t.gear,
        &mut t.harmonized_gear,
        &mut t.wheel_slip_lf,
        &mut t.wheel_slip_lr,
        &mut t.wheel_slip_rf,
        &mut t.wheel_slip_rr,
        &mut t.suspension_travel_lf,
        &mut t.suspension_travel_lr,
        &mut t.suspension_travel_rf,
        &mut t.suspension_travel_rr,
        &mut t.suspension_velocity_lf,
        &mut t.suspension_velocity_lr,
        &mut t.suspension_velocity_rf,
        &mut t.suspension_velocity_rr,
        &mut t.rumble_pitch_lf,
        &mut t.rumble_pitch_rf,
        &mut t.rumble_pitch_lr,
        &mut t.rumble_pitch_rr,
        &mut t.steering_wheel_angle,
        &mut t.flag,
        &mut t.raw_flag,
        &mut t.spotter_right,
        &mut t.spotter_left,
        &mut t.canopy,
        &mut t.afterburner,
        &mut t.landing_gear_left,
        &mut t.landing_gear_nose,
        &mut t.landing_gear_right,
        &mut t.turbulence,
        &mut t.touchdown_left,
        &mut t.touchdown_nose,
        &mut t.touchdown_right,
        &mut t.tyre_temperature_lf,
        &mut t.tyre_temperature_rf,
        &mut t.tyre_temperature_lr,
        &mut t.tyre_temperature_rr,
        &mut t.tyre_wear_lf,
        &mut t.tyre_wear_rf,
        &mut t.tyre_wear_lr,
        &mut t.tyre_wear_rr,
        &mut t.tyre_pressure_lf,
        &mut t.tyre_pressure_rf,
        &mut t.tyre_pressure_lr,
        &mut t.tyre_pressure_rr,
        &mut t.oil_temperature,
        &mut t.oil_pressure,
        &mut t.water_temperature,
        &mut t.water_pressure,
        &mut t.fuel_pressure,
        &mut t.fuel_level,
        &mut t.oil_level,
        &mut t.water_level,
        &mut t.turbo_boost_pressure,
        &mut t.lap,
        &mut t.lap_completed,
        &mut t.lap_dist,
        &mut t.lap_dist_percent,
        &mut t.race_laps,
        &mut t.abs_lock,
    ] {
        *slot = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_int_truncates_toward_zero_and_rejects_non_finite() {
        assert_eq!(python_int(2.9), Some(2.0));
        assert_eq!(python_int(-2.9), Some(-2.0));
        assert_eq!(python_int(-0.5).map(f64::to_bits), Some(0.0f64.to_bits()));
        assert_eq!(python_int(f64::NAN), None);
        assert_eq!(python_int(f64::INFINITY), None);
    }

    #[test]
    fn python_index_wraps_negative_indices() {
        assert_eq!(python_index(-1, 22), Some(21));
        assert_eq!(python_index(-22, 22), Some(0));
        assert_eq!(python_index(-23, 22), None);
        assert_eq!(python_index(22, 22), None);
        assert_eq!(python_index(3, 22), Some(3));
    }

    #[test]
    fn zeroing_leaves_dispatcher_owned_channels_alone() {
        let mut t = Telemetry::off();
        zero_dispatcher_channels(&mut t);
        assert_eq!((t.gear, t.max_rpm, t.steering_wheel_angle), (0.0, 0.0, 0.0));
        assert_eq!((t.max_speed, t.max_speed_kmh, t.speed_kmh), (1.0, 3.6, 0.0));
    }
}

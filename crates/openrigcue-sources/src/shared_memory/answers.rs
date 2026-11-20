//! Writing a listener's channel answers into [`Telemetry`] the way the dispatcher stores them.
//!
//! The dispatcher asks the listener for every registry channel with `auto_load`, plus `speed`,
//! `lateral_velocity`, `yaw`, `pitch`, `roll`, `car_name`, `track_loc` and the rate channels. The listeners of this
//! module answer `0` for every channel they do not implement, and `0` for everything while the game is paused (with
//! `pause_telemetry_when_not_playing`). A channel whose read failed is resolved here to what the
//! dispatcher then stores: the registry default when it has one, otherwise the previous value (left unchanged).

use openrigcue_model::Telemetry;

use super::listener_channels::begin_tick;

/// The non-registry rate channels the dispatcher requests (`yaw_rate`, `pitch_rate`, `roll_rate`), in the
/// listener's own units. `None` where the read failed.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MotionRates {
    pub yaw_rate: Option<f64>,
    pub pitch_rate: Option<f64>,
    pub roll_rate: Option<f64>,
}

impl MotionRates {
    /// Every rate channel answered `0` (a listener without rate channels, or a paused game).
    pub const ZERO: MotionRates = MotionRates { yaw_rate: Some(0.0), pitch_rate: Some(0.0), roll_rate: Some(0.0) };

    /// Every rate channel read failed (the listener has no data object).
    pub const RAISED: MotionRates = MotionRates { yaw_rate: None, pitch_rate: None, roll_rate: None };

    /// A yaw rate, with the pitch and roll rates answered `0`.
    pub fn yaw_only(yaw_rate: f64) -> Self {
        MotionRates { yaw_rate: Some(yaw_rate), ..MotionRates::ZERO }
    }
}

/// Answers `0` for every channel the dispatcher's generic loader reads (registry `auto_load`), leaving `speed`,
/// the motion channels (`lateral_velocity`, `yaw`, `pitch`, `roll`) and the names to the caller, which answers them
/// itself. The dispatcher's own channels (`harmonized_gear`, `speed_kmh`, ...) are untouched.
///
/// Channels a listener does not implement read 0; this uses
/// the crate's shared fall-through ([`begin_tick`]).
pub(crate) fn zero_generic_channels(t: &mut Telemetry) {
    let previous = begin_tick(t);
    t.speed = previous.speed;
    t.lateral_velocity = previous.lateral_velocity;
    t.yaw = previous.yaw;
    t.pitch = previous.pitch;
    t.roll = previous.roll;
    t.vehicle = previous.vehicle;
    t.location = previous.location;
}

/// Every requested channel answered `0`, as the listeners' pause gate
/// produces: every listener channel, and the integer `0` stored as vehicle and location.
pub(crate) fn write_all_zero(t: &mut Telemetry) {
    begin_tick(t);
    t.vehicle = ZERO_NAME.to_owned();
    t.location = ZERO_NAME.to_owned();
}

/// The dispatcher's outcome when every channel read failed (the listener has no data object): registry
/// defaults where they exist, `0` for the guarded motion channels, `0` as location; everything else, the vehicle
/// and `speed` keep their previous values.
pub(crate) fn write_all_raised(t: &mut Telemetry) {
    for value in [
        &mut t.rumble_pitch_lf,
        &mut t.rumble_pitch_rf,
        &mut t.rumble_pitch_lr,
        &mut t.rumble_pitch_rr,
        &mut t.steering_wheel_angle,
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
        &mut t.lateral_velocity,
        &mut t.yaw,
        &mut t.pitch,
        &mut t.roll,
    ] {
        *value = 0.0;
    }
    t.raw_flag = RAW_FLAG_DEFAULT;
    t.location = ZERO_NAME.to_owned();
}

/// The integer `0` stored as vehicle or location name. The crate represents it as the empty name,
/// as [`begin_tick`] and the UDP listeners' fall-through do, so every source reports "no name" the same way.
pub(crate) const ZERO_NAME: &str = "";

/// Registry default of `raw_flag` (written when its read failed).
pub(crate) const RAW_FLAG_DEFAULT: f64 = -1.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// Fall-through: unimplemented generic channels answer 0, harmonized_gear and motion untouched.
    #[test]
    fn unimplemented_channels_answer_zero() {
        let mut t = Telemetry::off();
        t.speed = 5.0;
        zero_generic_channels(&mut t);
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.max_rpm, 0.0);
        assert_eq!(t.steering_wheel_angle, 0.0);
        assert_eq!(t.harmonized_gear, 11.0);
        assert_eq!(t.speed, 5.0);
    }

    /// Pause gate: every requested channel 0, names become the integer 0.
    #[test]
    fn paused_answers_zero_everywhere() {
        let mut t = Telemetry::off();
        t.yaw = 12.0;
        t.vehicle = "car".into();
        write_all_zero(&mut t);
        assert_eq!(t.yaw, 0.0);
        assert_eq!(t.vehicle, ZERO_NAME);
        assert_eq!(t.location, ZERO_NAME);
    }

    /// Channels the dispatcher computes itself and never asks the listener for.
    const DISPATCHER_OWNED: [&str; 9] = [
        "speed_kmh",
        "front_tl",
        "max_speed",
        "max_speed_kmh",
        "steering_wheel_max_angle",
        "yaw_angle",
        "cannon_firing",
        "missile_firing",
        "harmonized_gear",
    ];

    /// Pause gate / fall-through: exactly the 70 channels the dispatcher requested answer 0; the
    /// fall-through alone also keeps speed, lateral_velocity, yaw, pitch, roll and the names for the caller.
    #[test]
    fn zeroed_channel_sets() {
        let mut every = Telemetry::off();
        for name in Telemetry::CHANNELS {
            *every.get_mut(name).unwrap() = 7.0;
        }
        every.vehicle = "car".into();
        let mut paused = every.clone();
        write_all_zero(&mut paused);
        let mut generic = every.clone();
        zero_generic_channels(&mut generic);
        let caller_answered = ["speed", "lateral_velocity", "yaw", "pitch", "roll"];
        for name in Telemetry::CHANNELS {
            let owned = DISPATCHER_OWNED.contains(name);
            assert_eq!(paused.get(name), Some(if owned { 7.0 } else { 0.0 }), "{name}");
            let kept = owned || caller_answered.contains(name);
            assert_eq!(generic.get(name), Some(if kept { 7.0 } else { 0.0 }), "{name}");
        }
        assert_eq!(Telemetry::CHANNELS.len() - DISPATCHER_OWNED.len(), 70);
        assert_eq!(generic.vehicle, "car");
    }

    /// Every channel read failing: defaults where defined, previous values elsewhere.
    #[test]
    fn raising_uses_registry_defaults() {
        let mut t = Telemetry::off();
        t.rpm = 4000.0;
        t.speed = 20.0;
        t.abs_lock = 1.0;
        t.vehicle = "car".into();
        write_all_raised(&mut t);
        assert_eq!(t.rpm, 4000.0);
        assert_eq!(t.speed, 20.0);
        assert_eq!(t.abs_lock, 0.0);
        assert_eq!(t.raw_flag, -1.0);
        assert_eq!(t.vehicle, "car");
        assert_eq!(t.location, ZERO_NAME);
    }
}

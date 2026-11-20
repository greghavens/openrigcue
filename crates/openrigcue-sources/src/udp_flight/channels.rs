//! What a listener publishes per tick, beyond the shared channel registry.

use openrigcue_model::Telemetry;

/// Registry channels the dispatcher computes itself instead of copying them from the listener
/// (speed in km/h, DAMS front slip, harmonized gear, maximum speed, maximum steering
/// angle, yaw angle and the two shortcut-driven weapon effects).
const DISPATCHER_CHANNELS: &[&str] = &[
    "speed_kmh",
    "front_tl",
    "harmonized_gear",
    "max_speed",
    "max_speed_kmh",
    "steering_wheel_max_angle",
    "yaw_angle",
    "cannon_firing",
    "missile_firing",
];

/// Sets every channel the dispatcher copies from the listener to 0, and clears vehicle and location.
///
/// For listeners that answer 0 for anything they do not provide: the dispatcher then
/// writes that 0 into the channel every tick. Sources call this before writing the channels they do provide.
pub(crate) fn clear_listener_channels(telemetry: &mut Telemetry) {
    clear_listener_channels_except(telemetry, &[]);
}

/// [`clear_listener_channels`] leaving `held` untouched: channels whose read can fail and whose
/// registry default is `None`, for which the dispatcher keeps the previous value (a failed read
/// writes only a default that exists).
pub(crate) fn clear_listener_channels_except(telemetry: &mut Telemetry, held: &[&str]) {
    for &name in Telemetry::CHANNELS {
        if DISPATCHER_CHANNELS.contains(&name) || held.contains(&name) {
            continue;
        }
        if let Some(value) = telemetry.get_mut(name) {
            *value = 0.0;
        }
    }
    telemetry.vehicle.clear();
    telemetry.location.clear();
}

/// Writes a channel the listener could compute; `None` (a failed computation) leaves the 0 written
/// by [`clear_listener_channels`].
pub(crate) fn assign(slot: &mut f64, value: Option<f64>) {
    if let Some(value) = value {
        *slot = value;
    }
}

/// The `pitch_rate`, `roll_rate` and `yaw_rate` channels of a listener, which the dispatcher reads by name
/// but which are not part of the channel registry. Units are whatever the game sends (see each source).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AngularRates {
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
}

/// A flight-simulator source: a [`crate::Source`] that also answers the rate channels.
///
/// For `flying` games the dispatcher feeds motion yaw from `yaw_rate`, and pitch/roll from
/// `pitch_rate`/`roll_rate` unless the profile selects angles. A listener without rates answers 0.
pub trait FlightSource: crate::Source {
    /// Rates as of the last [`crate::Source::poll`].
    fn angular_rates(&self) -> AngularRates;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clears_listener_channels_and_keeps_dispatcher_channels() {
        let mut t = Telemetry::off();
        t.vehicle = "car".into();
        t.location = "track".into();
        t.speed_kmh = 5.0;
        t.cannon_firing = 1.0;
        clear_listener_channels(&mut t);
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.max_rpm, 0.0);
        assert_eq!(t.steering_wheel_angle, 0.0);
        assert_eq!(t.harmonized_gear, 11.0);
        assert_eq!(t.speed_kmh, 5.0);
        assert_eq!(t.cannon_firing, 1.0);
        assert!(t.vehicle.is_empty() && t.location.is_empty());
    }

    #[test]
    fn held_channels_keep_their_previous_value() {
        let mut t = Telemetry::off();
        t.rpm = 3000.0;
        t.gear = 4.0;
        clear_listener_channels_except(&mut t, &["rpm"]);
        assert_eq!((t.rpm, t.gear), (3000.0, 0.0));
    }
}

//! The channel set a listener answers for, and its "anything else returns 0" rule.
//!
//! Every listener answers 0 for a channel it does not implement, and the dispatcher asks the
//! listener for every channel it does not compute itself. So on each processed tick a channel the game does
//! not provide becomes 0, and the vehicle and location names become the listener's 0 (empty here).

use openrigcue_model::Telemetry;

/// Channels the dispatcher computes itself (or overwrites after asking the listener), so a listener
/// never decides their value.
const DISPATCHER_CHANNELS: &[&str] = &[
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

/// Start a processed tick: remember the previous values (kept by channels whose read fails) and set
/// every listener-answered channel to the listener's default 0, with empty vehicle and location names.
pub(crate) fn begin_tick(telemetry: &mut Telemetry) -> Telemetry {
    let previous = telemetry.clone();
    for name in Telemetry::CHANNELS.iter().filter(|name| !DISPATCHER_CHANNELS.contains(name)) {
        if let Some(value) = telemetry.get_mut(name) {
            *value = 0.0;
        }
    }
    telemetry.vehicle.clear();
    telemetry.location.clear();
    previous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_channels_reset_to_zero_dispatcher_channels_kept() {
        let mut t = Telemetry::off();
        t.rpm = 5000.0;
        t.tyre_wear_lf = 3.0;
        t.vehicle = "car".into();
        t.location = "track".into();
        let previous = begin_tick(&mut t);
        assert_eq!(previous.rpm, 5000.0);
        assert_eq!(t.rpm, 0.0);
        assert_eq!(t.tyre_wear_lf, 0.0);
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.max_speed_kmh, 3.6);
        assert_eq!(t.harmonized_gear, 11.0);
        assert!(t.vehicle.is_empty() && t.location.is_empty());
    }
}

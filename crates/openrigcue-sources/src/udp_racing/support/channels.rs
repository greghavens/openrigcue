//! What the dispatcher stores from a listener's
//! channel answers: channels it reads, channels it computes itself, and its fallbacks when a read fails.

use openrigcue_model::Telemetry;

use crate::udp_racing::unhandled::DispatcherEnds;

/// Channels the dispatcher computes itself instead of reading them from the listener.
const DISPATCHER_OWNED: &[&str] = &[
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

/// Listener-read channels the dispatcher stores by name instead of in the auto-load loop
/// (registry entries that are not auto-loaded).
const DIRECT_READS: &[&str] = &["speed", "lateral_velocity", "yaw", "pitch", "roll"];

/// Auto-loaded channels whose registry `default` is `None`: the loop
/// leaves them unchanged when their read fails.
const KEPT_WHEN_READ_RAISES: &[&str] = &[
    "lateral_acceleration",
    "longitudinal_acceleration",
    "vertical_acceleration",
    "longitudinal_velocity",
    "vertical_velocity",
    "max_rpm",
    "rpm",
    "gear",
    "wheel_slip_lf",
    "wheel_slip_lr",
    "wheel_slip_rf",
    "wheel_slip_rr",
    "suspension_travel_lf",
    "suspension_travel_lr",
    "suspension_travel_rf",
    "suspension_travel_rr",
    "suspension_velocity_lf",
    "suspension_velocity_lr",
    "suspension_velocity_rf",
    "suspension_velocity_rr",
    "flag",
];

/// Whether the dispatcher takes `channel` from the listener (as opposed to computing it).
pub fn is_listener_read(channel: &str) -> bool {
    Telemetry::CHANNELS.contains(&channel) && !DISPATCHER_OWNED.contains(&channel)
}

/// Whether the dispatcher stores `channel` through its auto-load loop.
pub fn is_auto_load(channel: &str) -> bool {
    is_listener_read(channel) && !DIRECT_READS.contains(&channel)
}

/// Set every listener-read channel to 0, as such a listener reports for the channels it does not provide;
/// `car_name` and `track_loc` read 0 too, so the vehicle and location names are cleared. The listener then
/// overwrites what it does provide.
pub fn zero_listener_channels(t: &mut Telemetry) {
    t.vehicle.clear();
    t.location.clear();
    for name in Telemetry::CHANNELS.iter().filter(|n| !DISPATCHER_OWNED.contains(n)) {
        if let Some(v) = t.get_mut(name) {
            *v = 0.0;
        }
    }
}

/// The value the auto-load loop leaves in `channel` when its read fails (a text that is not a
/// number): the previous value for a `default: None` entry, otherwise the default (-1 for `raw_flag`, 0 for
/// the rest).
pub fn auto_load_fallback(channel: &str, previous: f64) -> f64 {
    if KEPT_WHEN_READ_RAISES.contains(&channel) {
        previous
    } else if channel == "raw_flag" {
        -1.0
    } else {
        0.0
    }
}

/// Publish a tick on which the listener's `speed` read fails (a NaN, infinite or non-numeric
/// speed has no integer value, or a listener whose every read fails): only the vehicle name (`None` when the `car_name` read
/// fails too, which keeps it), the cleared location and `lateral_velocity` (pass 0 when its own read fails)
/// are stored; every other channel keeps its value.
///
/// The dispatcher tick up to the top-speed update: failures of the `car_name` and `track_loc` reads are handled
/// (the location becomes 0), the first `speed` read only logs, `lateral_velocity` is stored
/// (0 on failure), then the top-speed update repeats the speed read, whose failure is
/// not handled: the tick ends before pitch, yaw, roll and the auto-load
/// loop.
///
/// Returns the [`DispatcherEnds`] the source reports for that tick.
pub fn publish_speed_unreadable(
    t: &mut Telemetry,
    vehicle: Option<String>,
    lateral_velocity: f64,
) -> Result<(), DispatcherEnds> {
    if let Some(name) = vehicle {
        t.vehicle = name;
    }
    t.location.clear();
    t.lateral_velocity = lateral_velocity;
    Err(speed_unreadable())
}

/// The top-speed update's failed speed read, which ends the dispatcher.
pub fn speed_unreadable() -> DispatcherEnds {
    DispatcherEnds::speed_read_raised()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_dispatcher_channels_alone() {
        let mut t = Telemetry::off();
        zero_listener_channels(&mut t);
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.max_rpm, 0.0);
        assert_eq!(t.steering_wheel_angle, 0.0);
        assert_eq!(t.max_speed_kmh, 3.6);
        assert_eq!(t.harmonized_gear, 11.0);
        t.vehicle = "car".into();
        zero_listener_channels(&mut t);
        assert!(t.vehicle.is_empty());
    }

    #[test]
    fn auto_load_fallback_follows_the_registry_default() {
        // Auto-load fallback: default None keeps, raw_flag -1, other defaults 0
        assert_eq!(auto_load_fallback("rpm", 5000.0), 5000.0);
        assert_eq!(auto_load_fallback("flag", 2.0), 2.0);
        assert_eq!(auto_load_fallback("vertical_acceleration", 1.5), 1.5);
        assert_eq!(auto_load_fallback("raw_flag", 3.0), -1.0);
        assert_eq!(auto_load_fallback("fuel_level", 40.0), 0.0);
        assert_eq!(auto_load_fallback("steering_wheel_angle", 12.0), 0.0);
    }

    #[test]
    fn auto_load_excludes_direct_and_dispatcher_channels() {
        // pitch/roll/yaw/lateral_velocity/speed are not auto-loaded
        for direct in DIRECT_READS {
            assert!(is_listener_read(direct) && !is_auto_load(direct), "{direct}");
        }
        assert!(is_auto_load("rpm") && is_auto_load("raw_flag"));
        assert!(!is_listener_read("yaw_angle") && !is_listener_read("car_name"));
    }

    #[test]
    fn speed_unreadable_stores_only_name_location_and_lateral_velocity() {
        // The top-speed update's failed speed read is not handled
        let before = Telemetry {
            rpm: 5000.0,
            speed: 20.0,
            pitch: 4.0,
            raw_flag: 2.0,
            lateral_velocity: 0.7,
            vehicle: "car".into(),
            location: "track".into(),
            ..Telemetry::off()
        };
        let mut t = before.clone();
        assert!(publish_speed_unreadable(&mut t, None, 0.0).is_err());
        let expected = Telemetry { lateral_velocity: 0.0, location: String::new(), ..before.clone() };
        assert_eq!(t, expected);
        let mut t = before.clone();
        assert!(publish_speed_unreadable(&mut t, Some(String::new()), 3.5).is_err());
        assert_eq!((t.vehicle.as_str(), t.lateral_velocity, t.pitch, t.speed), ("", 3.5, 4.0, 20.0));
    }
}

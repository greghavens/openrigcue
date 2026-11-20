//! Pieces shared by the listeners of this module: which channels the dispatcher takes from a listener,
//! what it writes when a listener's channel read fails, and the quaternion angles.
//! (The create-or-open mapping rule lives in `shared_memory::block_reader::KeptMapping`.)
//!
//! The "anything else reads 0" rule, the radian-to-degree conversion and the ±90° fold come from `udp_flight`, so the crate
//! keeps one copy of each.

use openrigcue_model::Telemetry;

use crate::udp_flight::clear_listener_channels;
use crate::udp_flight::python::rad_to_deg;

/// Whether the dispatcher reads `name` from the listener (as opposed to computing it): exactly the
/// registry channels the shared "anything else reads 0" rule resets.
pub(crate) fn is_listener_channel(name: &str) -> bool {
    let mut probe = Telemetry::off();
    let Some(slot) = probe.get_mut(name) else { return false };
    *slot = f64::NAN;
    clear_listener_channels(&mut probe);
    probe.get(name) == Some(0.0)
}

/// Auto-loaded registry channels whose registry entry has a default. The
/// dispatcher's registry loop writes that default when the listener's read of the channel failed;
/// every other channel keeps its value.
const REGISTRY_DEFAULTS: &[(&str, f64)] = &[
    ("rumble_pitch_lf", 0.0),
    ("rumble_pitch_rf", 0.0),
    ("rumble_pitch_lr", 0.0),
    ("rumble_pitch_rr", 0.0),
    ("steering_wheel_angle", 0.0),
    ("raw_flag", -1.0),
    ("spotter_right", 0.0),
    ("spotter_left", 0.0),
    ("canopy", 0.0),
    ("afterburner", 0.0),
    ("landing_gear_left", 0.0),
    ("landing_gear_nose", 0.0),
    ("landing_gear_right", 0.0),
    ("turbulence", 0.0),
    ("touchdown_left", 0.0),
    ("touchdown_nose", 0.0),
    ("touchdown_right", 0.0),
    ("tyre_temperature_lf", 0.0),
    ("tyre_temperature_rf", 0.0),
    ("tyre_temperature_lr", 0.0),
    ("tyre_temperature_rr", 0.0),
    ("tyre_wear_lf", 0.0),
    ("tyre_wear_rf", 0.0),
    ("tyre_wear_lr", 0.0),
    ("tyre_wear_rr", 0.0),
    ("tyre_pressure_lf", 0.0),
    ("tyre_pressure_rf", 0.0),
    ("tyre_pressure_lr", 0.0),
    ("tyre_pressure_rr", 0.0),
    ("oil_temperature", 0.0),
    ("oil_pressure", 0.0),
    ("water_temperature", 0.0),
    ("water_pressure", 0.0),
    ("fuel_pressure", 0.0),
    ("fuel_level", 0.0),
    ("oil_level", 0.0),
    ("water_level", 0.0),
    ("turbo_boost_pressure", 0.0),
    ("lap", 0.0),
    ("lap_completed", 0.0),
    ("lap_dist", 0.0),
    ("lap_dist_percent", 0.0),
    ("race_laps", 0.0),
    ("abs_lock", 0.0),
];

/// The value the dispatcher's registry loop writes into `name` when its read failed; `None` keeps the
/// channel's value.
pub(crate) fn registry_default(name: &str) -> Option<f64> {
    REGISTRY_DEFAULTS.iter().find(|(channel, _)| *channel == name).map(|&(_, value)| value)
}

/// Writes a registry channel by name; names outside the registry (`car_name`, `track_loc`, rates) are ignored.
pub(crate) fn set_channel(telemetry: &mut Telemetry, name: &str, value: f64) {
    if let Some(slot) = telemetry.get_mut(name) {
        *slot = value;
    }
}

/// An orientation quaternion as the games deliver it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quaternion {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub w: f64,
}

impl Quaternion {
    /// Pitch in degrees:
    /// `deg(atan2(2(wx - zy), sqrt((2(wy + xy))² + (1 - 2(x² + y²))²)))`.
    pub fn pitch_deg(&self) -> f64 {
        let Quaternion { x, y, z, w } = *self;
        let numerator = 2.0 * (w * x - z * y);
        let a = 2.0 * (w * y + x * y);
        let b = 1.0 - 2.0 * (x * x + y * y);
        rad_to_deg(numerator.atan2((a * a + b * b).sqrt()))
    }

    /// Roll in degrees:
    /// `deg(atan2(2(yx + wz), 1 - 2(x² + z²)))`.
    pub fn roll_deg(&self) -> f64 {
        let Quaternion { x, y, z, w } = *self;
        rad_to_deg((2.0 * (y * x + w * z)).atan2(1.0 - 2.0 * (x * x + z * z)))
    }

    /// Yaw in degrees, which ignores `z`:
    /// `deg(atan2(2(wy + xy), 1 - 2(x² + y²)))`.
    pub fn yaw_deg(&self) -> f64 {
        let Quaternion { x, y, w, .. } = *self;
        rad_to_deg((2.0 * (w * y + x * y)).atan2(1.0 - 2.0 * (x * x + y * y)))
    }
}

/// What the dispatcher writes for a tick in which every channel read failed:
/// the vehicle name and speed keep their values, the location reads 0 (empty), lateral velocity, yaw,
/// pitch and roll store 0, and each auto-loaded registry channel takes its registry default (or keeps its value).
pub(crate) fn apply_raising_tick(telemetry: &mut Telemetry) {
    telemetry.location.clear();
    telemetry.lateral_velocity = 0.0;
    telemetry.yaw = 0.0;
    telemetry.pitch = 0.0;
    telemetry.roll = 0.0;
    for (name, value) in REGISTRY_DEFAULTS {
        set_channel(telemetry, name, *value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dispatcher takes every registry channel from the listener except those it computes itself.
    #[test]
    fn listener_channels_exclude_dispatcher_channels() {
        for channel in ["rpm", "gear", "speed", "lateral_velocity", "raw_flag"] {
            assert!(is_listener_channel(channel), "{channel}");
        }
        for channel in ["harmonized_gear", "max_speed_kmh", "speed_kmh", "yaw_angle", "cannon_firing", "car_name"] {
            assert!(!is_listener_channel(channel), "{channel}");
        }
    }

    /// Quaternion pitch, roll and yaw, with a fixed operand order.
    #[test]
    fn quaternion_angles_match_functions_module() {
        let (x, y, z, w) = (0.1_f64, 0.2_f64, 0.3_f64, 0.9_f64);
        let q = Quaternion { x, y, z, w };
        let a = (w * y + x * y) * 2.0;
        let b = 1.0 - (x * x + y * y) * 2.0;
        let pitch = ((w * x - z * y) * 2.0).atan2((a * a + b * b).sqrt());
        assert_eq!(q.pitch_deg(), rad_to_deg(pitch));
        let roll = ((y * x + w * z) * 2.0).atan2(1.0 - (x * x + z * z) * 2.0);
        assert_eq!(q.roll_deg(), rad_to_deg(roll));
        let yaw = ((w * y + x * y) * 2.0).atan2(1.0 - (x * x + y * y) * 2.0);
        assert_eq!(q.yaw_deg(), rad_to_deg(yaw));
        assert!(q.pitch_deg() > 0.0 && q.roll_deg() > 0.0 && q.yaw_deg() > 0.0);
        // z does not take part in the yaw.
        assert_eq!(Quaternion { z: 5.0, ..q }.yaw_deg(), q.yaw_deg());
        // Identity quaternion: level.
        let identity = Quaternion { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };
        assert_eq!((identity.pitch_deg(), identity.roll_deg(), identity.yaw_deg()), (0.0, 0.0, 0.0));
    }

    /// A tick whose every channel read fails.
    #[test]
    fn raising_tick_follows_dispatcher_handlers() {
        let mut t = Telemetry::off();
        (t.speed, t.rpm, t.gear, t.raw_flag, t.steering_wheel_angle) = (40.0, 3000.0, 3.0, 2.0, 15.0);
        (t.lateral_velocity, t.yaw, t.pitch, t.roll, t.harmonized_gear) = (1.0, 2.0, 3.0, 4.0, 5.0);
        (t.vehicle, t.location) = ("car".into(), "track".into());
        apply_raising_tick(&mut t);
        assert_eq!((t.speed, t.rpm, t.gear, t.harmonized_gear), (40.0, 3000.0, 3.0, 5.0));
        assert_eq!((t.raw_flag, t.steering_wheel_angle), (-1.0, 0.0));
        assert_eq!((t.lateral_velocity, t.yaw, t.pitch, t.roll), (0.0, 0.0, 0.0, 0.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("car", ""));
    }

    /// The registry's `default` column (auto-loaded entries).
    #[test]
    fn registry_defaults_follow_global_variables() {
        assert_eq!(registry_default("raw_flag"), Some(-1.0));
        assert_eq!(registry_default("steering_wheel_angle"), Some(0.0));
        assert_eq!(registry_default("abs_lock"), Some(0.0));
        for none in ["gear", "rpm", "flag", "wheel_slip_lf", "suspension_travel_rr", "harmonized_gear"] {
            assert_eq!(registry_default(none), None, "{none}");
        }
        // Not auto-loaded, so the registry loop never asks for them.
        assert_eq!(registry_default("cannon_firing"), None);
        assert!(REGISTRY_DEFAULTS.iter().all(|(name, _)| Telemetry::CHANNELS.contains(name)));
        assert_eq!(REGISTRY_DEFAULTS.len(), 44);
    }
}

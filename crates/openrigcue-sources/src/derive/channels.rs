//! Per-channel metadata of the channel registry.
//!
//! The "value when off" of each channel lives in [`openrigcue_model::Telemetry::off`]; this table adds the columns the
//! dispatcher needs: the display label, whether the generic loader reads the channel from the listener, the
//! fallback written when the listener cannot provide it, the rounding applied, and whether the telemetry view and
//! CSV export list it.

/// Registry columns of one channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelSpec {
    /// Registry name, identical to the [`openrigcue_model::Telemetry`] field.
    pub name: &'static str,
    /// Label shown in the telemetry view and written to the CSV export.
    pub label: &'static str,
    /// Read from the listener by the generic loader every processed tick (`auto_load`).
    pub auto_load: bool,
    /// Written as an integer when the listener cannot provide the channel (`default`).
    /// Every defined default is 0 or -1; `None` keeps the previous value.
    pub fallback: Option<f64>,
    /// Decimal digits the loaded value is rounded to (`round`).
    pub round_digits: Option<i32>,
    /// Listed in the telemetry view and export (`display`), unless every channel is shown.
    pub display: bool,
}

const fn spec(
    name: &'static str,
    label: &'static str,
    auto_load: bool,
    fallback: Option<f64>,
    round_digits: Option<i32>,
    display: bool,
) -> ChannelSpec {
    ChannelSpec { name, label, auto_load, fallback, round_digits, display }
}

/// Every registry channel in registry order.
pub const CHANNEL_SPECS: &[ChannelSpec] = &[
    spec("pitch", "Pitch", false, None, None, true),
    spec("roll", "Roll", false, None, None, true),
    spec("yaw", "Yaw", false, None, None, true),
    spec("lateral_acceleration", "Lat G Sway", true, None, None, true),
    spec("longitudinal_acceleration", "Long G Surge Decel / Accel", true, None, None, true),
    spec("vertical_acceleration", "Vert G Heave", true, None, None, true),
    spec("lateral_velocity", "Traction Loss", false, None, None, true),
    spec("longitudinal_velocity", "Longitudinal Velocity", true, None, None, true),
    spec("vertical_velocity", "Vertical Velocity", true, None, None, true),
    spec("speed", "Speed", false, None, None, true),
    spec("speed_kmh", "Speed KM/H", false, None, None, true),
    spec("front_tl", "Front TL", false, None, None, true),
    spec("max_rpm", "Max Rpm", true, None, None, false),
    spec("rpm", "RPM", true, None, None, true),
    spec("gear", "Gear", true, None, None, true),
    spec("harmonized_gear", "Harmonized Gear", true, None, None, false),
    spec("wheel_slip_lf", "Wheel Slip LF", true, None, None, true),
    spec("wheel_slip_lr", "Wheel Slip LR", true, None, None, true),
    spec("wheel_slip_rf", "Wheel Slip RF", true, None, None, true),
    spec("wheel_slip_rr", "Wheel Slip RR", true, None, None, true),
    spec("suspension_travel_lf", "Suspension Pos LF", true, None, None, true),
    spec("suspension_travel_lr", "Suspension Pos LR", true, None, None, true),
    spec("suspension_travel_rf", "Suspension Pos RF", true, None, None, true),
    spec("suspension_travel_rr", "Suspension Pos RR", true, None, None, true),
    spec("suspension_velocity_lf", "Suspension Speed LF", true, None, None, true),
    spec("suspension_velocity_lr", "Suspension Speed LR", true, None, None, true),
    spec("suspension_velocity_rf", "Suspension Speed RF", true, None, None, true),
    spec("suspension_velocity_rr", "Suspension Speed RR", true, None, None, true),
    spec("rumble_pitch_lf", "Rumble Pitch LF", true, Some(0.0), None, false),
    spec("rumble_pitch_rf", "Rumble Pitch RF", true, Some(0.0), None, false),
    spec("rumble_pitch_lr", "Rumble Pitch LR", true, Some(0.0), None, false),
    spec("rumble_pitch_rr", "Rumble Pitch RR", true, Some(0.0), None, false),
    spec("max_speed", "Max Speed", false, None, None, false),
    spec("max_speed_kmh", "Max Speed KM/H", false, None, None, false),
    spec("steering_wheel_angle", "Steering Wheel Angle", true, Some(0.0), Some(2), true),
    spec("steering_wheel_max_angle", "Steering Wheel Max Angle", false, None, Some(2), false),
    spec("flag", "Flag", true, None, None, true),
    spec("raw_flag", "Raw Flag", true, Some(-1.0), None, true),
    spec("spotter_right", "Spotter Right", true, Some(0.0), None, true),
    spec("spotter_left", "Spotter Left", true, Some(0.0), None, true),
    spec("yaw_angle", "Yaw Angle", false, None, None, true),
    spec("cannon_firing", "Cannon", false, Some(0.0), None, true),
    spec("missile_firing", "Missile", false, Some(0.0), None, true),
    spec("canopy", "Canopy", true, Some(0.0), None, true),
    spec("afterburner", "Afterburner", true, Some(0.0), None, true),
    spec("landing_gear_left", "Landing Gear Left", true, Some(0.0), None, true),
    spec("landing_gear_nose", "Landing Gear Nose", true, Some(0.0), None, true),
    spec("landing_gear_right", "Landing Gear Right", true, Some(0.0), None, true),
    spec("turbulence", "Turbulence", true, Some(0.0), None, true),
    spec("touchdown_left", "Touchdown Left", true, Some(0.0), None, true),
    spec("touchdown_nose", "Touchdown Nose", true, Some(0.0), None, true),
    spec("touchdown_right", "Touchdown Right", true, Some(0.0), None, true),
    spec("tyre_temperature_lf", "Tyre Temperature LF", true, Some(0.0), None, true),
    spec("tyre_temperature_rf", "Tyre Temperature RF", true, Some(0.0), None, true),
    spec("tyre_temperature_lr", "Tyre Temperature LR", true, Some(0.0), None, true),
    spec("tyre_temperature_rr", "Tyre Temperature RR", true, Some(0.0), None, true),
    spec("tyre_wear_lf", "Tyre Wear LF", true, Some(0.0), None, true),
    spec("tyre_wear_rf", "Tyre Wear RF", true, Some(0.0), None, true),
    spec("tyre_wear_lr", "Tyre Wear LR", true, Some(0.0), None, true),
    spec("tyre_wear_rr", "Tyre Wear RR", true, Some(0.0), None, true),
    spec("tyre_pressure_lf", "Tyre Pressure LF", true, Some(0.0), None, true),
    spec("tyre_pressure_rf", "Tyre Pressure RF", true, Some(0.0), None, true),
    spec("tyre_pressure_lr", "Tyre Pressure LR", true, Some(0.0), None, true),
    spec("tyre_pressure_rr", "Tyre Pressure RR", true, Some(0.0), None, true),
    spec("oil_temperature", "Oil Temperature", true, Some(0.0), None, true),
    spec("oil_pressure", "Oil Pressure", true, Some(0.0), None, true),
    spec("water_temperature", "Water Temperature", true, Some(0.0), None, true),
    spec("water_pressure", "Water Pressure", true, Some(0.0), None, true),
    spec("fuel_pressure", "Fuel Pressure", true, Some(0.0), None, true),
    spec("fuel_level", "Fuel Level", true, Some(0.0), None, true),
    spec("oil_level", "Oil Level", true, Some(0.0), None, true),
    spec("water_level", "Water Level", true, Some(0.0), None, true),
    spec("turbo_boost_pressure", "Turbo Boost Pressure", true, Some(0.0), None, true),
    spec("lap", "Lap", true, Some(0.0), None, true),
    spec("lap_completed", "Lap Completed", true, Some(0.0), None, true),
    spec("lap_dist", "Lap Distance", true, Some(0.0), None, true),
    spec("lap_dist_percent", "Lap Distance Percentage", true, Some(0.0), None, true),
    spec("race_laps", "Race Laps", true, Some(0.0), None, true),
    spec("abs_lock", "ABS Lock Active", true, Some(0.0), None, true),
];

/// Looks up the registry columns of a channel by name.
pub fn channel_spec(name: &str) -> Option<&'static ChannelSpec> {
    CHANNEL_SPECS.iter().find(|spec| spec.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_model::Telemetry;

    /// Same channels, same order as the shared telemetry registry.
    #[test]
    fn specs_follow_the_shared_registry_order() {
        let names: Vec<&str> = CHANNEL_SPECS.iter().map(|spec| spec.name).collect();
        assert_eq!(names, Telemetry::CHANNELS);
    }

    /// The generic loader's fallbacks and rounding.
    #[test]
    fn loader_columns_match_the_registry() {
        let swa = channel_spec("steering_wheel_angle").unwrap();
        assert_eq!((swa.fallback, swa.round_digits, swa.auto_load), (Some(0.0), Some(2), true));
        assert_eq!(channel_spec("raw_flag").unwrap().fallback, Some(-1.0));
        assert_eq!(channel_spec("flag").unwrap().fallback, None);
        assert!(channel_spec("harmonized_gear").unwrap().auto_load);
        let loaded = CHANNEL_SPECS.iter().filter(|spec| spec.auto_load).count();
        assert_eq!(loaded, 79 - 13);
        let hidden: Vec<&str> = CHANNEL_SPECS.iter().filter(|s| !s.display).map(|s| s.name).collect();
        assert_eq!(
            hidden,
            [
                "max_rpm",
                "harmonized_gear",
                "rumble_pitch_lf",
                "rumble_pitch_rf",
                "rumble_pitch_lr",
                "rumble_pitch_rr",
                "max_speed",
                "max_speed_kmh",
                "steering_wheel_max_angle"
            ]
        );
    }
}

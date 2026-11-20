//! The telemetry channel set shared by every source, the motion pipeline, shakers and accessories.
//!
//! A registry of 79 channels: each channel's name, its
//! units (whatever each game listener delivers), and the
//! "value when off" that is injected while no game is running.

use crate::native_value::LegacyValue;
use serde::{Deserialize, Serialize};

/// Typed answers for listeners that deliver exact value kinds. Numeric fields remain
/// the compatibility view for the other listeners and consumers. Each answer remembers that view's
/// bits so a later direct numeric assignment cannot expose a stale typed answer.
#[derive(Debug, Clone, Default)]
pub struct NativeChannels {
    values: std::collections::BTreeMap<String, (u64, LegacyValue)>,
    // Known registry defaults use static values, avoiding a dictionary allocation per off frame.
    off_mask: u128,
    // None is the complete off/compatibility frame. A live dispatcher starts with Some(empty).
    order: Option<Vec<&'static str>>,
}

impl NativeChannels {
    fn stored(&self, index: usize, name: &str) -> Option<(u64, &LegacyValue)> {
        self.values.get(name).map(|(bits, value)| (*bits, value)).or_else(|| {
            (self.off_mask & (1_u128 << index) != 0).then(|| {
                let value = &Telemetry::OFF_VALUES[index];
                (value.number().expect("numeric registry default").to_bits(), value)
            })
        })
    }
}

impl PartialEq for NativeChannels {
    fn eq(&self, other: &Self) -> bool {
        // Equality ignores insertion order.
        let left = self.order.as_deref().unwrap_or(Telemetry::CHANNELS);
        let right = other.order.as_deref().unwrap_or(Telemetry::CHANNELS);
        left.len() == right.len()
            && left.iter().all(|name| right.contains(name))
            && Telemetry::CHANNELS
                .iter()
                .enumerate()
                .filter(|(_, name)| left.contains(name))
                .all(|(index, name)| self.stored(index, name) == other.stored(index, name))
    }
}

// The registry has 78 integer defaults and one fractional float (3.6).
// The standby fixture guards both the values and their types.
const fn registry_off_value(value: f64) -> LegacyValue {
    if value == (value as i64) as f64 {
        LegacyValue::Int(value as i64)
    } else {
        LegacyValue::Float(value)
    }
}

macro_rules! channels {
    ($( $name:ident, $label:literal, $off:expr; )*) => {
        /// Numeric views plus typed channel answers. The motion loop clears numeric NaNs.
        #[derive(Debug, Clone, PartialEq, Deserialize)]
        pub struct Telemetry {
            /// Game, vehicle and track identifiers as reported by the source (may be empty).
            pub game: String,
            pub vehicle: String,
            pub location: String,
            #[serde(skip)]
            pub native_channels: NativeChannels,
            $( #[doc = $label] pub $name: f64, )*
        }

        impl Telemetry {
            /// Every channel name, in registry order.
            pub const CHANNELS: &'static [&'static str] = &[$(stringify!($name)),*];

            /// Registry defaults with their integer/float kinds.
            pub const OFF_VALUES: &'static [LegacyValue] = &[$(registry_off_value($off)),*];

            /// The stand-by state: every channel at its "value when off".
            pub fn off() -> Self {
                Telemetry { game: String::new(), vehicle: String::new(), location: String::new(), native_channels: NativeChannels { off_mask: (1_u128 << Self::CHANNELS.len()) - 1, ..Default::default() }, $( $name: $off, )* }
            }

            /// Reset registry values, retaining identity and key-order metadata. Writers record
            /// their assignment sequence separately (the emulator writes its eight special keys first).
            pub fn reset_to_off(&mut self) {
                $( self.$name = $off; )*
                self.native_channels.values.clear();
                self.native_channels.off_mask = (1_u128 << Self::CHANNELS.len()) - 1;
            }

            /// Channel value by registry name.
            pub fn get(&self, channel: &str) -> Option<f64> {
                match channel { $( stringify!($name) => Some(self.$name), )* _ => None }
            }

            /// Mutable channel by registry name.
            pub fn get_mut(&mut self, channel: &str) -> Option<&mut f64> {
                self.native_channels.values.remove(channel);
                if let Some(index) = Self::CHANNELS.iter().position(|name| *name == channel) {
                    self.native_channels.off_mask &= !(1_u128 << index);
                }
                match channel { $( stringify!($name) => Some(&mut self.$name), )* _ => None }
            }

            /// Set a typed answer and its numeric compatibility view. Nonnumeric values project to
            /// NaN, while `native_value` keeps the actual value for checked consumers.
            pub fn set_native(&mut self, channel: &str, value: LegacyValue) {
                let Some(slot) = self.get_mut(channel) else { return };
                *slot = value.number().unwrap_or(f64::NAN);
                let bits = slot.to_bits();
                self.native_channels.values.insert(channel.to_owned(), (bits, value));
                if let Some(&name) = Self::CHANNELS.iter().find(|&&name| name == channel) {
                    self.assigned(name);
                }
            }

            /// The typed value, if its numeric view has not since been changed directly.
            pub fn native_value(&self, channel: &str) -> Option<&LegacyValue> {
                if self.native_channels.order.as_ref().is_some_and(|order| !order.contains(&channel)) { return None }
                let index = Self::CHANNELS.iter().position(|name| *name == channel)?;
                let (bits, value) = self.native_channels.stored(index, channel)?;
                (self.get(channel)?.to_bits() == bits).then_some(value)
            }

            /// A live frame starts with an empty dict; numeric off fields remain a compatibility
            /// view until assigned. Presence and insertion order travel with published snapshots.
            pub fn unfilled() -> Self {
                let mut telemetry = Self::off();
                telemetry.native_channels.off_mask = 0;
                telemetry.native_channels.order = Some(Vec::new());
                telemetry
            }

            pub fn assigned(&mut self, channel: &'static str) {
                if let Some(order) = &mut self.native_channels.order {
                    if !order.contains(&channel) { order.push(channel); }
                }
            }

            pub fn channel_names(&self) -> &[&'static str] {
                self.native_channels.order.as_deref().unwrap_or(Self::CHANNELS)
            }

            /// Numeric compatibility view only when the shared dictionary contains this key.
            pub fn get_assigned(&self, channel: &str) -> Option<f64> {
                self.channel_names().contains(&channel).then(|| self.get(channel)).flatten()
            }

            /// Clears numeric NaNs; a text/None projection is not a numeric NaN. The checked motion
            /// scan tests each value for NaN in order and stops on those unsupported values.
            pub fn clear_nan(&mut self) -> Vec<&'static str> {
                let mut found = Vec::new();
                for name in self.channel_names().to_vec() {
                    if self.native_value(name).is_some_and(|value| value.number().is_none()) { continue }
                    if self.get(name).is_some_and(f64::is_nan) {
                        self.set_native(name, LegacyValue::Int(0));
                        found.push(name);
                    }
                }
                found
            }
        }

        impl Serialize for Telemetry {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(3 + self.channel_names().len()))?;
                map.serialize_entry("game", &self.game)?;
                map.serialize_entry("vehicle", &self.vehicle)?;
                map.serialize_entry("location", &self.location)?;
                for name in self.channel_names() {
                    if let Some(value) = self.native_value(name) {
                        map.serialize_entry(name, value)?;
                    } else {
                        map.serialize_entry(name, &self.get(name).expect("registry channel"))?;
                    }
                }
                map.end()
            }
        }
    };
}

impl Default for Telemetry {
    fn default() -> Self {
        Telemetry::off()
    }
}

channels! {
    pitch, "Pitch", 0.0;
    roll, "Roll", 0.0;
    yaw, "Yaw", 0.0;
    lateral_acceleration, "Lat G Sway", 0.0;
    longitudinal_acceleration, "Long G Surge Decel / Accel", 0.0;
    vertical_acceleration, "Vert G Heave", 0.0;
    lateral_velocity, "Traction Loss", 0.0;
    longitudinal_velocity, "Longitudinal Velocity", 0.0;
    vertical_velocity, "Vertical Velocity", 0.0;
    speed, "Speed", 0.0;
    speed_kmh, "Speed KM/H", 0.0;
    front_tl, "Front TL", 0.0;
    max_rpm, "Max Rpm", 20000.0;
    rpm, "RPM", 0.0;
    gear, "Gear", 11.0;
    harmonized_gear, "Harmonized Gear", 11.0;
    wheel_slip_lf, "Wheel Slip LF", 0.0;
    wheel_slip_lr, "Wheel Slip LR", 0.0;
    wheel_slip_rf, "Wheel Slip RF", 0.0;
    wheel_slip_rr, "Wheel Slip RR", 0.0;
    suspension_travel_lf, "Suspension Pos LF", 0.0;
    suspension_travel_lr, "Suspension Pos LR", 0.0;
    suspension_travel_rf, "Suspension Pos RF", 0.0;
    suspension_travel_rr, "Suspension Pos RR", 0.0;
    suspension_velocity_lf, "Suspension Speed LF", 0.0;
    suspension_velocity_lr, "Suspension Speed LR", 0.0;
    suspension_velocity_rf, "Suspension Speed RF", 0.0;
    suspension_velocity_rr, "Suspension Speed RR", 0.0;
    rumble_pitch_lf, "Rumble Pitch LF", 0.0;
    rumble_pitch_rf, "Rumble Pitch RF", 0.0;
    rumble_pitch_lr, "Rumble Pitch LR", 0.0;
    rumble_pitch_rr, "Rumble Pitch RR", 0.0;
    max_speed, "Max Speed", 1.0;
    max_speed_kmh, "Max Speed KM/H", 3.6;
    steering_wheel_angle, "Steering Wheel Angle", 30.0;
    steering_wheel_max_angle, "Steering Wheel Max Angle", 0.0;
    flag, "Flag", 0.0;
    raw_flag, "Raw Flag", 0.0;
    spotter_right, "Spotter Right", 0.0;
    spotter_left, "Spotter Left", 0.0;
    yaw_angle, "Yaw Angle", 0.0;
    cannon_firing, "Cannon", 0.0;
    missile_firing, "Missile", 0.0;
    canopy, "Canopy", 0.0;
    afterburner, "Afterburner", 0.0;
    landing_gear_left, "Landing Gear Left", 0.0;
    landing_gear_nose, "Landing Gear Nose", 0.0;
    landing_gear_right, "Landing Gear Right", 0.0;
    turbulence, "Turbulence", 0.0;
    touchdown_left, "Touchdown Left", 0.0;
    touchdown_nose, "Touchdown Nose", 0.0;
    touchdown_right, "Touchdown Right", 0.0;
    tyre_temperature_lf, "Tyre Temperature LF", 0.0;
    tyre_temperature_rf, "Tyre Temperature RF", 0.0;
    tyre_temperature_lr, "Tyre Temperature LR", 0.0;
    tyre_temperature_rr, "Tyre Temperature RR", 0.0;
    tyre_wear_lf, "Tyre Wear LF", 0.0;
    tyre_wear_rf, "Tyre Wear RF", 0.0;
    tyre_wear_lr, "Tyre Wear LR", 0.0;
    tyre_wear_rr, "Tyre Wear RR", 0.0;
    tyre_pressure_lf, "Tyre Pressure LF", 0.0;
    tyre_pressure_rf, "Tyre Pressure RF", 0.0;
    tyre_pressure_lr, "Tyre Pressure LR", 0.0;
    tyre_pressure_rr, "Tyre Pressure RR", 0.0;
    oil_temperature, "Oil Temperature", 0.0;
    oil_pressure, "Oil Pressure", 0.0;
    water_temperature, "Water Temperature", 0.0;
    water_pressure, "Water Pressure", 0.0;
    fuel_pressure, "Fuel Pressure", 0.0;
    fuel_level, "Fuel Level", 0.0;
    oil_level, "Oil Level", 0.0;
    water_level, "Water Level", 0.0;
    turbo_boost_pressure, "Turbo Boost Pressure", 0.0;
    lap, "Lap", 0.0;
    lap_completed, "Lap Completed", 0.0;
    lap_dist, "Lap Distance", 0.0;
    lap_dist_percent, "Lap Distance Percentage", 0.0;
    race_laps, "Race Laps", 0.0;
    abs_lock, "ABS Lock Active", 0.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cache_mutation_reset_and_explicit_same_value_assignment() {
        let mut telemetry = Telemetry::off();
        assert_eq!(telemetry.native_value("speed_kmh"), Some(&LegacyValue::Int(0)));
        telemetry.set_native("speed_kmh", LegacyValue::Float(0.0));
        assert_eq!(telemetry.native_value("speed_kmh"), Some(&LegacyValue::Float(0.0)));
        telemetry.set_native("pitch", LegacyValue::Int(0));
        let mut expected = Telemetry::off();
        expected.set_native("speed_kmh", LegacyValue::Float(0.0));
        assert_eq!(telemetry, expected, "explicit and cached defaults have the same native value");
        *telemetry.get_mut("roll").unwrap() = 0.0;
        assert_eq!(telemetry.native_value("roll"), None, "checked numeric writes invalidate even an equal default");
        telemetry.game = "kept".into();
        telemetry.reset_to_off();
        assert_eq!(telemetry.native_value("roll"), Some(&LegacyValue::Int(0)));
        assert_eq!(telemetry.native_value("speed_kmh"), Some(&LegacyValue::Int(0)));
        assert_eq!(telemetry.game, "kept");
        let mut unfilled = Telemetry::unfilled();
        assert_eq!(unfilled.native_value("pitch"), None);
        unfilled.set_native("roll", LegacyValue::Int(5));
        unfilled.set_native("pitch", LegacyValue::Int(4));
        unfilled.reset_to_off();
        assert_eq!(unfilled.channel_names(), ["roll", "pitch"]);
        assert_eq!(unfilled.native_value("gear"), None);
        unfilled.roll = f64::NAN;
        unfilled.clear_nan();
        assert_eq!(unfilled.native_value("roll"), Some(&LegacyValue::Int(0)));
    }

    #[test]
    fn unfilled_snapshots_keep_presence_and_assignment_order() {
        let mut telemetry = Telemetry::unfilled();
        assert!(telemetry.channel_names().is_empty());
        assert_ne!(telemetry, Telemetry::off());
        telemetry.set_native("roll", LegacyValue::Int(1));
        telemetry.set_native("pitch", LegacyValue::Int(2));
        telemetry.set_native("roll", LegacyValue::Int(3));
        let snapshot = telemetry.clone();
        assert_eq!(snapshot.channel_names(), ["roll", "pitch"]);
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            json.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            ["game", "vehicle", "location", "roll", "pitch"]
        );
        assert!(json.get("yaw").is_none());
    }

    #[test]
    fn native_values_serialize_without_losing_their_python_type() {
        let mut telemetry = Telemetry::off();
        telemetry.set_native("pitch", LegacyValue::Str("bad".into()));
        telemetry.set_native("gear", LegacyValue::Float(3.0));
        telemetry.set_native("rpm", LegacyValue::Int(4000));
        telemetry.set_native("flag", LegacyValue::None);
        let json = serde_json::to_value(&telemetry).unwrap();
        assert_eq!(json["pitch"], "bad");
        assert!(json["gear"].as_number().unwrap().is_f64());
        assert!(json["rpm"].as_number().unwrap().is_i64());
        assert!(json["flag"].is_null());
        assert!(telemetry.pitch.is_nan());
        assert_eq!(telemetry.native_value("pitch"), Some(&LegacyValue::Str("bad".into())));
        telemetry.gear = 4.0;
        assert_eq!(telemetry.native_value("gear"), None);
        *telemetry.get_mut("rpm").unwrap() = 4000.0;
        assert_eq!(telemetry.native_value("rpm"), None, "numeric assignment clears even an equal-value override");
        telemetry.set_native("roll", LegacyValue::Float(f64::NAN));
        assert_eq!(telemetry.clear_nan(), ["roll"]);
        assert_eq!(telemetry.native_value("roll"), Some(&LegacyValue::Int(0)));
        assert_eq!(telemetry.native_value("pitch"), Some(&LegacyValue::Str("bad".into())));
    }

    #[test]
    fn registry_has_79_channels_and_off_values() {
        assert_eq!(Telemetry::CHANNELS.len(), 79);
        let t = Telemetry::off();
        assert_eq!(t.gear, 11.0);
        assert_eq!(t.max_rpm, 20000.0);
        assert_eq!(t.max_speed_kmh, 3.6);
        assert_eq!(t.steering_wheel_angle, 30.0);
        assert_eq!(t.get("rpm"), Some(0.0));
    }
}

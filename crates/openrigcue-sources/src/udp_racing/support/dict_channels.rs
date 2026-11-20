//! The string-keyed telemetry dict of the text listeners (Epic Roller Coasters, Red Rover): every key the
//! game sends is a channel of that name, its value a float or the text that is not a float. A
//! channel read answers the stored value, or 0 for a missing key.

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use super::channels::{
    auto_load_fallback, is_auto_load, is_listener_read, publish_speed_unreadable, zero_listener_channels,
};
use super::pymath::{float_repr, trunc};
use crate::udp_racing::unhandled::DispatcherEnds;

/// One value of the dict.
#[derive(Debug, Clone, PartialEq)]
pub enum DictValue {
    Integer(i64),
    Number(f64),
    Text(String),
    Native(LegacyValue),
}

/// The listener's dict, in insertion order (a repeated key keeps its first position).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TelemetryDict {
    entries: Vec<(String, DictValue)>,
}

impl TelemetryDict {
    /// A channel read: keep the dict value's type; a missing key answers integer 0.
    pub fn native_channel(&self, key: &str) -> LegacyValue {
        match self.get(key) {
            Some(DictValue::Integer(value)) => LegacyValue::Int(*value),
            Some(DictValue::Number(value)) => LegacyValue::Float(*value),
            Some(DictValue::Text(value)) => LegacyValue::Str(value.clone()),
            Some(DictValue::Native(value)) => value.clone(),
            None => LegacyValue::Int(0),
        }
    }
    /// Stores a value under a key.
    pub fn set(&mut self, key: &str, value: DictValue) {
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key.to_string(), value)),
        }
    }

    /// The value of a key, `None` for a missing key.
    pub fn get(&self, key: &str) -> Option<&DictValue> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The mutable value of a key.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut DictValue> {
        self.entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The value of a key as a number: `Some` only for a float value.
    pub fn number(&self, key: &str) -> Option<f64> {
        match self.get(key) {
            Some(DictValue::Integer(n)) => Some(*n as f64),
            Some(DictValue::Number(n)) => Some(*n),
            Some(DictValue::Native(value)) => value.number(),
            _ => None,
        }
    }

    /// A name channel (`car_name`, `track_loc`) as the dispatcher stores it: the text, the text of a float,
    /// empty for the 0 of a missing key.
    fn name(&self, key: &str) -> String {
        match self.get(key) {
            Some(DictValue::Integer(n)) => n.to_string(),
            Some(DictValue::Text(text)) => text.clone(),
            Some(DictValue::Number(n)) => float_repr(*n),
            Some(DictValue::Native(value)) => value.py_str().unwrap_or_default(),
            None => String::new(),
        }
    }

    /// Publish the dict as the dispatcher stores its channel answers.
    ///
    /// A channel read (the stored value, 0 for a missing key) followed by the dispatcher tick:
    /// a text, NaN or infinite `speed` has no integer value and ends the tick after `lateral_velocity`
    /// ([`publish_speed_unreadable`]); an auto-loaded channel holding text cannot be converted and gets
    /// [`auto_load_fallback`]. Text in `pitch`, `roll`, `yaw` or `lateral_velocity`
    /// is something a numeric channel cannot hold; the channel keeps its value.
    ///
    /// Returns the [`DispatcherEnds`] of a tick whose `speed` read fails.
    pub fn publish(&self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let previous = t.clone();
        let lateral_velocity = match self.get("lateral_velocity") {
            Some(DictValue::Integer(n)) => *n as f64,
            Some(DictValue::Number(n)) => *n,
            Some(DictValue::Text(_)) => previous.lateral_velocity,
            Some(DictValue::Native(value)) => value.number().unwrap_or(f64::NAN),
            None => 0.0,
        };
        let speed_readable = match self.get("speed") {
            Some(DictValue::Integer(_)) => true,
            Some(DictValue::Number(n)) => trunc(*n).is_some(),
            Some(DictValue::Text(_)) => false,
            Some(DictValue::Native(value)) => value.py_int().is_some(),
            None => true,
        };
        if !speed_readable {
            let ends = publish_speed_unreadable(t, Some(self.name("car_name")), lateral_velocity);
            t.location = self.name("track_loc");
            return ends;
        }
        zero_listener_channels(t);
        t.vehicle = self.name("car_name");
        t.location = self.name("track_loc");
        for (key, value) in self.entries.iter().filter(|(k, _)| is_listener_read(k)) {
            let (Some(slot), Some(before)) = (t.get_mut(key), previous.get(key)) else { continue };
            *slot = match value {
                DictValue::Integer(n) => *n as f64,
                DictValue::Number(n) => *n,
                DictValue::Text(_) if is_auto_load(key) => auto_load_fallback(key, before),
                DictValue::Text(_) => before,
                DictValue::Native(value) => value.number().unwrap_or(f64::NAN),
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: &[(&str, DictValue)]) -> TelemetryDict {
        let mut d = TelemetryDict::default();
        for (k, v) in entries {
            d.set(k, v.clone());
        }
        d
    }

    fn text(s: &str) -> DictValue {
        DictValue::Text(s.into())
    }

    #[test]
    fn missing_keys_read_zero_and_numbers_pass_through() {
        // A channel read: the stored value, 0 for a missing key
        let mut t = Telemetry { oil_temperature: 90.0, vehicle: "old".into(), ..Telemetry::off() };
        dict(&[("rpm", DictValue::Number(4000.0)), ("speed", DictValue::Number(12.7))]).publish(&mut t).unwrap();
        assert_eq!((t.rpm, t.speed, t.oil_temperature, t.gear), (4000.0, 12.7, 0.0, 0.0));
        assert!(t.vehicle.is_empty());
    }

    #[test]
    fn text_in_auto_load_channels_takes_the_loop_fallback() {
        // Auto-load: a text has no integer value -> default (raw_flag -1, fuel 0) or kept (default None)
        let mut t = Telemetry { fuel_level: 40.0, gear: 3.0, raw_flag: 5.0, pitch: 2.0, ..Telemetry::off() };
        dict(&[("fuel_level", text("abc")), ("gear", text("P")), ("raw_flag", text("x")), ("pitch", text("abc"))])
            .publish(&mut t)
            .unwrap();
        assert_eq!((t.fuel_level, t.gear, t.raw_flag), (0.0, 3.0, -1.0));
        assert_eq!(t.pitch, 2.0, "a str pitch is stored as is by the original; kept here");
    }

    #[test]
    fn unreadable_speed_stores_only_names_and_lateral_velocity() {
        // The top-speed update's speed read fails for text, NaN and inf
        for speed in [text("fast"), DictValue::Number(f64::NAN), DictValue::Number(f64::INFINITY)] {
            let mut t = Telemetry { rpm: 3000.0, speed: 9.0, pitch: 1.0, location: "old".into(), ..Telemetry::off() };
            let d = dict(&[
                ("speed", speed),
                ("rpm", DictValue::Number(5000.0)),
                ("lateral_velocity", DictValue::Number(0.5)),
                ("car_name", text("Rover")),
            ]);
            assert!(d.publish(&mut t).is_err(), "getMaxSpeed raises out of the dispatcher");
            assert_eq!((t.rpm, t.speed, t.pitch, t.lateral_velocity), (3000.0, 9.0, 1.0, 0.5));
            assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("Rover", ""));
        }
    }

    #[test]
    fn name_channels_are_text_or_float_str() {
        // The dispatcher stores the vehicle name and location from channel reads
        let mut t = Telemetry::off();
        dict(&[("car_name", text("Rover")), ("track_loc", DictValue::Number(5.0))]).publish(&mut t).unwrap();
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("Rover", "5.0"));
    }

    #[test]
    fn dispatcher_owned_keys_are_ignored() {
        // the dispatcher computes yaw_angle / max_speed itself and never reads them from the listener
        let mut t = Telemetry { yaw_angle: 7.0, ..Telemetry::off() };
        dict(&[("yaw_angle", DictValue::Number(1.0)), ("max_speed", DictValue::Number(99.0))]).publish(&mut t).unwrap();
        assert_eq!((t.yaw_angle, t.max_speed), (7.0, Telemetry::off().max_speed));
    }
}

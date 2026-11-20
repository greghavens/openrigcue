//! Condor 2 / Condor 3: the simulator's UDP text output, `key=value` lines separated by CR LF.
//!
//! Angles and rates arrive in radians and radians per second.

use std::collections::HashMap;
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use super::channels::{assign, AngularRates};
use super::python::{parse_float, rad_to_deg};
use super::transport::{Decoder, PolledUdpSource, RateDecoder};

/// The Condor listener (`condor`, port 55278).
pub type CondorSource = PolledUdpSource<CondorDecoder>;

/// The only key that is not preceded by the `\n` of the previous line (it opens the datagram).
const FIRST_KEY: &str = "time";

/// Channel values last stored by the receive step (`None` until a datagram set them).
#[derive(Debug, Default)]
pub struct CondorDecoder {
    yaw: Option<f64>,
    roll: Option<f64>,
    pitch: Option<f64>,
    roll_rate: Option<f64>,
    pitch_rate: Option<f64>,
    yaw_rate: Option<f64>,
    vertical_acceleration: Option<f64>,
    lateral_acceleration: Option<f64>,
    longitudinal_acceleration: Option<f64>,
    lateral_velocity: Option<f64>,
    longitudinal_velocity: Option<f64>,
    vertical_velocity: Option<f64>,
    speed: Option<f64>,
    steering_wheel_angle: Option<f64>,
}

/// How one channel is derived from one Condor value.
#[derive(Clone, Copy)]
enum Conversion {
    Raw,
    Degrees,
    NegatedDegrees,
}

impl Conversion {
    fn apply(self, value: f64) -> f64 {
        match self {
            Conversion::Raw => value,
            Conversion::Degrees => rad_to_deg(value),
            Conversion::NegatedDegrees => -rad_to_deg(value),
        }
    }
}

impl CondorDecoder {
    /// Channels in storing order, with their Condor key and conversion.
    fn assignments(&mut self) -> [(&mut Option<f64>, &'static str, Conversion); 14] {
        use Conversion::*;
        [
            (&mut self.yaw, "yaw", Degrees),
            (&mut self.roll, "bank", NegatedDegrees),
            (&mut self.pitch, "pitch", Degrees),
            (&mut self.roll_rate, "rollrate", NegatedDegrees),
            (&mut self.pitch_rate, "pitchrate", Degrees),
            (&mut self.yaw_rate, "yawrate", Degrees),
            (&mut self.vertical_acceleration, "az", Raw),
            (&mut self.lateral_acceleration, "ay", Raw),
            (&mut self.longitudinal_acceleration, "ax", Raw),
            (&mut self.lateral_velocity, "vy", Raw),
            (&mut self.longitudinal_velocity, "vx", Raw),
            (&mut self.vertical_velocity, "vz", Raw),
            (&mut self.speed, "airspeed", Raw),
            (&mut self.steering_wheel_angle, "ay", Raw),
        ]
    }
}

/// Parses the `key=value` pieces between `\r` separators. A piece without a value after its first `=`, or whose
/// value is not a number, is skipped; keys other than `time` lose
/// their first character (the `\n` of the line break).
fn parse_values(text: &str) -> HashMap<String, f64> {
    text.split('\r').filter_map(parse_piece).collect()
}

/// One `key=value` piece, or `None` when it has no numeric value after its first `=`.
fn parse_piece(piece: &str) -> Option<(String, f64)> {
    let mut parts = piece.split('=');
    let key = parts.next().unwrap_or_default();
    let value = parse_float(parts.next()?)?;
    let key = if key == FIRST_KEY { key.to_owned() } else { key.chars().skip(1).collect() };
    Some((key, value))
}

impl Decoder for CondorDecoder {
    const ID: &'static str = "condor";
    const DEFAULT_PORT: u16 = 55278;
    // A receive timeout of 1/15 s: binary64 0x3fb1111111111111, rounded up to 66,666,667 ns.
    const TIMEOUT: Duration = crate::udp_racing::net::socket_timeout(66_666_667);
    const MAX_DATAGRAM: usize = 1024;

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = match name {
            "yaw" => self.yaw,
            "roll" => self.roll,
            "pitch" => self.pitch,
            "roll_rate" => self.roll_rate,
            "pitch_rate" => self.pitch_rate,
            "yaw_rate" => self.yaw_rate,
            "vertical_acceleration" => self.vertical_acceleration,
            "lateral_acceleration" => self.lateral_acceleration,
            "longitudinal_acceleration" => self.longitudinal_acceleration,
            "lateral_velocity" => self.lateral_velocity,
            "longitudinal_velocity" => self.longitudinal_velocity,
            "vertical_velocity" => self.vertical_velocity,
            "speed" => self.speed,
            "steering_wheel_angle" => self.steering_wheel_angle,
            _ => None,
        };
        Some(value.map_or(LegacyValue::Int(0), LegacyValue::Float))
    }

    /// The receive step: the channels are stored in order and a missing key stops the
    /// sequence, leaving the channels before it updated.
    fn decode(&mut self, datagram: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(datagram) else {
            return false;
        };
        let values = parse_values(text);
        for (slot, key, conversion) in self.assignments() {
            let Some(&value) = values.get(key) else {
                return false;
            };
            *slot = Some(conversion.apply(value));
        }
        true
    }

    /// Writes the stored channels (0 when absent).
    fn write(&self, t: &mut Telemetry) {
        assign(&mut t.yaw, self.yaw);
        assign(&mut t.roll, self.roll);
        assign(&mut t.pitch, self.pitch);
        assign(&mut t.vertical_acceleration, self.vertical_acceleration);
        assign(&mut t.lateral_acceleration, self.lateral_acceleration);
        assign(&mut t.longitudinal_acceleration, self.longitudinal_acceleration);
        assign(&mut t.lateral_velocity, self.lateral_velocity);
        assign(&mut t.longitudinal_velocity, self.longitudinal_velocity);
        assign(&mut t.vertical_velocity, self.vertical_velocity);
        assign(&mut t.speed, self.speed);
        assign(&mut t.steering_wheel_angle, self.steering_wheel_angle);
    }
}

impl RateDecoder for CondorDecoder {
    /// The `pitch_rate` / `roll_rate` / `yaw_rate` channels.
    fn rates(&self) -> AngularRates {
        AngularRates {
            pitch: self.pitch_rate.unwrap_or(0.0),
            roll: self.roll_rate.unwrap_or(0.0),
            yaw: self.yaw_rate.unwrap_or(0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_flight::channels::clear_listener_channels;

    const FULL: &str = "time=12.5\r\nairspeed=30.5\r\nax=0.1\r\nay=-0.2\r\naz=9.8\r\nvx=30\r\nvy=1\r\nvz=-2\r\n\
                        yaw=1\r\npitch=0.5\r\nbank=-0.25\r\nrollrate=0.1\r\npitchrate=0.2\r\nyawrate=0.3";

    fn view(decoder: &CondorDecoder) -> Telemetry {
        let mut t = Telemetry::off();
        clear_listener_channels(&mut t);
        decoder.write(&mut t);
        t
    }

    #[test]
    fn native_getters_distinguish_stored_float_zero_from_the_missing_integer_default() {
        use crate::derive::{Listener, SourceListener};
        use crate::udp_flight::UdpConfig;
        use crate::Source;
        use openrigcue_model::native_value::LegacyValue::{Float, Int};
        let mut decoder = CondorDecoder::default();
        assert!(!decoder.decode(b"time=1\r\nyaw=0"));
        let source = CondorSource::new(decoder, UdpConfig::port(0));
        assert!(!source.channel_read_raised("roll"), "getChannel catches the missing key and returns zero");
        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.native_channel("yaw"), Some(Float(0.0)));
        assert_eq!(listener.native_channel("roll"), Some(Int(0)));
        assert_eq!(listener.native_channel("speed"), Some(Int(0)));
        assert_eq!(listener.native_channel("rpm"), Some(Int(0)));
        assert_eq!(listener.native_channel("pitch_rate"), Some(Int(0)));
    }

    #[test]
    fn native_getters_keep_float_channels_and_the_partial_decode_prefix() {
        use crate::udp_flight::UdpConfig;
        use crate::Source;
        use openrigcue_model::native_value::LegacyValue::{Float, Int};
        let mut decoder = CondorDecoder::default();
        assert!(decoder.decode(FULL.as_bytes()));
        assert!(!decoder.decode(b"time=2\r\nyaw=0\r\nbank=0"));
        let source = CondorSource::new(decoder, UdpConfig::port(0));
        for (name, expected) in [
            ("yaw", Float(0.0)),
            ("roll", Float(-0.0)),
            ("pitch", Float(rad_to_deg(0.5))),
            ("roll_rate", Float(-rad_to_deg(0.1))),
            ("speed", Float(30.5)),
            ("steering_wheel_angle", Float(-0.2)),
            ("max_rpm", Int(0)),
        ] {
            assert_eq!(source.native_channel(name), Some(expected), "{name}");
        }
    }

    #[test]
    fn receive_data_converts_and_maps_condor_keys() {
        let mut d = CondorDecoder::default();
        assert!(d.decode(FULL.as_bytes()));
        let t = view(&d);
        assert_eq!(t.yaw, rad_to_deg(1.0));
        assert_eq!(t.pitch, rad_to_deg(0.5));
        assert_eq!(t.roll, -rad_to_deg(-0.25));
        assert_eq!((t.vertical_acceleration, t.lateral_acceleration, t.longitudinal_acceleration), (9.8, -0.2, 0.1));
        assert_eq!((t.lateral_velocity, t.longitudinal_velocity, t.vertical_velocity), (1.0, 30.0, -2.0));
        assert_eq!(t.speed, 30.5);
        assert_eq!(t.steering_wheel_angle, -0.2);
        let rates = d.rates();
        assert_eq!(rates.roll, -rad_to_deg(0.1));
        assert_eq!((rates.pitch, rates.yaw), (rad_to_deg(0.2), rad_to_deg(0.3)));
    }

    #[test]
    fn receive_data_skips_pieces_without_a_numeric_value() {
        let mut full = CondorDecoder::default();
        assert!(full.decode(FULL.as_bytes()));
        let mut d = CondorDecoder::default();
        assert!(d.decode(format!("{FULL}\r\n").as_bytes()), "the last piece \"\\n\" has no '=' and is skipped");
        assert_eq!(view(&d), view(&full));
        let mut d = CondorDecoder::default();
        assert!(d.decode(FULL.replacen("\r\nairspeed", "\r\nbroken\r\nbad=x\r\nairspeed", 1).as_bytes()));
        assert_eq!(view(&d), view(&full), "malformed middle pieces are skipped");
    }

    #[test]
    fn receive_data_missing_key_stops_after_earlier_channels() {
        let mut d = CondorDecoder::default();
        assert!(!d.decode(b"time=1\r\nyaw=1\r\nbank=1"));
        let t = view(&d);
        assert_eq!(t.yaw, rad_to_deg(1.0));
        assert_eq!(t.roll, -rad_to_deg(1.0));
        assert_eq!(t.pitch, 0.0);
    }

    #[test]
    fn keys_other_than_time_lose_their_first_character() {
        let values = parse_values("time=1\r\nax=2\r\nXay=3");
        assert_eq!(values.get("time"), Some(&1.0));
        assert_eq!(values.get("ax"), Some(&2.0));
        assert_eq!(values.get("Xay"), Some(&3.0), "only the line-break character is dropped");
        assert_eq!(parse_values("Xtime=4").get("time"), Some(&4.0));
    }
}

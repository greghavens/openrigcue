//! Aerofly FS 2: eight `;`-separated integers sent by the SRS game plugin DLL.
//!
//! The plugin sends pitch, bank, turn rate and three angular velocities multiplied by 1000, then unscaled
//! airspeed and gear. The listener maps the first six to the channels below and divides them by 10.
//! The plugin hard-codes `127.0.0.1:4123`.

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::channels::{assign, AngularRates};
use super::python::parse_float;
use super::transport::{Decoder, PolledUdpSource, RateDecoder};

/// The Aerofly FS 2 listener (`aeroflyfs2`, port 4123).
pub type AeroflySource = PolledUdpSource<AeroflyDecoder>;

/// Channels in datagram order.
#[derive(Debug, Default)]
pub struct AeroflyDecoder {
    pitch: Option<f64>,
    roll: Option<f64>,
    yaw: Option<f64>,
    lateral_acceleration: Option<f64>,
    longitudinal_acceleration: Option<f64>,
    vertical_acceleration: Option<f64>,
    speed: Option<f64>,
    gear: Option<f64>,
}

impl AeroflyDecoder {
    /// Fields in datagram order, with whether the listener divides them by ten.
    fn fields_mut(&mut self) -> [(&mut Option<f64>, bool); 8] {
        [
            (&mut self.pitch, true),
            (&mut self.roll, true),
            (&mut self.yaw, true),
            (&mut self.lateral_acceleration, true),
            (&mut self.longitudinal_acceleration, true),
            (&mut self.vertical_acceleration, true),
            (&mut self.speed, false),
            (&mut self.gear, false),
        ]
    }
}

impl Decoder for AeroflyDecoder {
    const ID: &'static str = "aeroflyfs2";
    const DEFAULT_PORT: u16 = 4123;
    const TIMEOUT: Duration = Duration::from_secs(1);
    const MAX_DATAGRAM: usize = 2000;

    /// The receive step: fields are stored one by one, so a short or malformed
    /// datagram updates the fields before the failing one.
    fn decode(&mut self, datagram: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(datagram) else {
            return false;
        };
        let mut values = text.split(';');
        for (slot, scaled_by_ten) in self.fields_mut() {
            let Some(value) = values.next().and_then(parse_float) else {
                return false;
            };
            *slot = Some(if scaled_by_ten { value / 10.0 } else { value });
        }
        true
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = match name {
            "pitch" => self.pitch,
            "roll" => self.roll,
            "yaw" => self.yaw,
            "lateral_acceleration" => self.lateral_acceleration,
            "longitudinal_acceleration" => self.longitudinal_acceleration,
            "vertical_acceleration" => self.vertical_acceleration,
            "speed" => self.speed,
            "gear" => self.gear,
            _ => None,
        };
        Some(value.map_or(LegacyValue::Int(0), LegacyValue::Float))
    }

    /// Numeric projection of the stored channels (0 when absent).
    fn write(&self, t: &mut Telemetry) {
        assign(&mut t.pitch, self.pitch);
        assign(&mut t.roll, self.roll);
        assign(&mut t.yaw, self.yaw);
        assign(&mut t.lateral_acceleration, self.lateral_acceleration);
        assign(&mut t.longitudinal_acceleration, self.longitudinal_acceleration);
        assign(&mut t.vertical_acceleration, self.vertical_acceleration);
        assign(&mut t.speed, self.speed);
        assign(&mut t.gear, self.gear);
    }
}

/// The listener provides no rate channels; they read 0.
impl RateDecoder for AeroflyDecoder {
    /// The `pitch_rate` / `roll_rate` / `yaw_rate` channels.
    fn rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_flight::channels::clear_listener_channels;

    fn view(decoder: &AeroflyDecoder) -> Telemetry {
        let mut t = Telemetry::off();
        clear_listener_channels(&mut t);
        decoder.write(&mut t);
        t
    }

    #[test]
    fn receive_data_divides_the_first_six_fields_by_ten() {
        let mut d = AeroflyDecoder::default();
        assert!(d.decode(b"15;-30;1800;5;-7;12;63;2"));
        let t = view(&d);
        assert_eq!((t.pitch, t.roll, t.yaw), (1.5, -3.0, 180.0));
        assert_eq!((t.lateral_acceleration, t.longitudinal_acceleration, t.vertical_acceleration), (0.5, -0.7, 1.2));
        assert_eq!((t.speed, t.gear), (63.0, 2.0));
    }

    #[test]
    fn receive_data_stops_at_the_first_bad_field() {
        let mut d = AeroflyDecoder::default();
        assert!(!d.decode(b"0;x"));
        assert_eq!(d.native_channel("pitch"), Some(LegacyValue::Float(0.0)));
        assert_eq!(d.native_channel("roll"), Some(LegacyValue::Int(0)));
        d.decode(b"10;10;10;10;10;10;10;1");
        assert!(!d.decode(b"20;20;x;20;20;20;20;2"));
        let t = view(&d);
        assert_eq!((t.pitch, t.roll, t.yaw), (2.0, 2.0, 1.0));
        assert_eq!(t.gear, 1.0);
        assert_eq!(d.native_channel("pitch"), Some(LegacyValue::Float(2.0)));
        assert_eq!(d.native_channel("gear"), Some(LegacyValue::Float(1.0)));
        assert_eq!(d.native_channel("roll_rate"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn get_channel_is_zero_before_any_data() {
        let t = view(&AeroflyDecoder::default());
        assert_eq!((t.pitch, t.speed, t.gear), (0.0, 0.0, 0.0));
    }
}

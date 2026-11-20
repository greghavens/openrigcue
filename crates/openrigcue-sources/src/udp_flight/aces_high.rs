//! Aces High III: the game's "Output Flight Data" text, comma-separated.
//!
//! Fields 0–5 are not used; fields 6–17 are velocities, accelerations, attitude and rates as the game sends them.

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::channels::{assign, AngularRates};
use super::python::parse_float;
use super::transport::{Decoder, PolledUdpSource, ProbeStage, RateDecoder};

/// The Aces High III listener (`aceshigh`, port 556).
pub type AcesHighSource = PolledUdpSource<AcesHighDecoder>;

/// Index of the first field the listener reads.
const FIRST_FIELD: usize = 6;

/// Channels in datagram order starting at field 6, then the derived speed.
#[derive(Debug, Default)]
pub struct AcesHighDecoder {
    lateral_velocity: Option<f64>,
    vertical_velocity: Option<f64>,
    longitudinal_velocity: Option<f64>,
    longitudinal_acceleration: Option<f64>,
    vertical_acceleration: Option<f64>,
    lateral_acceleration: Option<f64>,
    roll: Option<f64>,
    pitch: Option<f64>,
    yaw: Option<f64>,
    roll_rate: Option<f64>,
    pitch_rate: Option<f64>,
    yaw_rate: Option<f64>,
    speed: Option<f64>,
}

impl AcesHighDecoder {
    fn fields_mut(&mut self) -> [&mut Option<f64>; 12] {
        [
            &mut self.lateral_velocity,
            &mut self.vertical_velocity,
            &mut self.longitudinal_velocity,
            &mut self.longitudinal_acceleration,
            &mut self.vertical_acceleration,
            &mut self.lateral_acceleration,
            &mut self.roll,
            &mut self.pitch,
            &mut self.yaw,
            &mut self.roll_rate,
            &mut self.pitch_rate,
            &mut self.yaw_rate,
        ]
    }

    /// `sqrt(lat² + vert² + long²) * 0.036 / 3.6`: the velocities are taken as cm/s and
    /// published in m/s. `None` where a square overflows (a finite value whose square is not finite).
    fn speed_from(velocities: [f64; 3]) -> Option<f64> {
        let mut sum = 0.0;
        for velocity in velocities {
            let square = velocity.powf(2.0);
            if square.is_infinite() && velocity.is_finite() {
                return None;
            }
            sum += square;
        }
        Some(sum.sqrt() * 0.036 / 3.6)
    }
}

impl Decoder for AcesHighDecoder {
    const ID: &'static str = "aceshigh";
    const DEFAULT_PORT: u16 = 556;
    const TIMEOUT: Duration = Duration::from_secs(1);
    const MAX_DATAGRAM: usize = 320;

    fn on_probe_failure(&mut self, stage: ProbeStage, error: &std::io::Error) -> Option<String> {
        (stage == ProbeStage::Creation).then(|| format!("Exception1 {}", crate::printed::os_error(error)))
    }

    /// The receive step: fields are stored one by one (a short or malformed datagram
    /// updates those before the failing one), then speed is derived from the three velocities.
    fn decode(&mut self, datagram: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(datagram) else {
            return false;
        };
        let mut values = text.split(',').skip(FIRST_FIELD);
        let mut stored = [0.0; 12];
        for (slot, value_out) in self.fields_mut().into_iter().zip(&mut stored) {
            let Some(value) = values.next().and_then(parse_float) else {
                return false;
            };
            *slot = Some(value);
            *value_out = value;
        }
        let Some(speed) = Self::speed_from([stored[0], stored[1], stored[2]]) else {
            return false;
        };
        self.speed = Some(speed);
        true
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = match name {
            "lateral_velocity" => self.lateral_velocity,
            "vertical_velocity" => self.vertical_velocity,
            "longitudinal_velocity" => self.longitudinal_velocity,
            "longitudinal_acceleration" => self.longitudinal_acceleration,
            "vertical_acceleration" => self.vertical_acceleration,
            "lateral_acceleration" => self.lateral_acceleration,
            "roll" => self.roll,
            "pitch" => self.pitch,
            "yaw" => self.yaw,
            "roll_rate" => self.roll_rate,
            "pitch_rate" => self.pitch_rate,
            "yaw_rate" => self.yaw_rate,
            "speed" => self.speed,
            _ => None,
        };
        Some(value.map_or(LegacyValue::Int(0), LegacyValue::Float))
    }

    /// Numeric projection of the stored channels (0 when absent).
    fn write(&self, t: &mut Telemetry) {
        assign(&mut t.lateral_velocity, self.lateral_velocity);
        assign(&mut t.vertical_velocity, self.vertical_velocity);
        assign(&mut t.longitudinal_velocity, self.longitudinal_velocity);
        assign(&mut t.longitudinal_acceleration, self.longitudinal_acceleration);
        assign(&mut t.vertical_acceleration, self.vertical_acceleration);
        assign(&mut t.lateral_acceleration, self.lateral_acceleration);
        assign(&mut t.roll, self.roll);
        assign(&mut t.pitch, self.pitch);
        assign(&mut t.yaw, self.yaw);
        assign(&mut t.speed, self.speed);
    }
}

impl RateDecoder for AcesHighDecoder {
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

    fn view(decoder: &AcesHighDecoder) -> Telemetry {
        let mut t = Telemetry::off();
        clear_listener_channels(&mut t);
        decoder.write(&mut t);
        t
    }

    #[test]
    fn receive_data_reads_fields_6_to_17_and_derives_speed() {
        let mut d = AcesHighDecoder::default();
        assert!(d.decode(b"a,b,c,d,e,f,300,0,400,0.5,1.5,-0.2,10,5,270,1,2,3"));
        let t = view(&d);
        assert_eq!((t.lateral_velocity, t.vertical_velocity, t.longitudinal_velocity), (300.0, 0.0, 400.0));
        assert_eq!((t.longitudinal_acceleration, t.vertical_acceleration, t.lateral_acceleration), (0.5, 1.5, -0.2));
        assert_eq!((t.roll, t.pitch, t.yaw), (10.0, 5.0, 270.0));
        assert_eq!(t.speed, 500.0 * 0.036 / 3.6);
        let rates = d.rates();
        assert_eq!((rates.roll, rates.pitch, rates.yaw), (1.0, 2.0, 3.0));
    }

    #[test]
    fn receive_data_short_datagram_updates_leading_fields_only() {
        let mut d = AcesHighDecoder::default();
        assert!(!d.decode(b"0,0,0,0,0,0,7,8"));
        let t = view(&d);
        assert_eq!((t.lateral_velocity, t.vertical_velocity), (7.0, 8.0));
        assert_eq!(t.speed, 0.0, "speed is only derived after all twelve fields");
        assert_eq!(d.native_channel("lateral_velocity"), Some(LegacyValue::Float(7.0)));
        assert_eq!(d.native_channel("vertical_velocity"), Some(LegacyValue::Float(8.0)));
        assert_eq!(d.native_channel("speed"), Some(LegacyValue::Int(0)));
        assert_eq!(d.native_channel("pitch_rate"), Some(LegacyValue::Int(0)));
        d.decode(b"0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0");
        assert!(!d.decode(b"0,0,0,0,0,0,7,8"));
        assert_eq!(d.native_channel("pitch_rate"), Some(LegacyValue::Float(0.0)));
        assert_eq!(d.native_channel("speed"), Some(LegacyValue::Float(0.0)));
        assert_eq!(d.native_channel("rpm"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn receive_data_speed_overflow_keeps_previous_speed() {
        let mut d = AcesHighDecoder::default();
        d.decode(b"0,0,0,0,0,0,100,0,0,0,0,0,0,0,0,0,0,0");
        assert!(!d.decode(b"0,0,0,0,0,0,1e200,0,0,0,0,0,0,0,0,0,0,0"));
        let t = view(&d);
        assert_eq!(t.lateral_velocity, 1e200);
        assert_eq!(t.speed, 100.0 * 0.036 / 3.6);
    }
}

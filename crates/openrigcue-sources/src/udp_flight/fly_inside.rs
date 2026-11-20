//! FlyInside Flight Simulator: comma-separated numbers sent by a FlyInside startup script.
//!
//! Fields: `0` speed, `1` roll, `2` pitch (both sign-inverted on receipt), `3` yaw, `4` longitudinal, `5` lateral and
//! `6` vertical acceleration.

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::channels::{assign, AngularRates};
use super::python::parse_float;
use super::transport::{Decoder, PolledUdpSource, ProbeStage, RateDecoder};

/// The FlyInside listener (`flyinside`, port 31234).
pub type FlyInsideSource = PolledUdpSource<FlyInsideDecoder>;

/// Channel values last stored by the receive step (`None` until a datagram set them).
#[derive(Debug, Default)]
pub struct FlyInsideDecoder {
    speed: Option<f64>,
    roll: Option<f64>,
    pitch: Option<f64>,
    yaw: Option<f64>,
    longitudinal_acceleration: Option<f64>,
    lateral_acceleration: Option<f64>,
    vertical_acceleration: Option<f64>,
}

impl FlyInsideDecoder {
    /// Fields in datagram order with the sign applied on receipt.
    fn fields_mut(&mut self) -> [(&mut Option<f64>, f64); 7] {
        [
            (&mut self.speed, 1.0),
            (&mut self.roll, -1.0),
            (&mut self.pitch, -1.0),
            (&mut self.yaw, 1.0),
            (&mut self.longitudinal_acceleration, 1.0),
            (&mut self.lateral_acceleration, 1.0),
            (&mut self.vertical_acceleration, 1.0),
        ]
    }
}

impl Decoder for FlyInsideDecoder {
    const ID: &'static str = "flyinside";
    const DEFAULT_PORT: u16 = 31234;
    // A receive timeout of 1/3 s: binary64 0x3fd5555555555555, rounded up to 333,333,334 ns.
    const TIMEOUT: Duration = crate::udp_racing::net::socket_timeout(333_333_334);
    const MAX_DATAGRAM: usize = 1024;

    fn on_probe_failure(&mut self, stage: ProbeStage, _error: &std::io::Error) -> Option<String> {
        (stage == ProbeStage::Bind).then(|| "sw fail sock".to_owned())
    }

    /// The receive step: every field must be a number (else nothing changes); then the
    /// seven channels are stored in order, a shorter datagram updating the leading ones only.
    fn decode(&mut self, datagram: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(datagram) else {
            return false;
        };
        let Some(values) = text.split(',').map(parse_float).collect::<Option<Vec<f64>>>() else {
            return false;
        };
        let mut values = values.into_iter();
        for (slot, sign) in self.fields_mut() {
            let Some(value) = values.next() else {
                return false;
            };
            *slot = Some(value * sign);
        }
        true
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = match name {
            "speed" => self.speed,
            "roll" => self.roll,
            "pitch" => self.pitch,
            "yaw" => self.yaw,
            "longitudinal_acceleration" => self.longitudinal_acceleration,
            "lateral_acceleration" => self.lateral_acceleration,
            "vertical_acceleration" => self.vertical_acceleration,
            _ => None,
        };
        Some(value.map_or(LegacyValue::Int(0), LegacyValue::Float))
    }

    /// Numeric projection of the stored channels (0 when absent).
    fn write(&self, t: &mut Telemetry) {
        assign(&mut t.speed, self.speed);
        assign(&mut t.roll, self.roll);
        assign(&mut t.pitch, self.pitch);
        assign(&mut t.yaw, self.yaw);
        assign(&mut t.longitudinal_acceleration, self.longitudinal_acceleration);
        assign(&mut t.lateral_acceleration, self.lateral_acceleration);
        assign(&mut t.vertical_acceleration, self.vertical_acceleration);
    }
}

/// The listener provides no rate channels; they read 0.
impl RateDecoder for FlyInsideDecoder {
    /// The `pitch_rate` / `roll_rate` / `yaw_rate` channels.
    fn rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_flight::channels::clear_listener_channels;

    fn view(decoder: &FlyInsideDecoder) -> Telemetry {
        let mut t = Telemetry::off();
        clear_listener_channels(&mut t);
        decoder.write(&mut t);
        t
    }

    #[test]
    fn receive_data_inverts_roll_and_pitch() {
        let mut d = FlyInsideDecoder::default();
        assert!(d.decode(b"55.5,10,-4,180,0.1,0.2,0.3"));
        let t = view(&d);
        assert_eq!((t.speed, t.roll, t.pitch, t.yaw), (55.5, -10.0, 4.0, 180.0));
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration, t.vertical_acceleration), (0.1, 0.2, 0.3));
    }

    #[test]
    fn receive_data_rejects_any_non_number_before_storing() {
        let mut d = FlyInsideDecoder::default();
        d.decode(b"1,2,3,4,5,6,7");
        assert!(!d.decode(b"9,9,9,9,9,9,x"));
        assert_eq!(view(&d).speed, 1.0);
    }

    #[test]
    fn receive_data_short_datagram_updates_leading_channels() {
        let mut d = FlyInsideDecoder::default();
        assert!(!d.decode(b"20,1"));
        let t = view(&d);
        assert_eq!((t.speed, t.roll, t.pitch), (20.0, -1.0, 0.0));
        assert_eq!(d.native_channel("roll"), Some(LegacyValue::Float(-1.0)));
        assert_eq!(d.native_channel("pitch"), Some(LegacyValue::Int(0)));
        assert!(!d.decode(b"0,x"));
        assert_eq!(d.native_channel("speed"), Some(LegacyValue::Float(20.0)));
        assert!(!d.decode(b"0,0"));
        assert!(matches!(d.native_channel("roll"), Some(LegacyValue::Float(v)) if v == 0.0 && v.is_sign_negative()));
        assert_eq!(d.native_channel("yaw_rate"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn check_status_timeout_is_a_third_of_a_second() {
        assert_eq!(FlyInsideDecoder::TIMEOUT, crate::udp_racing::net::timeout_secs(1.0 / 3.0));
        assert_eq!(FlyInsideDecoder::TIMEOUT, Duration::from_micros(if cfg!(windows) { 333_334 } else { 334_000 }));
    }
}

//! DCS World: comma-separated text sent by the export hook every simulation frame.
//!
//! Datagram fields (see the `SRSExport.lua` hook the game configuration installs): `0` protocol, `1` rpm, `2` true airspeed (m/s),
//! `3..5` acceleration x/y/z (G), `6..8` pitch/bank/yaw (rad), `9..11` angular velocity z/x/y (rad/s),
//! `12` aircraft name, `13` canopy, `14` landing gear, `15` cannon shells.

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::channels::{assign, AngularRates};
use super::python::{fold_roll, parse_float, rad_to_deg, truncate};
use super::transport::{Decoder, PolledUdpSource, ProbeStage, RateDecoder};

/// The DCS World listener (`dcs`, port 9800).
pub type DcsSource = PolledUdpSource<DcsDecoder>;

/// Aircraft whose gear channel ignores deployment below 40 % (the `landing_gear_*` channels).
const GEAR_THRESHOLD_AIRCRAFT: &str = "SpitfireLFMkIX";
const GEAR_THRESHOLD: f64 = 0.4;

/// Last datagram split into its text fields.
#[derive(Debug, Default)]
pub struct DcsDecoder {
    fields: Vec<String>,
}

impl DcsDecoder {
    fn number(&self, index: usize) -> Option<f64> {
        self.fields.get(index).and_then(|field| parse_float(field))
    }

    fn degrees(&self, index: usize) -> Option<f64> {
        self.number(index).map(rad_to_deg)
    }

    /// The `landing_gear_*` channels.
    fn landing_gear(&self) -> Option<f64> {
        let deployed = self.number(14)?;
        let aircraft = self.fields.get(12)?;
        Some(if aircraft == GEAR_THRESHOLD_AIRCRAFT && deployed < GEAR_THRESHOLD { 0.0 } else { deployed })
    }
}

impl Decoder for DcsDecoder {
    const ID: &'static str = "dcs";
    const DEFAULT_PORT: u16 = 9800;
    const TIMEOUT: Duration = Duration::from_secs(1);
    const MAX_DATAGRAM: usize = 8192;
    const EMPTY_DATAGRAM_IS_RUNNING: bool = true;

    fn on_probe_failure(&mut self, stage: ProbeStage, error: &std::io::Error) -> Option<String> {
        match stage {
            ProbeStage::Creation => {
                tracing::error!(target: "App.backend.listeners.dcsListener.checkStatus", "socket error {error}")
            }
            ProbeStage::Bind => {
                tracing::error!(target: "App.backend.listeners.dcsListener.checkStatus", "socket bind {error}")
            }
            ProbeStage::Receive => {}
        }
        None
    }

    /// The receive step: UTF-8 text split at `,`.
    fn decode(&mut self, datagram: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(datagram) else {
            return false;
        };
        self.fields = text.split(',').map(str::to_owned).collect();
        true
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};

        let value = match name {
            "rpm" => {
                // Truncate the field to an integer first, then multiply by 100: the integer is multiplied, not the float.
                return Some(self.number(1).and_then(crate::udp_racing::int_text).map_or(Int(0), |mut text| {
                    if text != "0" {
                        text.push_str("00");
                    }
                    Str(text)
                }));
            }
            "max_rpm" => return Some(Str("10000".into())),
            "gear" => return Some(Str("0".into())),
            "speed" => {
                return Some(Str(self.number(2).and_then(crate::udp_racing::int_text).unwrap_or_else(|| "0".into())));
            }
            "car_name" => return Some(self.fields.get(12).cloned().map_or(Int(0), Str)),
            "lateral_acceleration" => self.number(5).map(|g| g * 100.0),
            "vertical_acceleration" => self.number(4),
            "longitudinal_acceleration" => self.number(3).map(|g| g * 10.0),
            "pitch" => self.degrees(6),
            "roll" => self.degrees(7).map(fold_roll),
            "yaw" => self.degrees(8),
            "pitch_rate" => self.degrees(9),
            "roll_rate" => self.degrees(10),
            "yaw_rate" => self.degrees(11),
            "canopy" => self.number(13),
            "cannon_firing" => self.number(15),
            _ if name.starts_with("landing_gear_") => self.landing_gear(),
            _ => None,
        };
        // Each numeric channel handles its own conversion/index failure. Once field 14
        // exists, the aircraft field at index 12 also necessarily exists.
        Some(value.map_or(Int(0), Float))
    }

    /// Writes the channels.
    fn write(&self, t: &mut Telemetry) {
        assign(&mut t.rpm, self.number(1).and_then(truncate).map(|rpm| rpm * 100.0));
        assign(&mut t.lateral_acceleration, self.number(5).map(|g| g * 100.0));
        assign(&mut t.vertical_acceleration, self.number(4));
        assign(&mut t.longitudinal_acceleration, self.number(3).map(|g| g * 10.0));
        assign(&mut t.pitch, self.degrees(6));
        assign(&mut t.roll, self.degrees(7).map(fold_roll));
        assign(&mut t.yaw, self.degrees(8));
        assign(&mut t.speed, self.number(2).and_then(truncate));
        assign(&mut t.canopy, self.number(13));
        let gear = self.landing_gear();
        assign(&mut t.landing_gear_left, gear);
        assign(&mut t.landing_gear_nose, gear);
        assign(&mut t.landing_gear_right, gear);
        t.max_rpm = 10000.0;
        if let Some(aircraft) = self.fields.get(12) {
            t.vehicle.clone_from(aircraft);
        }
    }
}

impl RateDecoder for DcsDecoder {
    /// The `pitch_rate` / `roll_rate` / `yaw_rate` channels.
    fn rates(&self) -> AngularRates {
        AngularRates {
            pitch: self.degrees(9).unwrap_or(0.0),
            roll: self.degrees(10).unwrap_or(0.0),
            yaw: self.degrees(11).unwrap_or(0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;
    use crate::udp_flight::channels::clear_listener_channels;

    /// A datagram as the hook formats it.
    fn frame(aircraft: &str, rpm: f64, gear: f64, bank: f64) -> Vec<u8> {
        format!(
            "{:.2},{:.2},{:.2},{:.5},{:.5},{:.5},{:.5},{:.5},{:.5},{:.5},{:.5},{:.5},{},{},{},{:.5}",
            1.0,
            rpm,
            123.45,
            0.1,
            1.2,
            -0.3,
            0.1,
            bank,
            PI / 2.0,
            0.01,
            0.02,
            0.03,
            aircraft,
            0.5,
            gear,
            250.0
        )
        .into_bytes()
    }

    fn view(decoder: &DcsDecoder) -> Telemetry {
        let mut t = Telemetry::off();
        clear_listener_channels(&mut t);
        decoder.write(&mut t);
        t
    }

    #[test]
    fn native_getters_preserve_text_float_and_caught_integer_answers() {
        use crate::derive::{Listener, SourceListener};
        use crate::udp_flight::UdpConfig;
        use openrigcue_model::native_value::LegacyValue::{Float, Int, Str};

        let mut decoder = DcsDecoder::default();
        decoder.decode(&frame("SpitfireLFMkIX", 87.65, 0.39, 0.2));
        let mut listener = SourceListener::new(DcsSource::new(decoder, UdpConfig::port(0)));
        listener.receive();
        for (name, expected) in [
            ("rpm", Str("8700".into())),
            ("max_rpm", Str("10000".into())),
            ("gear", Str("0".into())),
            ("speed", Str("123".into())),
            ("pitch", Float(rad_to_deg(0.1))),
            ("roll", Float(fold_roll(rad_to_deg(0.2)))),
            ("pitch_rate", Float(rad_to_deg(0.01))),
            ("roll_rate", Float(rad_to_deg(0.02))),
            ("yaw_rate", Float(rad_to_deg(0.03))),
            ("canopy", Float(0.5)),
            ("landing_gear_left", Float(0.0)),
            ("landing_gear_any_suffix", Float(0.0)),
            ("car_name", Str("SpitfireLFMkIX".into())),
            ("cannon_firing", Float(250.0)),
            ("unknown", Int(0)),
        ] {
            assert_eq!(listener.native_channel(name), Some(expected), "{name}");
        }
    }

    #[test]
    fn native_invalid_fields_retain_each_getters_local_fallback_kind() {
        use openrigcue_model::native_value::LegacyValue::{Int, Str};

        for packet in [b"".as_slice(), b"1,NaN,inf", b"1,abc,-inf"] {
            let mut decoder = DcsDecoder::default();
            decoder.decode(packet);
            for (name, expected) in [
                ("rpm", Int(0)),
                ("speed", Str("0".into())),
                ("pitch", Int(0)),
                ("pitch_rate", Int(0)),
                ("landing_gear_nose", Int(0)),
                ("car_name", Int(0)),
            ] {
                assert_eq!(decoder.native_channel(name), Some(expected), "{packet:?}: {name}");
                assert!(!decoder.channel_read_raised(name), "the getter catches this failure");
            }
        }
    }

    #[test]
    fn native_rpm_multiplies_the_integer_without_float_rounding() {
        use openrigcue_model::native_value::LegacyValue::Str;

        let mut decoder = DcsDecoder::default();
        for (rpm, expected) in [
            ("9007199254740994", "900719925474099400"),
            ("18446744073709551616", "1844674407370955161600"),
            ("-0.9", "0"),
            ("-87.65", "-8700"),
        ] {
            decoder.decode(format!("1,{rpm}").as_bytes());
            assert_eq!(decoder.native_channel("rpm"), Some(Str(expected.into())));
            assert!(!decoder.decode(&[0xff]));
            assert_eq!(decoder.native_channel("rpm"), Some(Str(expected.into())));
        }
    }

    #[test]
    fn get_channel_maps_the_export_fields() {
        let mut d = DcsDecoder::default();
        assert!(d.decode(&frame("F-5E-3", 87.65, 1.0, 0.2)));
        let t = view(&d);
        assert_eq!(t.rpm, 8700.0, "int(rpm) * 100");
        assert_eq!(t.speed, 123.0, "int(true airspeed)");
        assert_eq!(t.longitudinal_acceleration, 0.1 * 10.0);
        assert_eq!(t.vertical_acceleration, 1.2);
        assert_eq!(t.lateral_acceleration, -0.3 * 100.0);
        assert_eq!(t.pitch, rad_to_deg(0.1));
        assert_eq!(t.yaw, rad_to_deg(parse_float("1.57080").unwrap()), "the hook sends %.5f");
        assert_eq!(t.roll, rad_to_deg(0.2));
        assert_eq!(t.max_rpm, 10000.0);
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.canopy, 0.5);
        assert_eq!(t.landing_gear_nose, 1.0);
        assert_eq!(t.vehicle, "F-5E-3");
        let rates = d.rates();
        assert_eq!((rates.pitch, rates.roll, rates.yaw), (rad_to_deg(0.01), rad_to_deg(0.02), rad_to_deg(0.03)));
    }

    #[test]
    fn get_channel_folds_bank_beyond_90_degrees() {
        let mut d = DcsDecoder::default();
        d.decode(&frame("A-10C", 0.0, 0.0, 2.0));
        assert_eq!(view(&d).roll, 180.0 - rad_to_deg(2.0));
        d.decode(&frame("A-10C", 0.0, 0.0, -2.0));
        assert_eq!(view(&d).roll, -180.0 - rad_to_deg(-2.0));
    }

    #[test]
    fn get_channel_spitfire_gear_threshold() {
        let mut d = DcsDecoder::default();
        d.decode(&frame("SpitfireLFMkIX", 0.0, 0.39, 0.0));
        assert_eq!(view(&d).landing_gear_left, 0.0);
        d.decode(&frame("SpitfireLFMkIX", 0.0, 0.4, 0.0));
        assert_eq!(view(&d).landing_gear_left, 0.4);
        d.decode(&frame("P-51D", 0.0, 0.39, 0.0));
        assert_eq!(view(&d).landing_gear_right, 0.39);
    }

    #[test]
    fn get_channel_answers_zero_for_missing_or_bad_fields() {
        let mut d = DcsDecoder::default();
        assert!(d.decode(b"1.00,abc"));
        let t = view(&d);
        assert_eq!((t.rpm, t.speed, t.pitch, t.landing_gear_left), (0.0, 0.0, 0.0, 0.0));
        assert_eq!(t.max_rpm, 10000.0);
        assert!(t.vehicle.is_empty());
    }

    #[test]
    fn receive_data_keeps_previous_fields_on_invalid_utf8() {
        let mut d = DcsDecoder::default();
        d.decode(&frame("A-10C", 50.0, 0.0, 0.0));
        assert!(!d.decode(&[0xff, 0xfe]));
        assert_eq!(view(&d).rpm, 5000.0);
    }
}

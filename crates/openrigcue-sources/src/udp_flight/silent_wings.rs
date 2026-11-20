//! Silent Wings: the simulator's binary UDP output (`output_udp_protocol = binary`).

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::channels::{assign, AngularRates};
use super::transport::{Decoder, PolledUdpSource, RateDecoder};

/// The Silent Wings listener (`silentwings`, port 6060).
pub type SilentWingsSource = PolledUdpSource<SilentWingsDecoder>;

/// Size of one datagram: a `u32`, two doubles and 28 floats, little-endian, no padding.
pub const PACKET_SIZE: usize = 132;

/// One Silent Wings datagram.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SilentWingsPacket {
    pub timestamp: u32,
    pub position_latitude: f64,
    pub position_longitude: f64,
    pub altitude_msl: f32,
    pub altitude_ground: f32,
    pub altitude_ground_45: f32,
    pub altitude_ground_forward: f32,
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub d_roll: f32,
    pub d_pitch: f32,
    pub d_yaw: f32,
    pub vx: f32,
    pub vy: f32,
    pub vz: f32,
    pub vx_wind: f32,
    pub vy_wind: f32,
    pub vz_wind: f32,
    pub v_eas: f32,
    pub ax: f32,
    pub ay: f32,
    pub az: f32,
    pub angle_of_attack: f32,
    pub angle_sideslip: f32,
    pub vario: f32,
    pub heading: f32,
    pub rate_of_turn: f32,
    pub airpressure: f32,
    pub density: f32,
    pub temperature: f32,
}

impl SilentWingsPacket {
    /// Decodes a datagram of exactly [`PACKET_SIZE`] bytes (any other length is rejected).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != PACKET_SIZE {
            return None;
        }
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        let f64_at = |at: usize| f64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let floats: [f32; 28] = std::array::from_fn(|i| {
            let at = 20 + 4 * i;
            f32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
        });
        let [altitude_msl, altitude_ground, altitude_ground_45, altitude_ground_forward, roll, pitch, yaw, d_roll, d_pitch, d_yaw, vx, vy, vz, vx_wind, vy_wind, vz_wind, v_eas, ax, ay, az, angle_of_attack, angle_sideslip, vario, heading, rate_of_turn, airpressure, density, temperature] =
            floats;
        Some(SilentWingsPacket {
            timestamp: u32_at(0),
            position_latitude: f64_at(4),
            position_longitude: f64_at(12),
            altitude_msl,
            altitude_ground,
            altitude_ground_45,
            altitude_ground_forward,
            roll,
            pitch,
            yaw,
            d_roll,
            d_pitch,
            d_yaw,
            vx,
            vy,
            vz,
            vx_wind,
            vy_wind,
            vz_wind,
            v_eas,
            ax,
            ay,
            az,
            angle_of_attack,
            angle_sideslip,
            vario,
            heading,
            rate_of_turn,
            airpressure,
            density,
            temperature,
        })
    }
}

/// The last packet taken in (`None` until the first valid datagram).
#[derive(Debug, Default)]
pub struct SilentWingsDecoder {
    packet: Option<SilentWingsPacket>,
}

impl Decoder for SilentWingsDecoder {
    const ID: &'static str = "silentwings";
    const DEFAULT_PORT: u16 = 6060;
    const TIMEOUT: Duration = Duration::from_millis(125);
    const MAX_DATAGRAM: usize = 1024;

    /// The receive step. The packet has no airspeed field, so
    /// `speed` and `steering_wheel_angle` are never set (see [`Decoder::write`]) and the
    /// step answers false for every datagram.
    fn decode(&mut self, datagram: &[u8]) -> bool {
        if let Some(packet) = SilentWingsPacket::parse(datagram) {
            self.packet = Some(packet);
        }
        false
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = self.packet.and_then(|p| match name {
            "yaw" => Some(p.yaw),
            "roll" => Some(p.roll),
            "pitch" => Some(p.pitch),
            "yaw_rate" => Some(p.d_yaw),
            "roll_rate" => Some(p.d_roll),
            "pitch_rate" => Some(p.d_pitch),
            "vertical_acceleration" => Some(p.az),
            "lateral_acceleration" => Some(p.ay),
            "longitudinal_acceleration" => Some(p.ax),
            "lateral_velocity" => Some(p.vy),
            "longitudinal_velocity" => Some(p.vx),
            "vertical_velocity" => Some(p.vz),
            _ => None,
        });
        Some(value.map_or(LegacyValue::Int(0), |v| LegacyValue::Float(f64::from(v))))
    }

    /// Numeric projection of the stored channels: the twelve channels stored before the step fails; speed
    /// and steering angle stay 0. Degrees and degrees per second as sent.
    fn write(&self, t: &mut Telemetry) {
        let Some(p) = self.packet else {
            return;
        };
        let f = |value: f32| Some(f64::from(value));
        assign(&mut t.yaw, f(p.yaw));
        assign(&mut t.roll, f(p.roll));
        assign(&mut t.pitch, f(p.pitch));
        assign(&mut t.vertical_acceleration, f(p.az));
        assign(&mut t.lateral_acceleration, f(p.ay));
        assign(&mut t.longitudinal_acceleration, f(p.ax));
        assign(&mut t.lateral_velocity, f(p.vy));
        assign(&mut t.longitudinal_velocity, f(p.vx));
        assign(&mut t.vertical_velocity, f(p.vz));
    }
}

impl RateDecoder for SilentWingsDecoder {
    /// The `pitch_rate` / `roll_rate` / `yaw_rate` channels.
    fn rates(&self) -> AngularRates {
        self.packet.map_or_else(AngularRates::default, |p| AngularRates {
            pitch: f64::from(p.d_pitch),
            roll: f64::from(p.d_roll),
            yaw: f64::from(p.d_yaw),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_flight::channels::clear_listener_channels;

    /// A packet in the datagram layout with float `i` set to `i + 0.5`.
    fn packet_bytes() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PACKET_SIZE);
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&47.5f64.to_le_bytes());
        bytes.extend_from_slice(&8.25f64.to_le_bytes());
        for i in 0..28 {
            bytes.extend_from_slice(&(i as f32 + 0.5).to_le_bytes());
        }
        bytes
    }

    #[test]
    fn silent_wings_data_packet_field_order() {
        let p = SilentWingsPacket::parse(&packet_bytes()).unwrap();
        assert_eq!((p.timestamp, p.position_latitude, p.position_longitude), (7, 47.5, 8.25));
        assert_eq!((p.altitude_msl, p.roll, p.pitch, p.yaw), (0.5, 4.5, 5.5, 6.5));
        assert_eq!((p.d_roll, p.d_pitch, p.d_yaw), (7.5, 8.5, 9.5));
        assert_eq!((p.vx, p.vy, p.vz, p.v_eas), (10.5, 11.5, 12.5, 16.5));
        assert_eq!((p.ax, p.ay, p.az), (17.5, 18.5, 19.5));
        assert_eq!((p.angle_sideslip, p.heading, p.airpressure, p.temperature), (21.5, 23.5, 25.5, 27.5));
    }

    #[test]
    fn silent_wings_data_packet_requires_the_exact_size() {
        let mut bytes = packet_bytes();
        bytes.push(0);
        assert_eq!(SilentWingsPacket::parse(&bytes), None);
        assert_eq!(SilentWingsPacket::parse(&bytes[..PACKET_SIZE - 1]), None);
    }

    #[test]
    fn receive_data_sets_twelve_channels_but_never_speed() {
        let mut d = SilentWingsDecoder::default();
        assert_eq!(d.native_channel("roll"), Some(LegacyValue::Int(0)));
        assert!(!d.decode(&packet_bytes()), "the airspeed AttributeError makes receiveData return False");
        let mut t = Telemetry::off();
        clear_listener_channels(&mut t);
        d.write(&mut t);
        assert_eq!((t.roll, t.pitch, t.yaw), (4.5, 5.5, 6.5));
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration, t.vertical_acceleration), (17.5, 18.5, 19.5));
        assert_eq!((t.longitudinal_velocity, t.lateral_velocity, t.vertical_velocity), (10.5, 11.5, 12.5));
        assert_eq!((t.speed, t.steering_wheel_angle), (0.0, 0.0));
        assert_eq!(d.rates(), AngularRates { pitch: 8.5, roll: 7.5, yaw: 9.5 });
        assert_eq!(d.native_channel("roll"), Some(LegacyValue::Float(4.5)));
        assert_eq!(d.native_channel("pitch_rate"), Some(LegacyValue::Float(8.5)));
        assert_eq!(d.native_channel("speed"), Some(LegacyValue::Int(0)));
        assert_eq!(d.native_channel("steering_wheel_angle"), Some(LegacyValue::Int(0)));
        assert!(!d.decode(&[0; PACKET_SIZE - 1]));
        assert_eq!(d.native_channel("roll"), Some(LegacyValue::Float(4.5)));
    }
}

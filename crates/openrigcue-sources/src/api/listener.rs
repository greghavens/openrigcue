//! The public telemetry API listener: third-party senders on UDP port 33001.

use std::net::Ipv4Addr;
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use super::packet::{BasicPacket, FanPacket, Header, MotionPacket};
use crate::udp_flight::python::{is_python_space, round_to, truncate};
use crate::udp_flight::transport::ProbeStage;
use crate::udp_flight::{assign, clear_listener_channels, Decoder, PolledUdpSource};

/// The API listener (`api`).
pub type ApiSource = PolledUdpSource<ApiDecoder>;

/// Version whose channel reads use `wind_direction`, falling back to `right_fan_power`.
const BASIC_VERSION: u32 = 101;
/// Digits `wind_direction` is rounded to where it stands in for a channel.
const WIND_DIRECTION_DIGITS: usize = 6;
/// `rumble_pitch_*` of a wheel on a rumble surface.
const RUMBLE_PITCH: f64 = 40.0;
/// `wheel_terrain` value of a rumble surface.
const RUMBLE_TERRAIN: u32 = 1;

/// The last packet taken in.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiPacket {
    /// Version 101.
    Basic(BasicPacket),
    /// Version 102 or 103.
    Motion(Box<MotionPacket>),
    /// `lla` tag: fan powers sent directly.
    Fans(FanPacket),
}

/// Last packet and the fan-override flag.
#[derive(Debug, Default)]
pub struct ApiDecoder {
    packet: Option<ApiPacket>,
    fan_override: bool,
}

impl ApiDecoder {
    /// Whether the last datagram was an `lla` packet whose fan powers drive the wind fans directly (published
    /// through `speed` = left fan power and, when the packet's version field reads 101, `lateral_acceleration` =
    /// right fan power).
    pub fn fan_override(&self) -> bool {
        self.fan_override
    }

    /// The last packet taken in.
    pub fn packet(&self) -> Option<&ApiPacket> {
        self.packet.as_ref()
    }

    /// Decodes an `api` / `srs` datagram by version; `Ok(None)` for an unknown version (nothing stored).
    fn parse_versioned(version: u32, datagram: &[u8]) -> Result<Option<ApiPacket>, ()> {
        let packet = match version {
            101 => BasicPacket::parse(datagram).map(ApiPacket::Basic),
            102 => MotionPacket::parse_102(datagram).map(|p| ApiPacket::Motion(Box::new(p))),
            103 => MotionPacket::parse_103(datagram).map(|p| ApiPacket::Motion(Box::new(p))),
            _ => return Ok(None),
        };
        packet.map(Some).ok_or(())
    }
}

impl Decoder for ApiDecoder {
    const ID: &'static str = "api";
    const DEFAULT_PORT: u16 = 33001;
    const BIND_IP: Ipv4Addr = Ipv4Addr::UNSPECIFIED;
    const TIMEOUT: Duration = Duration::from_secs(1);
    const MAX_DATAGRAM: usize = 1024;
    const FORWARDS: bool = false;

    fn on_probe_failure(&mut self, stage: ProbeStage, error: &std::io::Error) -> Option<String> {
        match stage {
            ProbeStage::Creation => {
                self.fan_override = false;
                tracing::debug!(target: "App.apiListener.checkStatus", "Socket Error: {error}");
            }
            ProbeStage::Bind => {
                tracing::debug!(target: "App.apiListener.checkStatus", "Bind Error: {error}");
                self.fan_override = false;
            }
            ProbeStage::Receive => self.fan_override = false,
        }
        None
    }
    /// Once a packet is stored, the `rpm`, `max_rpm`, `gear`, `speed` and `lateral_acceleration` reads fail on a
    /// non-finite number that cannot become an integer
    /// and (`lateral_acceleration`) on an `lla` packet of a version other than 101; the dispatcher then keeps the
    /// previous value (registry default `None`). Before the first packet
    /// every channel answers 0, so nothing is held then (see `write`).
    const HELD_CHANNELS: &'static [&'static str] = &["rpm", "max_rpm", "gear", "speed", "lateral_acceleration"];

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        if matches!(name, "game" | "car_name" | "track_loc") {
            let Some(ApiPacket::Motion(packet)) = &self.packet else { return Some(LegacyValue::Int(0)) };
            return Some(if name == "track_loc" {
                LegacyValue::Bytes(packet.location.to_vec())
            } else {
                game_name(&packet.game).map_or(LegacyValue::Int(0), LegacyValue::Str)
            });
        }
        if !Telemetry::CHANNELS.contains(&name) && !matches!(name, "yaw_rate" | "pitch_rate" | "roll_rate") {
            return None;
        }
        let Some(packet) = &self.packet else { return Some(LegacyValue::Int(0)) };
        let integer_text = |value| crate::udp_racing::int_text(value).map(LegacyValue::Str);
        match name {
            "rpm" => return integer_text(packet.rpm()?),
            "max_rpm" => return integer_text(packet.max_rpm()?),
            "gear" => return Some(LegacyValue::Str(packet.gear().to_string())),
            "speed" => return integer_text(packet.speed()?),
            "lateral_acceleration" => return Some(LegacyValue::Float(packet.lateral_acceleration()?)),
            "steering_wheel_angle" => {
                return match packet {
                    ApiPacket::Fans(packet) if packet.version == 103 => Some(LegacyValue::Int(0)),
                    ApiPacket::Fans(packet) if packet.version != BASIC_VERSION => None,
                    _ => Some(LegacyValue::Float(packet.steering_wheel_angle())),
                }
            }
            _ => {}
        }
        let ApiPacket::Motion(packet) = packet else { return Some(LegacyValue::Int(0)) };
        Some(match name {
            "pitch" => LegacyValue::Float(f64::from(packet.pitch)),
            "roll" => LegacyValue::Float(f64::from(packet.roll)),
            "yaw" => LegacyValue::Float(f64::from(packet.yaw)),
            "vertical_acceleration" => LegacyValue::Float(f64::from(packet.vertical_acceleration)),
            "longitudinal_acceleration" => LegacyValue::Float(f64::from(packet.longitudinal_acceleration)),
            "lateral_velocity" => LegacyValue::Float(f64::from(packet.lateral_velocity)),
            "longitudinal_velocity" => {
                packet.longitudinal_velocity.map_or(LegacyValue::Int(0), |value| LegacyValue::Float(f64::from(value)))
            }
            "suspension_travel_lf" => LegacyValue::Float(f64::from(packet.suspension_travel[0])),
            "suspension_travel_rf" => LegacyValue::Float(f64::from(packet.suspension_travel[1])),
            "suspension_travel_lr" => LegacyValue::Float(f64::from(packet.suspension_travel[2])),
            "suspension_travel_rr" => LegacyValue::Float(f64::from(packet.suspension_travel[3])),
            "rumble_pitch_lf" => LegacyValue::Int(if packet.wheel_terrain[0] == RUMBLE_TERRAIN { 40 } else { 0 }),
            "rumble_pitch_rf" => LegacyValue::Int(if packet.wheel_terrain[1] == RUMBLE_TERRAIN { 40 } else { 0 }),
            "rumble_pitch_lr" => LegacyValue::Int(if packet.wheel_terrain[2] == RUMBLE_TERRAIN { 40 } else { 0 }),
            "rumble_pitch_rr" => LegacyValue::Int(if packet.wheel_terrain[3] == RUMBLE_TERRAIN { 40 } else { 0 }),
            _ => LegacyValue::Int(0),
        })
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        let Some(packet) = &self.packet else { return false };
        match name {
            "rpm" => packet.rpm().is_none(),
            "max_rpm" => packet.max_rpm().is_none(),
            "speed" => packet.speed().is_none(),
            "lateral_acceleration" => packet.lateral_acceleration().is_none(),
            "steering_wheel_angle" => {
                matches!(packet, ApiPacket::Fans(packet) if !matches!(packet.version, BASIC_VERSION | 103))
            }
            _ => false,
        }
    }

    /// Takes in the newest datagram.
    fn decode(&mut self, datagram: &[u8]) -> bool {
        let Some(header) = Header::parse(datagram) else {
            tracing::debug!("Header Error: datagram shorter than the header");
            self.fan_override = false;
            return false;
        };
        match std::str::from_utf8(&header.tag) {
            Ok("api" | "srs") => match Self::parse_versioned(header.version, datagram) {
                Ok(packet) => {
                    let stored = packet.is_some();
                    if let Some(packet) = packet {
                        self.packet = Some(packet);
                    }
                    self.fan_override = false;
                    stored
                }
                Err(()) => {
                    tracing::debug!(version = header.version, len = datagram.len(), "Failed to parse packet");
                    self.fan_override = false;
                    false
                }
            },
            Ok("lla") => match FanPacket::parse(datagram) {
                Some(packet) => {
                    self.packet = Some(ApiPacket::Fans(packet));
                    self.fan_override = true;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// A failed liveness test (bind error or nothing within 1 s) clears the fan override.
    fn on_silence(&mut self) {
        self.fan_override = false;
    }

    /// The `speed` read: the packet's speed as an integer, falling back to the left fan power as an integer.
    /// A non-finite speed (an `api`/`srs` packet has no
    /// left fan power) or a non-finite `lla` fan power makes the read fail.
    fn speed_raises(&self) -> bool {
        self.packet.as_ref().is_some_and(|packet| packet.speed().is_none())
    }

    /// Writes every channel. Before the first packet each channel read answers 0
    /// without failing, so every channel, held ones included, reads 0 and vehicle and location are cleared.
    fn write(&self, t: &mut Telemetry) {
        let Some(packet) = &self.packet else {
            clear_listener_channels(t);
            return;
        };
        assign(&mut t.rpm, packet.rpm());
        assign(&mut t.max_rpm, packet.max_rpm());
        t.gear = f64::from(packet.gear());
        assign(&mut t.speed, packet.speed());
        assign(&mut t.lateral_acceleration, packet.lateral_acceleration());
        t.steering_wheel_angle = packet.steering_wheel_angle();
        if let ApiPacket::Motion(m) = packet {
            t.pitch = f64::from(m.pitch);
            t.roll = f64::from(m.roll);
            t.yaw = f64::from(m.yaw);
            t.vertical_acceleration = f64::from(m.vertical_acceleration);
            t.longitudinal_acceleration = f64::from(m.longitudinal_acceleration);
            t.lateral_velocity = f64::from(m.lateral_velocity);
            assign(&mut t.longitudinal_velocity, m.longitudinal_velocity.map(f64::from));
            let [lf, rf, lr, rr] = m.suspension_travel.map(f64::from);
            (t.suspension_travel_lf, t.suspension_travel_rf, t.suspension_travel_lr, t.suspension_travel_rr) =
                (lf, rf, lr, rr);
            let [lf, rf, lr, rr] = m.wheel_terrain.map(rumble_pitch);
            (t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr) = (lf, rf, lr, rr);
            if let Some(game) = game_name(&m.game) {
                t.vehicle = game;
            }
            t.location = python_bytes_str(&m.location);
        }
    }
}

fn rumble_pitch(terrain: u32) -> f64 {
    if terrain == RUMBLE_TERRAIN {
        RUMBLE_PITCH
    } else {
        0.0
    }
}

/// The vehicle name (`car_name`): the game field decoded as UTF-8, stripped of surrounding whitespace and of
/// trailing NULs. `None` for invalid UTF-8 (no vehicle).
fn game_name(field: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(field).ok()?;
    Some(text.trim_matches(is_python_space).trim_end_matches('\0').to_owned())
}

/// The `track_loc` channel is the raw 50-byte location field (NUL padding
/// included, not decoded); `Telemetry::location` holds
/// its printed byte-string form, the text every reader of `location` gets
/// (for example `b'Spa\x00\x00…'`).
fn python_bytes_str(field: &[u8]) -> String {
    let quote = if field.contains(&b'\'') && !field.contains(&b'"') { '"' } else { '\'' };
    let mut text = String::with_capacity(field.len() + 3);
    text.push('b');
    text.push(quote);
    for &byte in field {
        match byte {
            b'\\' => text.push_str("\\\\"),
            b'\t' => text.push_str("\\t"),
            b'\n' => text.push_str("\\n"),
            b'\r' => text.push_str("\\r"),
            _ if char::from(byte) == quote => {
                text.push('\\');
                text.push(quote);
            }
            0x20..=0x7e => text.push(char::from(byte)),
            _ => text.push_str(&format!("\\x{byte:02x}")),
        }
    }
    text.push(quote);
    text
}

/// `x` truncated to an integer; `None` where it has no integer value (NaN, infinity).
fn whole(value: f32) -> Option<f64> {
    truncate(f64::from(value))
}

impl ApiPacket {
    /// The `rpm` read: rpm as an integer; `None` where that fails.
    fn rpm(&self) -> Option<f64> {
        whole(match self {
            ApiPacket::Basic(p) => p.rpm,
            ApiPacket::Motion(p) => p.rpm,
            ApiPacket::Fans(p) => p.rpm,
        })
    }

    /// The `max_rpm` read: maximum rpm as an integer; `None` where that fails.
    fn max_rpm(&self) -> Option<f64> {
        whole(match self {
            ApiPacket::Basic(p) => p.max_rpm,
            ApiPacket::Motion(p) => p.max_rpm,
            ApiPacket::Fans(p) => p.max_rpm,
        })
    }

    /// The `gear` read.
    fn gear(&self) -> i32 {
        match self {
            ApiPacket::Basic(p) => p.gear,
            ApiPacket::Motion(p) => p.gear,
            ApiPacket::Fans(p) => p.gear,
        }
    }

    /// The `speed` read: speed as an integer, falling back to
    /// the left fan power, which only an `lla` packet has; `None` where both fail.
    fn speed(&self) -> Option<f64> {
        match self {
            ApiPacket::Basic(p) => whole(p.speed),
            ApiPacket::Motion(p) => whole(p.speed),
            ApiPacket::Fans(p) => whole(p.left_fan_power),
        }
    }

    /// The `lateral_acceleration` read. Version 101: wind direction
    /// rounded to 6 digits; 102/103: lateral acceleration; `lla`: right fan power when its version field reads
    /// 101 (the missing wind direction falls back to it), otherwise `None` (the packet has no lateral
    /// acceleration). Only the version decides, not the packet tag.
    fn lateral_acceleration(&self) -> Option<f64> {
        match self {
            ApiPacket::Basic(p) => Some(round_to(f64::from(p.wind_direction), WIND_DIRECTION_DIGITS)),
            ApiPacket::Motion(p) => Some(f64::from(p.lateral_acceleration)),
            ApiPacket::Fans(p) => (p.version == BASIC_VERSION).then(|| f64::from(p.right_fan_power)),
        }
    }

    /// The `steering_wheel_angle` read. Version 101: wind direction
    /// rounded to 6 digits; 103: steering angle; 102: lateral acceleration; `lla`: right fan
    /// power when its version field reads 101 (the wind direction fallback), otherwise 0 (the read fails and
    /// the default 0 is written).
    fn steering_wheel_angle(&self) -> f64 {
        match self {
            ApiPacket::Basic(p) => round_to(f64::from(p.wind_direction), WIND_DIRECTION_DIGITS),
            ApiPacket::Motion(p) => f64::from(p.steering_wheel_angle.unwrap_or(p.lateral_acceleration)),
            ApiPacket::Fans(p) if p.version == BASIC_VERSION => f64::from(p.right_fan_power),
            ApiPacket::Fans(_) => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::packet::build::{basic, header, motion, sample};
    use crate::udp_flight::transport::publish;
    use crate::Source;

    /// One dispatcher pass over `previous`.
    fn publish_over(decoder: &ApiDecoder, mut previous: Telemetry) -> Telemetry {
        publish(decoder, &mut previous);
        previous
    }

    fn view(decoder: &ApiDecoder) -> Telemetry {
        publish_over(decoder, Telemetry::off())
    }

    /// Telemetry as another tick left it: the API's held channels and two that are always rewritten.
    fn earlier_tick() -> Telemetry {
        let mut t = Telemetry::off();
        (t.rpm, t.max_rpm, t.gear, t.speed, t.lateral_acceleration) = (3000.0, 8000.0, 4.0, 50.0, 1.5);
        (t.steering_wheel_angle, t.pitch) = (7.0, 2.0);
        t
    }

    #[test]
    fn native_packet_getters_cross_the_polled_source_adapter_with_their_types() {
        use crate::derive::{Listener, SourceListener};
        use crate::udp_flight::UdpConfig;
        use openrigcue_model::native_value::LegacyValue::{Float, Int, Str};
        for (packet, expected) in [
            (
                basic(b"api", 101, [12.9, 45.0, 3000.7, 7000.2], 4),
                vec![
                    ("rpm", Str("3000".into())),
                    ("max_rpm", Str("7000".into())),
                    ("gear", Str("4".into())),
                    ("speed", Str("12".into())),
                    ("lateral_acceleration", Float(45.0)),
                    ("pitch", Int(0)),
                    ("raw_flag", Int(0)),
                ],
            ),
            (
                motion(b"api", 102, "G", "T", &sample(false)),
                vec![
                    ("rpm", Str("6543".into())),
                    ("gear", Str("-1".into())),
                    ("speed", Str("27".into())),
                    ("pitch", Float(1.5)),
                    ("rumble_pitch_lf", Int(40)),
                    ("rumble_pitch_rf", Int(0)),
                    ("longitudinal_velocity", Int(0)),
                    ("steering_wheel_angle", Float(0.5)),
                ],
            ),
            (
                motion(b"api", 103, "G", "T", &sample(true)),
                vec![("longitudinal_velocity", Float(26.0)), ("steering_wheel_angle", Float(45.0))],
            ),
        ] {
            let mut decoder = ApiDecoder::default();
            assert!(decoder.decode(&packet));
            // The decoded packet supplies the poll; no socket probe is invoked.
            let source = ApiSource::new(decoder, UdpConfig::port(0));
            let mut listener = SourceListener::new(source);
            listener.receive();
            for (name, value) in expected {
                assert_eq!(listener.native_channel(name), Some(value), "{name}");
            }
        }
    }

    #[test]
    fn native_failed_reads_are_reported_through_the_polled_source() {
        use crate::udp_flight::UdpConfig;
        let mut decoder = ApiDecoder::default();
        assert!(decoder.decode(&basic(b"api", 101, [12.0, 1.0, f32::NAN, f32::INFINITY], 4)));
        let mut source = ApiSource::new(decoder, UdpConfig::port(0));
        let mut telemetry = earlier_tick();
        source.poll(&mut telemetry).unwrap();
        assert_eq!((telemetry.rpm, telemetry.max_rpm), (3000.0, 8000.0));
        assert!(source.channel_read_raised("rpm"));
        assert!(source.channel_read_raised("max_rpm"));
        assert_eq!(source.native_channel("rpm"), None);
        assert_eq!(source.native_channel("max_rpm"), None);
        assert!(!source.channel_read_raised("speed"));
    }

    #[test]
    fn native_empty_and_lla_version_branches_keep_their_lookup_and_catch_kinds() {
        use crate::udp_flight::UdpConfig;
        let empty = ApiSource::new(ApiDecoder::default(), UdpConfig::port(0));
        for name in Telemetry::CHANNELS {
            assert_eq!(empty.native_channel(name), Some(LegacyValue::Int(0)), "{name}");
            assert!(!empty.channel_read_raised(name));
        }
        for version in [101, 103, 1] {
            let mut decoder = ApiDecoder::default();
            assert!(decoder.decode(&basic(b"lla", version, [60.7, 30.5, 900.0, 1000.0], 2)));
            let source = ApiSource::new(decoder, UdpConfig::port(0));
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("60".into())));
            assert_eq!(source.channel_read_raised("lateral_acceleration"), version != 101);
            assert_eq!(source.channel_read_raised("steering_wheel_angle"), version == 1);
            let expected = match version {
                101 => Some(LegacyValue::Float(30.5)),
                103 => Some(LegacyValue::Int(0)),
                _ => None,
            };
            assert_eq!(source.native_channel("steering_wheel_angle"), expected);
        }
    }

    #[test]
    fn get_channel_before_any_packet_answers_zero_for_every_channel() {
        let mut earlier = earlier_tick();
        (earlier.vehicle, earlier.location) = ("Car".into(), "Track".into());
        let t = publish_over(&ApiDecoder::default(), earlier);
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed, t.lateral_acceleration), (0.0, 0.0, 0.0, 0.0, 0.0));
        assert_eq!((t.steering_wheel_angle, t.pitch), (0.0, 0.0));
        assert!(t.vehicle.is_empty() && t.location.is_empty());
    }

    #[test]
    fn get_channel_after_a_datagram_that_stores_nothing_answers_zero() {
        let mut d = ApiDecoder::default();
        assert!(!d.decode(&basic(b"xyz", 101, [12.0, 1.0, 3000.0, 7000.0], 4)), "unknown tag stores nothing");
        assert!(!d.decode(&basic(b"api", 999, [12.0, 1.0, 3000.0, 7000.0], 4)), "unknown version stores nothing");
        let t = publish_over(&d, Telemetry::off());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed, t.lateral_acceleration), (0.0, 0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn receive_data_101_wind_direction_feeds_lateral_and_steering() {
        let mut d = ApiDecoder::default();
        assert!(d.decode(&basic(b"srs", 101, [12.9, 45.123_455, 3000.7, 7000.2], 4)));
        let t = view(&d);
        assert_eq!((t.speed, t.rpm, t.max_rpm, t.gear), (12.0, 3000.0, 7000.0, 4.0));
        assert_eq!(t.lateral_acceleration, round_to(f64::from(45.123_455_f32), 6));
        assert_eq!(t.steering_wheel_angle, t.lateral_acceleration);
        assert_eq!(t.pitch, 0.0);
        assert!(t.vehicle.is_empty());
        assert!(!d.fan_override());
    }

    #[test]
    fn location_is_the_str_of_the_raw_bytes() {
        // The location is a byte string, printed as b'...' with the quote chosen by its content
        assert_eq!(python_bytes_str(b"Spa"), "b'Spa'");
        assert_eq!(python_bytes_str(b"it's"), "b\"it's\"");
        assert_eq!(python_bytes_str(b"'\""), "b'\\'\"'");
        assert_eq!(python_bytes_str(b"a\\b\t\n\r\x7f\xc3\xa9"), "b'a\\\\b\\t\\n\\r\\x7f\\xc3\\xa9'");
        assert_eq!(python_bytes_str(&[0, 0]), "b'\\x00\\x00'");
    }

    #[test]
    fn receive_data_102_and_103() {
        let mut d = ApiDecoder::default();
        let p = sample(false);
        assert!(d.decode(&motion(b"api", 102, "  My Game", "Spa", &p)));
        let t = view(&d);
        assert_eq!((t.speed, t.rpm, t.max_rpm, t.gear), (27.0, 6543.0, 8000.0, -1.0));
        assert_eq!((t.pitch, t.roll, t.yaw), (1.5, -2.5, 3.5));
        assert_eq!(t.steering_wheel_angle, 0.5, "102 falls back to lateral acceleration");
        assert_eq!(t.longitudinal_velocity, 0.0);
        assert_eq!((t.suspension_travel_lf, t.suspension_travel_rf), (f64::from(0.1f32), f64::from(0.2f32)));
        assert_eq!((t.suspension_travel_lr, t.suspension_travel_rr), (f64::from(0.3f32), f64::from(0.4f32)));
        assert_eq!(
            (t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr),
            (40.0, 0.0, 0.0, 40.0)
        );
        assert_eq!(t.vehicle, "My Game", "car_name is the game field");
        assert_eq!(t.location, format!("b'Spa{}'", "\\x00".repeat(47)), "str() of the raw 50-byte field");
        assert_eq!(d.native_channel("game"), Some(LegacyValue::Str("My Game".into())));
        assert_eq!(d.native_channel("car_name"), Some(LegacyValue::Str("My Game".into())));
        let mut raw_location = vec![0; crate::api::packet::TEXT_LEN];
        raw_location[..3].copy_from_slice(b"Spa");
        assert_eq!(d.native_channel("track_loc"), Some(LegacyValue::Bytes(raw_location)));
        assert!(d.decode(&motion(b"api", 103, "G", "", &sample(true))));
        let t = view(&d);
        assert_eq!((t.steering_wheel_angle, t.longitudinal_velocity), (45.0, 26.0));
    }

    #[test]
    fn receive_data_lla_sets_the_fan_override() {
        let mut d = ApiDecoder::default();
        assert!(d.decode(&basic(b"lla", 1, [60.7, 30.5, 900.0, 1000.0], 2)));
        assert!(d.fan_override());
        assert!(d.decode(&basic(b"api", 101, [0.0; 4], 0)));
        assert!(!d.fan_override(), "any api packet clears the override");
    }

    #[test]
    fn get_channel_lla_fan_powers_depend_on_the_version_field() {
        let mut d = ApiDecoder::default();
        d.decode(&basic(b"lla", 101, [60.7, 30.5, 900.0, 1000.0], 2));
        let t = publish_over(&d, earlier_tick());
        assert_eq!(
            (t.speed, t.rpm, t.max_rpm, t.gear),
            (60.0, 900.0, 1000.0, 2.0),
            "speed falls back to left_fan_power"
        );
        assert_eq!(
            (t.lateral_acceleration, t.steering_wheel_angle),
            (30.5, 30.5),
            "wind_direction falls back to right_fan_power"
        );
        d.decode(&basic(b"lla", 103, [60.7, 30.5, 900.0, 1000.0], 2));
        let t = publish_over(&d, earlier_tick());
        assert_eq!(t.lateral_acceleration, 1.5, "api.lateral_acceleration raises: previous value kept");
        assert_eq!(t.steering_wheel_angle, 0.0, "version 103: except: return 0");
        d.decode(&basic(b"lla", 1, [60.7, 30.5, 900.0, 1000.0], 2));
        let t = publish_over(&d, earlier_tick());
        assert_eq!(t.lateral_acceleration, 1.5, "raises: previous value kept");
        assert_eq!(t.steering_wheel_angle, 0.0, "raises: registry default 0");
        assert_eq!(t.speed, 60.0);
    }

    #[test]
    fn receive_data_errors() {
        let mut d = ApiDecoder::default();
        d.decode(&basic(b"lla", 1, [1.0; 4], 0));
        assert!(!d.decode(&header(b"api", 101)), "wrong length");
        assert!(!d.fan_override(), "Failed to parse packet clears the override");
        d.decode(&basic(b"lla", 1, [1.0; 4], 0));
        assert!(!d.decode(b"ap"), "Header Error");
        assert!(!d.fan_override());
        d.decode(&basic(b"lla", 1, [5.0; 4], 0));
        assert!(!d.decode(&basic(b"xyz", 101, [9.0; 4], 0)), "unknown tag");
        assert!(d.fan_override(), "an unknown tag changes nothing");
        assert!(!d.decode(&basic(b"api", 104, [9.0; 4], 0)), "unknown version stores nothing");
        assert!(!d.fan_override());
        assert_eq!(view(&d).rpm, 5.0, "the previous packet stays");
    }

    #[test]
    fn receive_data_tag_that_is_not_utf8_changes_nothing() {
        let mut d = ApiDecoder::default();
        d.decode(&basic(b"lla", 1, [5.0; 4], 0));
        let stored = d.packet().cloned();
        assert!(!d.decode(&basic(b"\xff\xfe\xfd", 101, [9.0; 4], 0)), "decode('utf-8') raises inside the bare try");
        assert!(d.fan_override(), "lla_switch untouched");
        assert_eq!(d.packet().cloned(), stored);
    }

    #[test]
    fn get_channel_int_of_non_finite_keeps_the_previous_value() {
        let mut d = ApiDecoder::default();
        d.decode(&basic(b"api", 101, [f32::NAN, 1.0, f32::INFINITY, f32::NAN], 3));
        let t = publish_over(&d, earlier_tick());
        assert_eq!((t.speed, t.rpm, t.max_rpm), (50.0, 3000.0, 8000.0), "int() raises; no left_fan_power fallback");
        assert_eq!(t.gear, 3.0);
        d.decode(&basic(b"lla", 101, [f32::INFINITY, 1.0, 1.0, 1.0], 3));
        assert_eq!(publish_over(&d, earlier_tick()).speed, 50.0, "int(left_fan_power) raises too");
    }

    #[test]
    fn game_name_strips_whitespace_then_trailing_nuls() {
        let mut field = [0u8; 50];
        field[..9].copy_from_slice(b" Game  \t ");
        assert_eq!(game_name(&field).unwrap(), " Game  \t ".trim_start(), "trailing NULs protect trailing spaces");
        assert_eq!(game_name(&[0xff; 50]), None);
    }

    #[test]
    fn check_status_failure_clears_the_fan_override() {
        let busy = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        let mut source =
            ApiSource::new(ApiDecoder { packet: None, fan_override: true }, crate::udp_flight::UdpConfig::port(port));
        assert!(!source.is_running().unwrap());
        assert!(!source.decoder().fan_override());
    }
}
